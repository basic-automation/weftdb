//! Idle connections kept per cached Turso database, so a transaction reuses one instead of
//! opening a new connection every time.
//!
//! Turso 0.8.1 in MVCC mode keeps about 600 bytes for every connection that ran a
//! transaction and was then dropped, and that memory survives even dropping the
//! `turso::Database`. WeftDB opened a fresh connection for every transaction (about five per
//! `capture_measurement`), so a capture-heavy process grew by gigabytes a day. A reused
//! connection does not leak: 100k single-row transactions on one connection stay flat at
//! 11 MB, against 212 MB at 400k on fresh ones (`examples/capture_rss.rs`).
//!
//! A pool exists only while something owns its database: the process's handle cache
//! (`CONNECTION_DATABASES`) for legacy databases, and each control-plane store
//! (`SegmentIndexStore`, `CatalogStore`, `AspectCatalog`, `AspectMetadataStore`) for its own
//! file. [`register`] runs when an owner takes the handle and [`unregister`] when it lets go;
//! registrations are counted, so the pool lives until the last owner leaves. Unregistering
//! drops the idle connections so they do not keep a closed database alive, and a connection
//! checked out before that is dropped on return rather than handed to the next opener.
//!
//! Each [`Role`] has its own pool, because a connection's busy timeout is its own: a
//! control-plane writer must see `Busy` at once while a reader waits it out, so the two never
//! share connections.

use std::{
	collections::HashMap,
	sync::{Arc, LazyLock, Mutex, PoisonError},
};

/// Idle connections kept per database by default. More concurrent transactions than this on
/// one file just open extra connections, which are dropped when they finish. Each idle
/// connection keeps its own page cache warm, so this trades memory for fewer reads.
/// `WEFT_POOL_MAX_IDLE` overrides it; `0` turns pooling off.
const DEFAULT_MAX_IDLE: usize = 4;

static MAX_IDLE: LazyLock<usize> = LazyLock::new(|| std::env::var("WEFT_POOL_MAX_IDLE").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_IDLE));

pub struct Pool {
	idle: Mutex<Vec<turso::Connection>>,
	/// Cleared by [`unregister`]; a returned connection then goes away instead.
	open: std::sync::atomic::AtomicBool,
}

impl Pool {
	/// Take back `conn` once its holder is done with it. Only a connection with no open
	/// transaction is kept: one left mid-transaction (a caller that bailed between `BEGIN`
	/// and `COMMIT`) is dropped, and Turso rolls its transaction back.
	pub fn give_back(&self, conn: turso::Connection) {
		if !self.open.load(std::sync::atomic::Ordering::Acquire) || !matches!(conn.is_autocommit(), Ok(true)) {
			return;
		}
		let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
		if idle.len() < *MAX_IDLE {
			idle.push(conn);
		}
	}
}

/// What a pooled connection is for. Connections are only reused within one role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
	/// Legacy `BEGIN CONCURRENT` / `BEGIN IMMEDIATE` transactions (`begin_concurrent`).
	Legacy,
	/// Control-plane connections with no busy timeout (`control_plane::connect`).
	Writer,
	/// Control-plane read connections with a busy timeout (`control_plane::connect_reader`).
	Reader,
}

/// Each pool with how many owners registered it.
/// A pool by (database identity, role), with how many owners registered it.
type Registry = HashMap<(usize, Role), (usize, Arc<Pool>)>;

static POOLS: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// `turso::Database` is a single `Arc` to the shared database, and its clones share that
// pointer, which is what identifies the database here. If Turso ever adds a field, this
// stops compiling instead of reading the wrong word.
const _: () = assert!(std::mem::size_of::<turso::Database>() == std::mem::size_of::<usize>());

const fn identity(db: &turso::Database) -> usize {
	// SAFETY: `turso::Database` is exactly one `Arc` (size asserted above), whose
	// representation is a non-null pointer; this reads that pointer as an integer and never
	// dereferences it.
	unsafe { std::ptr::from_ref(db).cast::<usize>().read() }
}

/// Start pooling `role` connections to `db`. The owner keeps a clone of `db` until it calls
/// [`unregister`], so the identity cannot be reused while the pool exists.
pub fn register(db: &turso::Database, role: Role) {
	let mut pools = POOLS.lock().unwrap_or_else(PoisonError::into_inner);
	let entry = pools.entry((identity(db), role)).or_insert_with(|| (0, Arc::new(Pool { idle: Mutex::new(Vec::new()), open: std::sync::atomic::AtomicBool::new(true) })));
	entry.0 += 1;
	drop(pools);
}

/// Undo one [`register`] of `role` on `db`; the last one drops the pool and its idle
/// connections.
pub fn unregister(db: &turso::Database, role: Role) {
	let pool = {
		let mut pools = POOLS.lock().unwrap_or_else(PoisonError::into_inner);
		let key = (identity(db), role);
		match pools.get_mut(&key) {
			Some(entry) if entry.0 > 1 => {
				entry.0 -= 1;
				None
			}
			Some(_) => pools.remove(&key).map(|(_, pool)| pool),
			None => None,
		}
	};
	if let Some(pool) = pool {
		pool.open.store(false, std::sync::atomic::Ordering::Release);
		pool.idle.lock().unwrap_or_else(PoisonError::into_inner).clear();
	}
}

