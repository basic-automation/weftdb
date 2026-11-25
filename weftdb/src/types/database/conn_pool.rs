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
	collections::HashMap, sync::{Arc, LazyLock, Mutex, PoisonError}
};

/// Idle connections kept per database by default. More concurrent transactions than this on
/// one file just open extra connections, which are dropped when they finish. Each idle
/// connection keeps its own page cache warm, so this trades memory for fewer reads.
/// `WEFT_POOL_MAX_IDLE` overrides it; `0` turns pooling off, and `1` is raised to 2 (see
/// [`max_idle_from`]).
const DEFAULT_MAX_IDLE: usize = 4;

/// Page cache each new connection gets by default, in KiB (`PRAGMA cache_size=-2048`, 512
/// pages of 4 KiB). Turso 0.8.1 gives every connection its own page cache of 2000 pages
/// (8 MB at 4 KiB pages), and with up to [`DEFAULT_MAX_IDLE`] idle connections per file and
/// role across a few hundred files, those caches were the largest heap growth in the fanctl
/// soak. A small cache only costs rereads of pages the OS page cache still holds.
/// `WEFT_CONN_CACHE_KIB` overrides it; Turso keeps at least 200 pages whatever is asked.
const DEFAULT_CONN_CACHE_KIB: u32 = 2048;

static CONN_CACHE_KIB: LazyLock<u32> = LazyLock::new(|| std::env::var("WEFT_CONN_CACHE_KIB").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_CONN_CACHE_KIB));

/// Page cache each new connection to a store's root catalog (`metadata.db`) gets by
/// default, in KiB. The catalog holds every aspect's unbatched queue (about 5.5 MB in the
/// fanctl soak, almost all of it `unbatched_measurements` and its index), and the batch
/// consumer walks that queue per aspect on every run. With [`DEFAULT_CONN_CACHE_KIB`] the
/// walk did not fit and every run reread the file: the soak read 3.2 times as much as with
/// Turso's default cache over 24 virtual hours and about 24 times as much over 72. 8 MiB
/// holds it and brings reads back to 1.05 times, while the few hundred per-series files keep
/// the small cache, so memory stays where the small cache put it. 4 MiB was too small (1.6
/// times) and 16 MiB gained nothing. It is one file, so the extra is bounded by its idle and
/// in-flight connections times the file's size. `WEFT_METADATA_CACHE_KIB` overrides it.
const DEFAULT_METADATA_CACHE_KIB: u32 = 8192;

static METADATA_CACHE_KIB: LazyLock<u32> = LazyLock::new(|| std::env::var("WEFT_METADATA_CACHE_KIB").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_METADATA_CACHE_KIB));

/// The file name of a store's root catalog, which gets [`METADATA_CACHE_KIB`].
const CATALOG_FILE_NAME: &str = "metadata.db";

/// The page cache, in KiB, for connections to the legacy database at `path`: the store's
/// root catalog gets [`METADATA_CACHE_KIB`], every other file [`CONN_CACHE_KIB`]. The
/// handle cache passes the result to [`register_with_cache`] when it opens the file, so
/// every connection its pool opens (concurrent and immediate transactions alike) gets it.
pub fn cache_kib_for_path(path: &str) -> u32 {
	if std::path::Path::new(path).file_name().is_some_and(|name| name == CATALOG_FILE_NAME) {
		*METADATA_CACHE_KIB
	} else {
		*CONN_CACHE_KIB
	}
}

/// A new connection to `db` with its page cache set to `kib`. `cache_size` is per
/// connection, so it has to be set on each one. Failing to set it keeps Turso's default
/// cache rather than failing the caller.
async fn connect(db: &turso::Database, kib: u32) -> turso::Result<turso::Connection> {
	let conn = db.connect()?;
	// A query, drained, rather than `execute`, in case the pragma answers with a row.
	let set = async {
		let mut rows = conn.query(format!("PRAGMA cache_size=-{kib}"), ()).await?;
		while rows.next().await?.is_some() {}
		turso::Result::Ok(())
	};
	if let Err(e) = set.await {
		tracing::debug!("[conn_pool] PRAGMA cache_size failed, keeping Turso's default: {e}");
	}
	Ok(conn)
}

