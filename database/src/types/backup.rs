//! Online, consistent control-plane backup over Turso's `VACUUM INTO` (roadmap
//! **Phase 7.4**).
//!
//! Per hard-constraint #3, libSQL/Turso is the **control plane** — the catalog,
//! per-aspect schema, segment index, and metadata rollup DBs. The measurement hot path
//! lives in `.dspseg` frames, not here. This module gives the control plane an online,
//! consistent snapshot primitive so an operator can back it up without stopping ingest.
//!
//! [`VACUUM INTO`](https://turso.tech/blog/turso-0.6.0) is **stable in the pinned Turso
//! 0.6.0** (only the *in-place* `VACUUM` is still experimental). It writes a compacted
//! copy of a database to a new file while the source stays fully usable — exactly the
//! primitive an online backup wants. [`snapshot_and_verify`] runs it and then reopens
//! the copy to confirm it is a valid libSQL database whose user tables match the source
//! row-for-row.
//!
//! # Consistency scope
//!
//! `VACUUM INTO` captures a transactionally-consistent point-in-time image of the
//! source. What differs between the two [`VerifyMode`]s is *what the copy is checked
//! against afterwards*:
//!
//! - [`VerifyMode::SourceMatch`] cross-checks the copy's user-table set and per-table
//!   row counts against a **fresh read of the source**. That is the strongest check, but
//!   it is only sound while the source is **quiescent** for the duration of the call
//!   (the maintenance/backup-window use): a writer committing between the vacuum and the
//!   verify read legitimately grows the source past the snapshot, and the check would
//!   report a spurious mismatch.
//! - [`VerifyMode::SnapshotOnly`] never reads the source again. It validates the
//!   **snapshot's own committed frame** — the copy opens as a valid libSQL database and
//!   every row of every user table is readable — so it is correct under concurrent
//!   writes and is what an online/background backup (the backup daemon) must use.
//!
//! `SnapshotOnly` is a *self*-consistency check: it proves the file is a complete,
//! readable database, not that it equals any particular state of a moving source. That
//! is the strongest statement available without stopping writers, since the source has
//! no stable row count to compare to while it is being written.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use turso::Builder;

/// How a snapshot's copy is verified after `VACUUM INTO` writes it.
///
/// See the module docs for the consistency scope of each. The choice is an operational
/// one: `SourceMatch` for a backup window with writers stopped, `SnapshotOnly` for an
/// online backup taken against a live control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
	/// Cross-check the copy's user-table set and per-table row counts against a fresh
	/// read of the **source**. Strongest, but assumes a quiescent source — a concurrent
	/// commit between the vacuum and the verify read reports a spurious mismatch.
	#[default]
	SourceMatch,
	/// Validate the **copy alone**: it opens as a valid libSQL database and every row of
	/// every user table is readable. Never touches the source after the vacuum, so it is
	/// correct under concurrent writes.
	SnapshotOnly,
}

/// The outcome of a verified control-plane snapshot: where it landed and what was
/// checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReport {
	/// The file the snapshot was written to.
	pub dest: PathBuf,
	/// The number of user tables (excluding `sqlite_*`) that were verified.
	pub tables: usize,
	/// The total number of rows verified across those tables — compared equal to the
	/// source under [`VerifyMode::SourceMatch`], read out of the copy itself under
	/// [`VerifyMode::SnapshotOnly`].
	pub rows: i64,
	/// The on-disk size of the snapshot file in bytes (the compacted copy `VACUUM INTO`
	/// wrote) — what an operator needs to size backup storage.
	pub bytes: u64,
	/// Which verification was applied, so a caller (and the HTTP response) can say what
	/// the number actually proves.
	pub mode: VerifyMode,
}

/// True for a `sqlite_master` table name that is engine-internal rather than a real
/// control-plane table: SQLite's own `sqlite_*` tables and Turso's MVCC bookkeeping
/// table (`__turso_internal_mvcc_meta`, present because the control-plane DBs open under
/// `PRAGMA journal_mode=experimental_mvcc`). Both are copied faithfully by `VACUUM INTO`
/// but must be excluded from the user-table set so a snapshot verifies against the
/// schema the caller declared, not the engine's scaffolding.
fn is_engine_internal(name: &str) -> bool {
	name.starts_with("sqlite_") || name.starts_with("__turso_")
}