/// A `role` connection to `db`: an idle pooled one when there is one, otherwise a new one.
/// The pool comes back with it when `db` is pooled for `role`, so the connection can be
/// returned to it.
pub fn checkout(db: &turso::Database, role: Role) -> turso::Result<(turso::Connection, Option<Arc<Pool>>)> {
	let pool = POOLS.lock().unwrap_or_else(PoisonError::into_inner).get(&(identity(db), role)).map(|(_, pool)| pool.clone());
	let Some(pool) = pool else { return Ok((db.connect()?, None)) };
	let idle = pool.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
	let conn = match idle {
		Some(conn) => conn,
		None => db.connect()?,
	};
	Ok((conn, Some(pool)))
}

/// How many idle `role` connections `db`'s pool holds, or `None` when it is not pooled.
#[cfg(test)]
pub fn idle_count(db: &turso::Database, role: Role) -> Option<usize> {
	POOLS.lock().unwrap_or_else(PoisonError::into_inner).get(&(identity(db), role)).map(|(_, p)| p.idle.lock().unwrap_or_else(PoisonError::into_inner).len())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{cache, database::traits::Connection as _, Database};

	#[tokio::test]
	async fn a_finished_transaction_returns_its_connection_and_an_open_one_does_not() {
		let dir = tempfile::tempdir().expect("a temp dir");
		let db = turso::Builder::new_local(dir.path().join("p.db").to_str().expect("utf-8")).build().await.expect("opens");
		db.connect().expect("connects").execute("PRAGMA journal_mode=experimental_mvcc", ()).await.ok();
		db.connect().expect("connects").execute("CREATE TABLE t (x INTEGER)", ()).await.expect("creates");
		assert_eq!(idle_count(&db, Role::Legacy), None, "an uncached database is not pooled");
		register(&db, Role::Legacy);

		let conn = Database::begin_concurrent(&db, "p", None).await.expect("begins");
		conn.as_ref().execute("INSERT INTO t VALUES (1)", ()).await.expect("inserts");
		Database::commit_concurrent(&conn).await.expect("commits");
		drop(conn);
		assert_eq!(idle_count(&db, Role::Legacy), Some(1), "a committed connection goes back");

		let conn = Database::begin_concurrent(&db, "p", None).await.expect("begins on the pooled connection");
		assert_eq!(idle_count(&db, Role::Legacy), Some(0), "the idle connection was reused");
		conn.as_ref().execute("INSERT INTO t VALUES (2)", ()).await.expect("inserts");
		drop(conn);
		assert_eq!(idle_count(&db, Role::Legacy), Some(0), "a connection left mid-transaction is not pooled");

		let conn: cache::Connection = Database::begin_concurrent(&db, "p", None).await.expect("begins after the abandoned one");
		Database::commit_concurrent(&conn).await.expect("commits");
		drop(conn);
		unregister(&db, Role::Legacy);
		assert_eq!(idle_count(&db, Role::Legacy), None, "unregistering drops the pool");
		drop(db);
	}

	#[tokio::test]
	async fn control_plane_roles_pool_apart_and_a_failed_connection_is_closed() {
		use crate::types::durable::control_plane;
		let dir = tempfile::tempdir().expect("a temp dir");
		let db = turso::Builder::new_local(dir.path().join("cp.db").to_str().expect("utf-8")).build().await.expect("opens");
		db.connect().expect("connects").execute("PRAGMA journal_mode=experimental_mvcc", ()).await.ok();
		db.connect().expect("connects").execute("CREATE TABLE t (x INTEGER)", ()).await.expect("creates");
		control_plane::adopt(&db);
		control_plane::adopt(&db);

		drop(control_plane::connect(&db).await.expect("a writer"));
		drop(control_plane::connect_reader(&db).await.expect("a reader"));
		assert_eq!((idle_count(&db, Role::Writer), idle_count(&db, Role::Reader)), (Some(1), Some(1)), "each role keeps its own");

		let writer = control_plane::connect(&db).await.expect("the pooled writer");
		writer.busy_timeout(std::time::Duration::from_secs(5)).expect("sets a timeout");
		drop(writer);
		let reader = control_plane::connect_reader(&db).await.expect("the pooled reader");
		let writer = control_plane::connect(&db).await.expect("the pooled writer again");
		assert_eq!(writer.busy_timeout(std::time::Duration::ZERO).map(|()| idle_count(&db, Role::Writer)).expect("resets"), Some(0), "the reader never took the writer");
		writer.execute("BEGIN CONCURRENT", ()).await.expect("begins");
		writer.discard();
		writer.execute("ROLLBACK", ()).await.expect("rolls back");
		drop(writer);
		drop(reader);
		assert_eq!((idle_count(&db, Role::Writer), idle_count(&db, Role::Reader)), (Some(0), Some(1)), "a discarded writer is closed, not pooled");

		control_plane::release(&db);
		assert_eq!(idle_count(&db, Role::Writer), Some(0), "a second owner keeps the pool");
		control_plane::release(&db);
		assert_eq!((idle_count(&db, Role::Writer), idle_count(&db, Role::Reader)), (None, None), "the last owner drops both pools");
		drop(db);
	}
}
