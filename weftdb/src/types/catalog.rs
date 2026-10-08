//! libSQL `catalog.db` DB/subject registry (roadmap **Phase 4.3**, control-plane
//! registry).
//!
//! The Storage v2 control plane has, until now, two libSQL pieces: the
//! [`SegmentIndexStore`](crate::SegmentIndexStore) (one index row per sealed
//! segment — *where* an aspect's data is) and the
//! [`AspectCatalog`](crate::AspectCatalog) (one row per aspect — *how* it is
//! encoded). Both are keyed by a `(database, subject, aspect)` triple, but nothing
//! yet records the *upper* two levels of that hierarchy in their own right: which
//! databases exist, and which subjects each holds. The roadmap names this missing
//! piece directly — the `catalog.db` "DB/subject/aspect registry" above the segment
//! index.
//!
//! [`CatalogStore`] is that registry. It records:
//!
//! - **databases** — the top-level namespaces (one row per database name);
//! - **subjects** — the measured entities within a database (`(database, subject)`),
//!   refusing a subject whose database is not registered so the hierarchy can never
//!   dangle.
//!
//! The aspect level is already covered by [`AspectCatalog`](crate::AspectCatalog) (which carries the
//! per-aspect schema), so this store stops at databases + subjects; together the two
//! span the full `catalog.db` hierarchy the roadmap layout names.
//!
//! Boundary (hard constraint #3): this is **control-plane** metadata only — names and
//! structure, never a measurement. The measurement bytes live in WeftDB's own `.weftseg`
//! segments. It uses the same MVCC write path the rest of the control plane does
//! (`BEGIN CONCURRENT` for writes).

use anyhow::{bail, Result};
use turso::{Builder, Value};

use crate::types::durable::{fault, FaultPoint};

/// A durable, libSQL-backed registry of the database → subject hierarchy.
///
/// Open one with [`CatalogStore::open`] (a file path) or
/// [`CatalogStore::open_in_memory`] (tests); register a database with
/// [`register_database`](CatalogStore::register_database) and a subject under it with
/// [`register_subject`](CatalogStore::register_subject); enumerate with
/// [`list_databases`](CatalogStore::list_databases) /
/// [`list_subjects`](CatalogStore::list_subjects).
pub struct CatalogStore {
	db: turso::Database,
}