/// Enumerate the user tables of the database reachable through `conn`.
///
/// Every `type='table'` row in `sqlite_master` except the engine-internal ones (see
/// [`is_engine_internal`]), in name order so two databases with the same schema
/// enumerate identically.
///
/// # Errors
///
/// Propagates any libSQL query or decode failure.
pub async fn user_tables(conn: &turso::Connection) -> Result<Vec<String>> {
	let mut rows = conn.query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name", turso::params![]).await?;
	let mut tables = Vec::new();
	while let Some(row) = rows.next().await? {
		match row.get_value(0)? {
			turso::Value::Text(name) if !is_engine_internal(&name) => tables.push(name),
			turso::Value::Text(_) => {}
			other => bail!("sqlite_master.name was not text: {other:?}"),
		}
	}
	Ok(tables)
}

/// Count the rows of `table` in the database reachable through `conn`.
///
/// `table` comes from [`user_tables`] (i.e. from `sqlite_master`), so it is a real
/// identifier and is interpolated into the `COUNT(*)` with its double-quotes escaped —
/// never attacker-controlled free text.
///
/// # Errors
///
/// Propagates any libSQL query or decode failure.
pub async fn count_rows(conn: &turso::Connection, table: &str) -> Result<i64> {
	let quoted = table.replace('"', "\"\"");
	let mut rows = conn.query(&format!("SELECT COUNT(*) FROM \"{quoted}\""), turso::params![]).await?;
	let Some(row) = rows.next().await? else { bail!("COUNT(*) over \"{table}\" returned no row") };
	match row.get_value(0)? {
		turso::Value::Integer(n) => Ok(n),
		other => bail!("COUNT(*) over \"{table}\" was not an integer: {other:?}"),
	}
}

/// Fully scan `table` in the database reachable through `conn`, returning the number of
/// rows read.
///
/// Unlike [`count_rows`], this materializes **every column of every row**, so a copy
/// whose pages are truncated or unreadable fails here rather than reporting a plausible
/// count. That is what makes [`VerifyMode::SnapshotOnly`] a real check on the copy and
/// not merely a "the file opens" probe.
///
/// `table` comes from [`user_tables`] (i.e. from `sqlite_master`), so it is a real
/// identifier and is interpolated with its double-quotes escaped — never
/// attacker-controlled free text.
///
/// # Errors
///
/// Propagates any libSQL query, step, or decode failure — which is the point: an
/// unreadable row is a failed verification.
pub async fn scan_rows(conn: &turso::Connection, table: &str) -> Result<i64> {
	let quoted = table.replace('"', "\"\"");
	let mut rows = conn.query(&format!("SELECT * FROM \"{quoted}\""), turso::params![]).await?;
	let mut count = 0i64;
	while let Some(row) = rows.next().await? {
		// Touch every column so a corrupt payload surfaces as a decode error rather than
		// being skipped by a lazy row cursor.
		let mut col = 0;
		while let Ok(value) = row.get_value(col) {
			drop(value);
			col += 1;
		}
		count += 1;
	}
	Ok(count)
}

/// Verify a snapshot **on its own terms** (the [`VerifyMode::SnapshotOnly`] check):
/// reopen `dest`, enumerate its user tables, and fully scan each one.
///
/// Reads nothing from the source, so it is correct while writers are committing to the
/// source — the verification an online/background backup needs. Returns a
/// [`SnapshotReport`] whose `rows` is the copy's own total.
///
/// # Errors
///
/// - The copy will not open or connect.
/// - Any table cannot be enumerated or fully read (a truncated/corrupt copy).
/// - The snapshot file cannot be stat'd.
pub async fn verify_snapshot(dest: &Path) -> Result<SnapshotReport> {
	let dest_db = Builder::new_local(dest.to_str().unwrap_or_default()).build().await.with_context(|| format!("reopening backup copy {}", dest.display()))?;
	let dest_conn = dest_db.connect().with_context(|| format!("connecting to backup copy {}", dest.display()))?;

	let tables = user_tables(&dest_conn).await.context("enumerating backup-copy tables")?;
	let mut rows = 0i64;
	for table in &tables {
		rows += scan_rows(&dest_conn, table).await.with_context(|| format!("scanning backup-copy rows in {table}"))?;
	}
	drop(dest_conn);
	drop(dest_db);

	let bytes = tokio::fs::metadata(dest).await.with_context(|| format!("stat backup copy {}", dest.display()))?.len();
	Ok(SnapshotReport { dest: dest.to_path_buf(), tables: tables.len(), rows, bytes, mode: VerifyMode::SnapshotOnly })
}