static MAX_IDLE: LazyLock<usize> = LazyLock::new(|| max_idle_from(std::env::var("WEFT_POOL_MAX_IDLE").ok().as_deref()));

/// The idle limit `WEFT_POOL_MAX_IDLE` asks for. A malformed or absent value is
/// [`DEFAULT_MAX_IDLE`], and `1` is raised to 2: the batch consumer and ingest routinely
/// run two transactions on one file at once, so a single idle slot sent every other
/// transaction to a fresh, cold connection. In the fanctl soak that read about 30 times as
/// much, took twice as long, and grew about 30 MB per virtual hour from Turso's per-dropped-
/// connection leak; 2 and 4 behaved the same.
fn max_idle_from(value: Option<&str>) -> usize {
	match value.and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_MAX_IDLE) {
		1 => {
			tracing::warn!("WEFT_POOL_MAX_IDLE=1 keeps too few connections and rereads every file; using 2");
			2
		}
		n => n,
	}
}

pub struct Pool {
	idle: Mutex<Vec<turso::Connection>>,
	/// Cleared by [`unregister`]; a returned connection then goes away instead.
	open: std::sync::atomic::AtomicBool,
	/// When a connection was last checked out, in milliseconds since [`EPOCH`]. The idle
	/// sweep reads it: every transaction checks out, so a file used through any clone of its
	/// handle (not only through the handle cache) counts as in use.
	last_checkout: std::sync::atomic::AtomicU64,
	/// The page cache, in KiB, of each connection this pool opens. The largest any owner
	/// registered with.
	cache_kib: std::sync::atomic::AtomicU32,
}

static EPOCH: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

fn stamp(at: std::time::Instant) -> u64 {
	u64::try_from(at.saturating_duration_since(*EPOCH).as_millis()).unwrap_or(u64::MAX)
}

/// Whether `db`'s `role` pool handed out a connection less than `within` before `now`.
/// An unpooled database answers false.
pub fn used_within(db: &turso::Database, role: Role, within: std::time::Duration, now: std::time::Instant) -> bool {
	let last = POOLS.lock().unwrap_or_else(PoisonError::into_inner).get(&(identity(db), role)).map(|(_, pool)| pool.last_checkout.load(std::sync::atomic::Ordering::Relaxed));
	last.is_some_and(|last| stamp(now).saturating_sub(last) < u64::try_from(within.as_millis()).unwrap_or(u64::MAX))
}