impl CatalogStore {
	/// The tables every `catalog.db` holds once opened. A snapshot missing any of them is
	/// not a backup of this database (an empty file passes every other check), so
	/// backup and restore verification require them.
	pub const TABLES: &'static [&'static str] = &["databases", "subjects"];

	/// Open (creating if absent) the `catalog.db` at `path` on its own, outside a store:
	/// enable MVCC, prove it took, and bring the schema up to this build's layout with the
	/// migration registry's DDL (the `databases` and `subjects` tables). A [`SegmentStore`](crate::SegmentStore)
	/// opens it without DDL instead and runs the registry itself, after its marker gate.
	///
	/// # Errors
	///
	/// Fails if the database does not end up in MVCC journal mode or a new connection
	/// does not sync FULL, and propagates any libSQL connection or DDL failure.
	pub async fn open(path: &str) -> Result<Self> {
		let store = Self::open_unmigrated(path).await?;
		crate::types::migrations::apply_standalone(&crate::types::migrations::ControlPlane { catalog: Some(crate::types::migrations::Db { db: &store.db, name: path }), ..crate::types::migrations::ControlPlane::default() }).await?;
		Ok(store)
	}

	/// Open the `catalog.db` at `path` without running any DDL: enable MVCC, prove it and
	/// sync its header, nothing else. What a [`SegmentStore`](crate::SegmentStore) opens
	/// before it runs the migrations.
	///
	/// # Errors
	///
	/// As [`open`](Self::open), less the DDL.
	pub(crate) async fn open_unmigrated(path: &str) -> Result<Self> {
		let db = Builder::new_local(path).build().await?;
		let conn = crate::types::durable::control_plane::connect(&db).await?;
		// The control plane's MVCC write path; open fails rather than run without it.
		crate::types::durable::control_plane::enable_mvcc_full(&db, &conn, path).await?;
		conn.execute("PRAGMA busy_timeout=600000", turso::params![]).await.ok();
		Ok(Self { db })
	}

	/// The database behind this store, which the migration registry runs against.
	pub(crate) const fn database(&self) -> &turso::Database {
		&self.db
	}

	/// Open an ephemeral in-memory catalog (`:memory:`) for tests.
	///
	/// # Errors
	///
	/// Propagates any libSQL connection or DDL failure.
	pub async fn open_in_memory() -> Result<Self> {
		Self::open(":memory:").await
	}

	/// Snapshot this `catalog.db` to `dest` (a fresh file) via Turso's `VACUUM INTO`,
	/// verifying the copy opens and its rows match. The online, consistent control-plane
	/// backup primitive (roadmap Phase 7.4) — see
	/// [`snapshot_and_verify`](crate::snapshot_and_verify) for the consistency scope.
	///
	/// # Errors
	///
	/// Propagates a connection failure or any backup/verify failure.
	pub async fn backup_to(&self, dest: &std::path::Path) -> Result<crate::SnapshotReport> {
		self.backup_to_with(dest, crate::VerifyMode::default()).await
	}

	/// Snapshot this database to `dest` under an explicit [`VerifyMode`](crate::VerifyMode).
	///
	/// [`VerifyMode::SnapshotOnly`](crate::VerifyMode::SnapshotOnly) verifies the copy
	/// without re-reading the source, so it is the mode an **online** backup (the backup
	/// daemon) must use while writers are still committing.
	///
	/// # Errors
	///
	/// Propagates a connection failure or any backup/verify failure.
	pub async fn backup_to_with(&self, dest: &std::path::Path, mode: crate::VerifyMode) -> Result<crate::SnapshotReport> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		crate::types::backup::snapshot_with_verify(&conn, dest, mode, Self::TABLES).await
	}

	/// Register a database namespace `name` (idempotent — re-registering an existing
	/// name is a no-op rather than an error).
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn register_database(&self, name: &str) -> Result<()> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = conn.execute("INSERT OR IGNORE INTO databases (name) VALUES (?)", turso::params![name.to_string()]).await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("register_database failed: {e}")
			}
		}
	}

	/// Register a `subject` under `database` (idempotent). The database must already be
	/// registered — registering a subject under an unknown database fails so the
	/// hierarchy never dangles.
	///
	/// # Errors
	///
	/// Returns an error if `database` is not registered, or propagates any libSQL write
	/// failure.
	pub async fn register_subject(&self, database: &str, subject: &str) -> Result<()> {
		if !self.database_exists(database).await? {
			bail!("register_subject: database {database:?} is not registered");
		}
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = conn.execute("INSERT OR IGNORE INTO subjects (database, subject) VALUES (?, ?)", turso::params![database.to_string(), subject.to_string()]).await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("register_subject failed: {e}")
			}
		}
	}

	/// Register `database` and `subject` under it in **one** transaction (idempotent):
	/// the scope a [`SegmentStore`](crate::SegmentStore) opens under.
	///
	/// [`register_database`](Self::register_database) followed by
	/// [`register_subject`](Self::register_subject) is two commits, so a crash between
	/// them leaves a database without the subject the open was for (design window
	/// open-scoped-register-database-then-subject). Here both rows commit together or
	/// not at all, and the subject can never dangle because its database is inserted in
	/// the same transaction.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure; nothing is registered unless both rows are.
	pub async fn register_scope(&self, database: &str, subject: &str) -> Result<()> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res: Result<()> = async {
			conn.execute("INSERT OR IGNORE INTO databases (name) VALUES (?)", turso::params![database.to_string()]).await?;
			fault::hit(FaultPoint::OScopeDatabaseInserted).await?;
			conn.execute("INSERT OR IGNORE INTO subjects (database, subject) VALUES (?, ?)", turso::params![database.to_string(), subject.to_string()]).await?;
			Ok(())
		}
		.await;
		match res {
			Ok(()) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("register_scope failed: {e:#}")
			}
		}
	}

	/// Whether `name` is a registered database.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn database_exists(&self, name: &str) -> Result<bool> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		let mut rows = conn.query("SELECT 1 FROM databases WHERE name = ?", turso::params![name.to_string()]).await?;
		Ok(rows.next().await?.is_some())
	}

	/// Whether `(database, subject)` is a registered subject.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn subject_exists(&self, database: &str, subject: &str) -> Result<bool> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		let mut rows = conn.query("SELECT 1 FROM subjects WHERE database = ? AND subject = ?", turso::params![database.to_string(), subject.to_string()]).await?;
		Ok(rows.next().await?.is_some())
	}

	/// All registered database names, in name order.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_databases(&self) -> Result<Vec<String>> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		let mut rows = conn.query("SELECT name FROM databases ORDER BY name", turso::params![]).await?;
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			if let Value::Text(s) = row.get_value(0)? {
				out.push(s);
			}
		}
		Ok(out)
	}

	/// The subject names registered under `database`, in name order.
	///
	/// # Errors
	///
	/// Propagates any libSQL read failure.
	pub async fn list_subjects(&self, database: &str) -> Result<Vec<String>> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		let mut rows = conn.query("SELECT subject FROM subjects WHERE database = ? ORDER BY subject", turso::params![database.to_string()]).await?;
		let mut out = Vec::new();
		while let Some(row) = rows.next().await? {
			if let Value::Text(s) = row.get_value(0)? {
				out.push(s);
			}
		}
		Ok(out)
	}

	/// Remove `subject` from `database` (idempotent — removing an absent subject is a
	/// no-op).
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn remove_subject(&self, database: &str, subject: &str) -> Result<()> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = conn.execute("DELETE FROM subjects WHERE database = ? AND subject = ?", turso::params![database.to_string(), subject.to_string()]).await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("remove_subject failed: {e}")
			}
		}
	}

	/// Remove a database and **all** its subjects (idempotent). The cascade keeps the
	/// hierarchy consistent — no subject can outlive the database it belonged to.
	///
	/// # Errors
	///
	/// Propagates any libSQL write failure.
	pub async fn remove_database(&self, name: &str) -> Result<()> {
		let conn = crate::types::durable::control_plane::connect(&self.db).await?;
		conn.execute("BEGIN CONCURRENT", turso::params![]).await?;
		let res = async {
			conn.execute("DELETE FROM subjects WHERE database = ?", turso::params![name.to_string()]).await?;
			conn.execute("DELETE FROM databases WHERE name = ?", turso::params![name.to_string()]).await
		}
		.await;
		match res {
			Ok(_) => {
				conn.execute("COMMIT", turso::params![]).await?;
				Ok(())
			}
			Err(e) => {
				conn.execute("ROLLBACK", turso::params![]).await.ok();
				bail!("remove_database failed: {e}")
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn register_then_list_databases_in_order() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		catalog.register_database("market").await.expect("registers");
		catalog.register_database("iot").await.expect("registers");
		// Re-registering is a no-op, not a duplicate.
		catalog.register_database("market").await.expect("idempotent");
		let dbs = catalog.list_databases().await.expect("lists");
		let exists = catalog.database_exists("market").await.expect("exists");
		let missing = catalog.database_exists("absent").await.expect("exists");
		drop(catalog);
		assert_eq!(dbs, vec!["iot".to_string(), "market".to_string()]);
		assert!(exists);
		assert!(!missing);
	}

	#[tokio::test]
	async fn subjects_scope_to_their_database_in_order() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		catalog.register_database("market").await.expect("registers");
		catalog.register_database("iot").await.expect("registers");
		catalog.register_subject("market", "BTCUSD").await.expect("registers");
		catalog.register_subject("market", "ETHUSD").await.expect("registers");
		catalog.register_subject("iot", "sensor-7").await.expect("registers");
		let market = catalog.list_subjects("market").await.expect("lists");
		let iot = catalog.list_subjects("iot").await.expect("lists");
		let exists = catalog.subject_exists("market", "BTCUSD").await.expect("exists");
		let missing = catalog.subject_exists("market", "sensor-7").await.expect("exists");
		drop(catalog);
		assert_eq!(market, vec!["BTCUSD".to_string(), "ETHUSD".to_string()]);
		assert_eq!(iot, vec!["sensor-7".to_string()]);
		assert!(exists);
		assert!(!missing, "a subject is isolated to its own database");
	}

	#[tokio::test]
	async fn subject_under_unregistered_database_fails() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		let err = catalog.register_subject("ghost", "BTCUSD").await;
		let exists = catalog.subject_exists("ghost", "BTCUSD").await.expect("exists");
		drop(catalog);
		assert!(err.is_err(), "a subject needs its database registered first");
		assert!(!exists, "the failed registration recorded nothing");
	}

	#[tokio::test]
	async fn register_subject_is_idempotent() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		catalog.register_database("d").await.expect("registers");
		catalog.register_subject("d", "s").await.expect("registers");
		catalog.register_subject("d", "s").await.expect("idempotent");
		let subjects = catalog.list_subjects("d").await.expect("lists");
		drop(catalog);
		assert_eq!(subjects, vec!["s".to_string()]);
	}

	#[tokio::test]
	async fn register_scope_registers_a_database_and_its_subject_together() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		// No prior register_database: the scope brings its own database row.
		catalog.register_scope("market", "BTCUSD").await.expect("registers");
		catalog.register_scope("market", "BTCUSD").await.expect("idempotent");
		catalog.register_scope("market", "ETHUSD").await.expect("a second subject under a registered database");
		let dbs = catalog.list_databases().await.expect("lists");
		let subjects = catalog.list_subjects("market").await.expect("lists");
		drop(catalog);
		assert_eq!(dbs, vec!["market".to_string()]);
		assert_eq!(subjects, vec!["BTCUSD".to_string(), "ETHUSD".to_string()]);
	}

	#[tokio::test]
	async fn remove_subject_leaves_the_database() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		catalog.register_database("d").await.expect("registers");
		catalog.register_subject("d", "a").await.expect("registers");
		catalog.register_subject("d", "b").await.expect("registers");
		catalog.remove_subject("d", "a").await.expect("removes");
		// Removing an absent subject is a no-op.
		catalog.remove_subject("d", "absent").await.expect("idempotent");
		let subjects = catalog.list_subjects("d").await.expect("lists");
		let db_exists = catalog.database_exists("d").await.expect("exists");
		drop(catalog);
		assert_eq!(subjects, vec!["b".to_string()]);
		assert!(db_exists, "removing a subject leaves its database registered");
	}

	#[tokio::test]
	async fn remove_database_cascades_to_subjects() {
		let catalog = CatalogStore::open_in_memory().await.expect("opens");
		catalog.register_database("d").await.expect("registers");
		catalog.register_database("keep").await.expect("registers");
		catalog.register_subject("d", "a").await.expect("registers");
		catalog.register_subject("d", "b").await.expect("registers");
		catalog.register_subject("keep", "c").await.expect("registers");
		catalog.remove_database("d").await.expect("removes");
		let dbs = catalog.list_databases().await.expect("lists");
		let orphans = catalog.list_subjects("d").await.expect("lists");
		let kept = catalog.list_subjects("keep").await.expect("lists");
		drop(catalog);
		assert_eq!(dbs, vec!["keep".to_string()], "the database is gone");
		assert!(orphans.is_empty(), "its subjects cascade away");
		assert_eq!(kept, vec!["c".to_string()], "an unrelated database is untouched");
	}

	#[tokio::test]
	async fn catalog_reopens_and_sees_prior_registrations() {
		let dir = tempfile::TempDir::new().expect("tempdir");
		let path = dir.path().join("catalog.db");
		let path = path.to_string_lossy().into_owned();
		let first = CatalogStore::open(&path).await.expect("opens");
		first.register_database("d").await.expect("registers");
		first.register_subject("d", "s").await.expect("registers");
		drop(first);
		// A fresh store over the same file sees the persisted hierarchy.
		let reopened = CatalogStore::open(&path).await.expect("reopens");
		let dbs = reopened.list_databases().await.expect("lists");
		let subjects = reopened.list_subjects("d").await.expect("lists");
		drop(reopened);
		assert_eq!(dbs, vec!["d".to_string()]);
		assert_eq!(subjects, vec!["s".to_string()]);
	}
}