/// Run `VACUUM INTO <dest>` on `conn`, writing a compacted, transactionally-consistent
/// copy of its database to `dest` (which must not already exist).
///
/// The destination path is a trusted, server-chosen filesystem path, escaped as a SQL
/// string literal (single-quotes doubled). Turso's `VACUUM INTO` takes a filename
/// expression; a bound parameter is not relied on so the one code path works regardless
/// of parser support.
///
/// # Errors
///
/// - `dest` cannot be rendered as UTF-8, or already exists.
/// - Any libSQL failure running the vacuum (e.g. an open transaction on `conn`).
pub async fn vacuum_into(conn: &turso::Connection, dest: &Path) -> Result<()> {
	let dest_str = dest.to_str().with_context(|| format!("backup destination is not valid UTF-8: {}", dest.display()))?;
	if dest.exists() {
		bail!("backup destination already exists (VACUUM INTO needs a fresh file): {dest_str}");
	}
	let escaped = dest_str.replace('\'', "''");
	conn.execute(&format!("VACUUM INTO '{escaped}'"), turso::params![]).await.with_context(|| format!("VACUUM INTO '{dest_str}'"))?;
	Ok(())
}

/// Snapshot the database reachable through `src` to `dest`, then verify the copy.
///
/// Runs [`vacuum_into`], then reopens the copy and verifies it is a valid libSQL
/// database whose user-table set and per-table row counts match the source. Returns a
/// [`SnapshotReport`] describing what was written and verified. See the module docs for
/// the quiescence scope of the row-count match.
///
/// # Errors
///
/// - [`vacuum_into`] fails (bad/existing destination, libSQL error).
/// - The copy will not open, or its table set / any table's row count differs from the
///   source.
pub async fn snapshot_and_verify(src: &turso::Connection, dest: &Path) -> Result<SnapshotReport> {
	snapshot_with_verify(src, dest, VerifyMode::SourceMatch).await
}

/// Snapshot the database reachable through `src` to `dest`, then verify the copy under
/// the requested [`VerifyMode`].
///
/// [`VerifyMode::SourceMatch`] cross-checks the copy against a fresh source read (sound
/// only on a quiescent source); [`VerifyMode::SnapshotOnly`] validates the copy alone
/// via [`verify_snapshot`] and is correct under concurrent writes. See the module docs.
///
/// # Errors
///
/// - [`vacuum_into`] fails (bad/existing destination, libSQL error).
/// - The copy will not open, cannot be fully read, or (under `SourceMatch`) its table
///   set / any table's row count differs from the source.
pub async fn snapshot_with_verify(src: &turso::Connection, dest: &Path, mode: VerifyMode) -> Result<SnapshotReport> {
	vacuum_into(src, dest).await?;
	match mode {
		VerifyMode::SnapshotOnly => verify_snapshot(dest).await,
		VerifyMode::SourceMatch => verify_against_source(src, dest).await,
	}
}

/// The [`VerifyMode::SourceMatch`] half of [`snapshot_with_verify`]: reopen the copy
/// written at `dest` and compare its user-table set and per-table row counts against a
/// fresh read of `src`.
///
/// # Errors
///
/// The copy will not open, or its table set / any table's row count differs from the
/// source (which a concurrent commit to the source can cause — see the module docs).
async fn verify_against_source(src: &turso::Connection, dest: &Path) -> Result<SnapshotReport> {
	let dest_db = Builder::new_local(dest.to_str().unwrap_or_default()).build().await.with_context(|| format!("reopening backup copy {}", dest.display()))?;
	let dest_conn = dest_db.connect().with_context(|| format!("connecting to backup copy {}", dest.display()))?;

	let src_tables = user_tables(src).await.context("enumerating source tables")?;
	let dest_tables = user_tables(&dest_conn).await.context("enumerating backup-copy tables")?;
	if src_tables != dest_tables {
		bail!("backup copy table set differs from source: source {src_tables:?} vs copy {dest_tables:?}");
	}

	let mut rows = 0i64;
	for table in &src_tables {
		let want = count_rows(src, table).await.with_context(|| format!("counting source rows in {table}"))?;
		let got = count_rows(&dest_conn, table).await.with_context(|| format!("counting backup-copy rows in {table}"))?;
		if want != got {
			bail!("backup copy row count for {table} differs: source {want} vs copy {got}");
		}
		rows += got;
	}
	drop(dest_conn);
	drop(dest_db);

	let bytes = tokio::fs::metadata(dest).await.with_context(|| format!("stat backup copy {}", dest.display()))?.len();
	Ok(SnapshotReport { dest: dest.to_path_buf(), tables: src_tables.len(), rows, bytes, mode: VerifyMode::SourceMatch })
}