/// Whether `db` currently has a `role` pool. A clone kept past an idle sweep of its file
/// has none. Production code keeps no such clones (`Database::get_*_db` caches paths), so
/// only tests ask.
#[cfg(test)]
pub fn is_pooled(db: &turso::Database, role: Role) -> bool {
	POOLS.lock().unwrap_or_else(PoisonError::into_inner).contains_key(&(identity(db), role))
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

/// Start pooling `role` connections to `db`, each with the default page cache
/// ([`CONN_CACHE_KIB`]). The owner keeps a clone of `db` until it calls [`unregister`], so
/// the identity cannot be reused while the pool exists.
pub fn register(db: &turso::Database, role: Role) {
	register_with_cache(db, role, *CONN_CACHE_KIB);
}

/// [`register`] with each new connection's page cache set to `cache_kib`. A pool registered
/// more than once keeps the largest size asked for; connections already idle keep theirs.
pub fn register_with_cache(db: &turso::Database, role: Role, cache_kib: u32) {
	let mut pools = POOLS.lock().unwrap_or_else(PoisonError::into_inner);
	let entry = pools.entry((identity(db), role)).or_insert_with(|| (0, Arc::new(Pool { idle: Mutex::new(Vec::new()), open: std::sync::atomic::AtomicBool::new(true), last_checkout: std::sync::atomic::AtomicU64::new(stamp(std::time::Instant::now())), cache_kib: std::sync::atomic::AtomicU32::new(cache_kib) })));
	entry.0 += 1;
	entry.1.cache_kib.fetch_max(cache_kib, std::sync::atomic::Ordering::Relaxed);
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
///
/// A new connection gets the page cache its pool was registered with, or [`CONN_CACHE_KIB`]
/// when `db` is not pooled.
pub async fn checkout(db: &turso::Database, role: Role) -> turso::Result<(turso::Connection, Option<Arc<Pool>>)> {
	let pool = POOLS.lock().unwrap_or_else(PoisonError::into_inner).get(&(identity(db), role)).map(|(_, pool)| pool.clone());
	let Some(pool) = pool else { return Ok((connect(db, *CONN_CACHE_KIB).await?, None)) };
	pool.last_checkout.store(stamp(std::time::Instant::now()), std::sync::atomic::Ordering::Relaxed);
	let idle = pool.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
	let conn = match idle {
		Some(conn) => conn,
		None => connect(db, pool.cache_kib.load(std::sync::atomic::Ordering::Relaxed)).await?,
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
	async fn a_new_connection_gets_the_small_page_cache() {
		let dir = tempfile::tempdir().expect("a temp dir");
		let db = turso::Builder::new_local(dir.path().join("c.db").to_str().expect("utf-8")).build().await.expect("opens");
		let (conn, pool) = checkout(&db, Role::Legacy).await.expect("connects");
		assert!(pool.is_none(), "an unregistered database is not pooled");
		let mut rows = conn.query("PRAGMA cache_size", ()).await.expect("queries");
		let row = rows.next().await.expect("reads").expect("a row");
		assert_eq!(row.get::<i64>(0).expect("an integer"), -i64::from(*CONN_CACHE_KIB), "the pragma reached the connection");
	}

	async fn cache_size(conn: &turso::Connection) -> i64 {
		let mut rows = conn.query("PRAGMA cache_size", ()).await.expect("queries");
		rows.next().await.expect("reads").expect("a row").get::<i64>(0).expect("an integer")
	}

	#[tokio::test]
	async fn the_catalog_pool_gets_the_larger_page_cache_for_every_transaction_kind() {
		let dir = tempfile::tempdir().expect("a temp dir");
		let path = dir.path().join(CATALOG_FILE_NAME);
		let path = path.to_str().expect("utf-8");
		assert_eq!(cache_kib_for_path(path), *METADATA_CACHE_KIB, "the root catalog is recognised by its file name");
		assert_eq!(cache_kib_for_path(dir.path().join("measurements.db").to_str().expect("utf-8")), *CONN_CACHE_KIB, "any other file keeps the small cache");
		assert_eq!(cache_kib_for_path(dir.path().join("metadata.db-wal").to_str().expect("utf-8")), *CONN_CACHE_KIB, "only the exact file name counts");

		let db = turso::Builder::new_local(path).build().await.expect("opens");
		let setup = db.connect().expect("connects");
		setup.execute("PRAGMA journal_mode=experimental_mvcc", ()).await.ok();
		drop(setup);
		register_with_cache(&db, Role::Legacy, cache_kib_for_path(path));
		register(&db, Role::Legacy);
		let immediate = Database::begin_immediate(&db).await.expect("an immediate transaction");
		assert_eq!(cache_size(immediate.as_ref()).await, -i64::from(*METADATA_CACHE_KIB), "an immediate transaction gets the catalog cache, and a second, default registration did not shrink it");
		Database::commit_immediate(&immediate).await.expect("commits");
		let first = Database::begin_concurrent(&db, path, None).await.expect("reuses the idle connection");
		let second = Database::begin_concurrent(&db, path, None).await.expect("opens a new connection beside it");
		assert_eq!(cache_size(second.as_ref()).await, -i64::from(*METADATA_CACHE_KIB), "a concurrent transaction on a new connection gets it too");
		Database::commit_concurrent(&second).await.expect("commits");
		Database::commit_concurrent(&first).await.expect("commits");
		drop((immediate, first, second));
		unregister(&db, Role::Legacy);
		unregister(&db, Role::Legacy);
		assert_eq!(idle_count(&db, Role::Legacy), None);
		drop(db);
	}

	#[test]
	fn one_idle_connection_is_raised_to_two() {
		assert_eq!(max_idle_from(None), DEFAULT_MAX_IDLE);
		assert_eq!(max_idle_from(Some("junk")), DEFAULT_MAX_IDLE);
		assert_eq!(max_idle_from(Some("0")), 0, "0 still turns pooling off");
		assert_eq!(max_idle_from(Some("1")), 2, "1 sends concurrent transactions to cold connections");
		assert_eq!(max_idle_from(Some(" 3 ")), 3);
	}

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
