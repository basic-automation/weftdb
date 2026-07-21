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
//! source. The row-count match [`snapshot_and_verify`] asserts holds when the source is
//! **quiescent** for the duration of the call (the intended maintenance/backup-window
//! use). If writers commit to the source *between* the vacuum and the verify read, the
//! source can legitimately have grown past the snapshot; a concurrent-write-safe
//! verification (compare against the snapshot's own committed frame, not a fresh source
//! read) is the next refinement — see the roadmap Phase 7.4 residue.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use turso::Builder;

/// The outcome of a verified control-plane snapshot: where it landed and what was
/// checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReport {
	/// The file the snapshot was written to.
	pub dest: PathBuf,
	/// The number of user tables (excluding `sqlite_*`) whose row counts were compared.
	pub tables: usize,
	/// The total number of rows verified equal across those tables.
	pub rows: i64,
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
	vacuum_into(src, dest).await?;

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

	Ok(SnapshotReport { dest: dest.to_path_buf(), tables: src_tables.len(), rows })
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

		// The copy is independently openable and holds the same rows.
		let copy_db = Builder::new_local(dest.to_str().unwrap()).build().await.unwrap();
		let copy_conn = copy_db.connect().unwrap();
		assert_eq!(count_rows(&copy_conn, "widget").await.unwrap(), 7);
		assert_eq!(count_rows(&copy_conn, "gadget").await.unwrap(), 3);
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