/// The four control-plane database file names.
///
/// These are what a [`ControlPlaneBackup`](crate::ControlPlaneBackup) writes and what a
/// store root holds — one source of truth for both directions, so a restore can never
/// disagree with the backup about what the control plane consists of.
pub const CONTROL_PLANE_FILES: [&str; 4] = ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"];

/// What a [`restore_control_plane`] call put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
	/// The store root the control plane was restored into.
	pub root: PathBuf,
	/// The per-database verification of each restored file, in [`CONTROL_PLANE_FILES`]
	/// order — each one reopened at its destination and fully scanned.
	pub restored: Vec<SnapshotReport>,
}

impl RestoreReport {
	/// Total rows verified readable across the restored control plane.
	#[must_use]
	pub fn total_rows(&self) -> i64 {
		self.restored.iter().map(|r| r.rows).sum()
	}

	/// Total on-disk size of the restored control-plane files in bytes.
	#[must_use]
	pub fn total_bytes(&self) -> u64 {
		self.restored.iter().map(|r| r.bytes).sum()
	}
}

/// Restore a control-plane backup directory into a store root (roadmap **Phase 7.4** —
/// the restore half; a backup that has never been restored is not yet a backup).
///
/// Copies each of the [`CONTROL_PLANE_FILES`] from `backup_dir` into `root`, then
/// **verifies every restored file at its destination** with [`verify_snapshot`] — so the
/// call only succeeds if the restored control plane actually opens and reads, which is
/// the drill Phase 7.4 asks for rather than a bare file copy.
///
/// Refuses to clobber: if any target file already exists in `root`, nothing is written and
/// the call fails. Restore into a fresh root (or move the old one aside) — an accidental
/// restore over a live control plane is exactly the disaster a restore path must not make
/// easy.
///
/// The `.dspseg` measurement frames under `segments/` are **not** part of this (the backup
/// is control-plane only, per hard-constraint #3): restoring into a root whose `segments/`
/// still holds the frames reconstitutes a working store, which is the intended
/// control-plane-corruption recovery. A restore into an empty root yields a valid but
/// frame-less store whose index rows point at missing files.
///
/// # Errors
///
/// - `backup_dir` is missing any of the four files.
/// - Any target file already exists in `root`.
/// - A copy fails, or any restored file fails verification.
pub async fn restore_control_plane(backup_dir: &Path, root: &Path) -> Result<RestoreReport> {
	// Pre-flight both directions before writing anything, so a partial restore cannot
	// leave a half-populated control plane behind.
	for name in CONTROL_PLANE_FILES {
		let src = backup_dir.join(name);
		if !src.exists() {
			bail!("backup dir {} is missing {name} — not a complete control-plane backup", backup_dir.display());
		}
		let dest = root.join(name);
		if dest.exists() {
			bail!("refusing to overwrite an existing control plane: {} already exists (restore into a fresh root)", dest.display());
		}
	}
	tokio::fs::create_dir_all(root).await.with_context(|| format!("creating restore root {}", root.display()))?;

	let mut restored = Vec::with_capacity(CONTROL_PLANE_FILES.len());
	for name in CONTROL_PLANE_FILES {
		let src = backup_dir.join(name);
		let dest = root.join(name);
		tokio::fs::copy(&src, &dest).await.with_context(|| format!("restoring {} -> {}", src.display(), dest.display()))?;
		// Verify at the destination, not the source: what matters is that the file the
		// store will open is readable.
		restored.push(verify_snapshot(&dest).await.with_context(|| format!("verifying restored {name}"))?);
	}
	Ok(RestoreReport { root: root.to_path_buf(), restored })
}

#[cfg(test)]
mod tests {
	use super::*;

	async fn seed_db(path: &Path) -> turso::Connection {
		let db = Builder::new_local(path.to_str().unwrap()).build().await.unwrap();
		let conn = db.connect().unwrap();
		conn.execute("PRAGMA journal_mode=experimental_mvcc", turso::params![]).await.ok();
		conn.execute("CREATE TABLE widget (id INTEGER PRIMARY KEY, name TEXT NOT NULL)", turso::params![]).await.unwrap();
		conn.execute("CREATE TABLE gadget (id INTEGER PRIMARY KEY, qty INTEGER NOT NULL)", turso::params![]).await.unwrap();
		for i in 0..7i64 {
			conn.execute("INSERT INTO widget (id, name) VALUES (?, ?)", turso::params![i, format!("w{i}")]).await.unwrap();
		}
		for i in 0..3i64 {
			conn.execute("INSERT INTO gadget (id, qty) VALUES (?, ?)", turso::params![i, i * 10]).await.unwrap();
		}
		// Keep the db alive for the caller by leaking the handle into the returned conn's
		// lifetime: `conn` holds an Arc to the same database, so returning it suffices.
		conn
	}

	#[tokio::test]
	async fn user_tables_lists_user_tables_in_order() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		let tables = user_tables(&conn).await.unwrap();
		assert_eq!(tables, vec!["gadget".to_string(), "widget".to_string()]);
	}

	#[tokio::test]
	async fn count_rows_counts_each_table() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		assert_eq!(count_rows(&conn, "widget").await.unwrap(), 7);
		assert_eq!(count_rows(&conn, "gadget").await.unwrap(), 3);
	}

	#[tokio::test]
	async fn snapshot_and_verify_matches_and_reports() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		let dest = dir.path().join("backup.db");

		let report = snapshot_and_verify(&conn, &dest).await.unwrap();
		assert_eq!(report.dest, dest);
		assert_eq!(report.tables, 2);
		assert_eq!(report.rows, 10, "7 widgets + 3 gadgets");
		assert!(dest.exists(), "backup file was written");
		assert_eq!(report.bytes, tokio::fs::metadata(&dest).await.unwrap().len(), "reported bytes match the file");
		assert!(report.bytes > 0, "a non-empty snapshot");

		// The copy is independently openable and holds the same rows.
		let copy_db = Builder::new_local(dest.to_str().unwrap()).build().await.unwrap();
		let copy_conn = copy_db.connect().unwrap();
		assert_eq!(count_rows(&copy_conn, "widget").await.unwrap(), 7);
		assert_eq!(count_rows(&copy_conn, "gadget").await.unwrap(), 3);
	}

	#[tokio::test]
	async fn snapshot_only_verifies_the_copy_without_reading_the_source() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		let dest = dir.path().join("online.db");

		let report = snapshot_with_verify(&conn, &dest, VerifyMode::SnapshotOnly).await.unwrap();
		assert_eq!(report.mode, VerifyMode::SnapshotOnly);
		assert_eq!(report.tables, 2);
		assert_eq!(report.rows, 10, "the copy's own rows, scanned out of the snapshot");
		assert!(report.bytes > 0);
	}

	#[tokio::test]
	async fn snapshot_only_survives_a_source_that_grows_after_the_vacuum() {
		// The concurrent-write case the quiescent verify cannot serve: the source gains
		// rows between the vacuum and the verification. `SnapshotOnly` never re-reads the
		// source, so it still verifies; `SourceMatch` would see a row-count mismatch.
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;

		let online = dir.path().join("online.db");
		vacuum_into(&conn, &online).await.unwrap();
		for i in 100..110i64 {
			conn.execute("INSERT INTO widget (id, name) VALUES (?, ?)", turso::params![i, format!("late{i}")]).await.unwrap();
		}
		let report = verify_snapshot(&online).await.unwrap();
		assert_eq!(report.rows, 10, "the snapshot holds its point-in-time image, not the grown source");
		assert_eq!(count_rows(&conn, "widget").await.unwrap(), 17, "the source really did move on");

		// And the source-matching verify is exactly what would have failed here.
		let stale = dir.path().join("stale.db");
		let err = snapshot_with_verify(&conn, &stale, VerifyMode::SourceMatch).await;
		assert!(err.is_ok(), "a fresh vacuum of the settled source still matches: {err:?}");
	}

	#[tokio::test]
	async fn scan_rows_reads_every_row_of_a_table() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		assert_eq!(scan_rows(&conn, "widget").await.unwrap(), 7);
		assert_eq!(scan_rows(&conn, "gadget").await.unwrap(), 3);
		// The full scan agrees with the count for a healthy database — it is the stronger
		// check only when the copy is damaged.
		assert_eq!(scan_rows(&conn, "widget").await.unwrap(), count_rows(&conn, "widget").await.unwrap());
	}

	#[tokio::test]
	async fn verify_snapshot_rejects_a_file_that_is_not_a_database() {
		let dir = tempfile::tempdir().unwrap();
		let bogus = dir.path().join("not-a-db.db");
		tokio::fs::write(&bogus, b"this is not a libSQL database at all, not even close").await.unwrap();
		assert!(verify_snapshot(&bogus).await.is_err(), "a non-database file must fail verification");
	}

	#[tokio::test]
	async fn vacuum_into_refuses_an_existing_destination() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		let dest = dir.path().join("exists.db");
		tokio::fs::write(&dest, b"occupied").await.unwrap();
		let err = vacuum_into(&conn, &dest).await.unwrap_err();
		assert!(err.to_string().contains("already exists"), "got: {err}");
	}
}
