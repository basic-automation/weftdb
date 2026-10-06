//! Online, consistent control-plane backup over Turso's `VACUUM INTO` (roadmap
//! **Phase 7.4**).
//!
//! Per hard-constraint #3, libSQL/Turso is the **control plane** — the catalog,
//! per-aspect schema, segment index, and metadata rollup DBs. The measurement hot path
//! lives in `.weftseg` frames, not here. This module gives the control plane an online,
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
//! no stable row count to compare to while it is being written. Both modes also require
//! the database's expected table set, so an empty file never passes.
//!
//! # Backup directories (docs/design/crash-consistency.md section 9)
//!
//! A backup directory is visible under its final name only once it is complete and
//! durable. [`SegmentStore::backup_control_plane`](crate::SegmentStore::backup_control_plane)
//! builds it in `.partial-{label}-{nonce}/`, verifies every snapshot, writes a
//! [`BACKUP_MANIFEST`] and fsyncs the directory, then renames it to its label and fsyncs
//! the base. Every other name a crash can leave behind starts with one of the
//! [`STAGING_PREFIXES`]: such a directory is never counted, listed or restored, and
//! [`sweep_backup_staging`] removes it once it has been left untouched for
//! [`STAGING_SWEEP_AGE`]. Retention removes a backup with [`retire_backup`], which
//! renames it to `.deleting-*` (durably) before removing its files, so a crash part-way
//! through never leaves a half-removed directory under a backup's name. Directories from
//! before the manifest (the four databases, no manifest) still count and restore.

use std::{
	collections::BTreeSet, ffi::{OsStr, OsString}, io, path::{Path, PathBuf}, time::{Duration, SystemTime}
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use turso::Builder;

use crate::types::durable::{
	fault::{self, FaultPoint}, RealFs, StoreFs, SyncPolicy
};

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
	/// The names of those tables, in name order. A backup's manifest records them.
	pub table_names: Vec<String>,
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
/// `is_engine_internal`), in name order so two databases with the same schema
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

/// The sidecar suffixes libSQL/Turso leaves beside a database file once it has been
/// opened: the WAL (`-wal`) and Turso's MVCC logical log (`-log`). The same two names
/// the legacy `Database` opener sweeps before reopening a measurement DB.
const SIDECAR_SUFFIXES: [&str; 2] = ["-wal", "-log"];

/// Remove the **empty** sidecar files Turso leaves beside `db` after a verification
/// reopens it.
///
/// `VACUUM INTO` creates a zero-byte `<db>-wal` beside the copy it writes, and reopening
/// the snapshot to verify it (which both [`VerifyMode`]s do) adds a zero-byte `<db>-log`
/// — the journal scaffolding for a database nobody will ever write to. They are pure
/// litter in a backup directory: the reported `bytes` never counted them, but an operator
/// listing a control-plane backup should see four files, not twelve.
///
/// Only a sidecar whose length is **exactly zero** is removed. A non-empty `-wal`/`-log`
/// holds frames that have not been checkpointed into the main file — data — and is never
/// touched, so the sweep is safe by construction rather than by assumption. A sidecar that
/// does not exist is simply skipped. Returns how many files were removed.
///
/// # Errors
///
/// An empty sidecar exists and cannot be removed (a filesystem error).
pub async fn remove_empty_sidecars(db: &Path) -> Result<usize> {
	let mut removed = 0usize;
	for suffix in SIDECAR_SUFFIXES {
		let mut name = db.as_os_str().to_os_string();
		name.push(suffix);
		let sidecar = PathBuf::from(name);
		let Ok(meta) = tokio::fs::metadata(&sidecar).await else { continue };
		if meta.len() == 0 {
			tokio::fs::remove_file(&sidecar).await.with_context(|| format!("removing empty sidecar {}", sidecar.display()))?;
			removed += 1;
		}
	}
	Ok(removed)
}

/// Fail unless every one of `expected` is among `found`, the user tables of the copy at
/// `dest`. An empty or truncated-to-the-header database opens and scans cleanly, so
/// without this an empty `catalog.db` would verify as a backup of a populated one.
fn require_tables(dest: &Path, found: &[String], expected: &[&str]) -> Result<()> {
	let missing: Vec<&str> = expected.iter().copied().filter(|table| !found.iter().any(|f| f == table)).collect();
	if !missing.is_empty() {
		bail!("backup copy {} is missing the table(s) {missing:?} it must hold (it has {found:?}): not a complete snapshot", dest.display());
	}
	Ok(())
}

/// Verify a snapshot **on its own terms** (the [`VerifyMode::SnapshotOnly`] check):
/// reopen `dest`, require each of `expected_tables`, enumerate its user tables, and
/// fully scan each one.
///
/// Reads nothing from the source, so it is correct while writers are committing to the
/// source — the verification an online/background backup needs. Returns a
/// [`SnapshotReport`] whose `rows` is the copy's own total. The empty `-wal`/`-log`
/// sidecars the reopen leaves beside the copy are swept afterwards
/// ([`remove_empty_sidecars`]).
///
/// `expected_tables` is the database's own table set (for a control-plane database,
/// its store's `TABLES`, as [`expected_tables`] returns). A copy may hold more tables
/// than that (a newer schema), never fewer.
///
/// # Errors
///
/// - The copy will not open or connect.
/// - Any of `expected_tables` is missing (an empty or wrong database).
/// - Any table cannot be enumerated or fully read (a truncated/corrupt copy).
/// - The snapshot file cannot be stat'd, or an empty sidecar cannot be removed.
pub async fn verify_snapshot(dest: &Path, expected_tables: &[&str]) -> Result<SnapshotReport> {
	let dest_db = Builder::new_local(dest.to_str().unwrap_or_default()).build().await.with_context(|| format!("reopening backup copy {}", dest.display()))?;
	let dest_conn = dest_db.connect().with_context(|| format!("connecting to backup copy {}", dest.display()))?;

	let tables = user_tables(&dest_conn).await.context("enumerating backup-copy tables")?;
	require_tables(dest, &tables, expected_tables)?;
	let mut rows = 0i64;
	for table in &tables {
		rows += scan_rows(&dest_conn, table).await.with_context(|| format!("scanning backup-copy rows in {table}"))?;
	}
	drop(dest_conn);
	drop(dest_db);
	remove_empty_sidecars(dest).await?;

	let bytes = tokio::fs::metadata(dest).await.with_context(|| format!("stat backup copy {}", dest.display()))?.len();
	Ok(SnapshotReport { dest: dest.to_path_buf(), tables: tables.len(), table_names: tables, rows, bytes, mode: VerifyMode::SnapshotOnly })
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
/// the quiescence scope of the row-count match. No particular table is required; use
/// [`snapshot_with_verify`] to name the tables a database must hold.
///
/// # Errors
///
/// - [`vacuum_into`] fails (bad/existing destination, libSQL error).
/// - The copy will not open, or its table set / any table's row count differs from the
///   source.
pub async fn snapshot_and_verify(src: &turso::Connection, dest: &Path) -> Result<SnapshotReport> {
	snapshot_with_verify(src, dest, VerifyMode::SourceMatch, &[]).await
}

/// Snapshot the database reachable through `src` to `dest`, then verify the copy under
/// the requested [`VerifyMode`], requiring each of `expected_tables` in it.
///
/// [`VerifyMode::SourceMatch`] cross-checks the copy against a fresh source read (sound
/// only on a quiescent source); [`VerifyMode::SnapshotOnly`] validates the copy alone
/// via [`verify_snapshot`] and is correct under concurrent writes. See the module docs.
///
/// # Errors
///
/// - [`vacuum_into`] fails (bad/existing destination, libSQL error).
/// - The copy will not open, cannot be fully read, lacks one of `expected_tables`, or
///   (under `SourceMatch`) its table set / any table's row count differs from the
///   source.
pub async fn snapshot_with_verify(src: &turso::Connection, dest: &Path, mode: VerifyMode, expected_tables: &[&str]) -> Result<SnapshotReport> {
	vacuum_into(src, dest).await?;
	match mode {
		VerifyMode::SnapshotOnly => verify_snapshot(dest, expected_tables).await,
		VerifyMode::SourceMatch => verify_against_source(src, dest, expected_tables).await,
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
async fn verify_against_source(src: &turso::Connection, dest: &Path, expected_tables: &[&str]) -> Result<SnapshotReport> {
	let dest_db = Builder::new_local(dest.to_str().unwrap_or_default()).build().await.with_context(|| format!("reopening backup copy {}", dest.display()))?;
	let dest_conn = dest_db.connect().with_context(|| format!("connecting to backup copy {}", dest.display()))?;

	let src_tables = user_tables(src).await.context("enumerating source tables")?;
	let dest_tables = user_tables(&dest_conn).await.context("enumerating backup-copy tables")?;
	if src_tables != dest_tables {
		bail!("backup copy table set differs from source: source {src_tables:?} vs copy {dest_tables:?}");
	}
	require_tables(dest, &dest_tables, expected_tables)?;

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
	remove_empty_sidecars(dest).await?;

	let bytes = tokio::fs::metadata(dest).await.with_context(|| format!("stat backup copy {}", dest.display()))?.len();
	Ok(SnapshotReport { dest: dest.to_path_buf(), tables: src_tables.len(), table_names: src_tables, rows, bytes, mode: VerifyMode::SourceMatch })
}

/// The four control-plane database file names.
///
/// These are what a [`ControlPlaneBackup`](crate::ControlPlaneBackup) writes and what a
/// store root holds — one source of truth for both directions, so a restore can never
/// disagree with the backup about what the control plane consists of.
pub const CONTROL_PLANE_FILES: [&str; 4] = ["segment_index.db", "metadata.db", "aspect_catalog.db", "catalog.db"];

/// The tables the control-plane database file `file` (one of [`CONTROL_PLANE_FILES`])
/// must hold: its store's `TABLES`. `None` for any other name.
#[must_use]
pub fn expected_tables(file: &str) -> Option<&'static [&'static str]> {
	match file {
		"segment_index.db" => Some(crate::SegmentIndexStore::TABLES),
		"metadata.db" => Some(crate::AspectMetadataStore::TABLES),
		"aspect_catalog.db" => Some(crate::AspectCatalog::TABLES),
		"catalog.db" => Some(crate::CatalogStore::TABLES),
		_ => None,
	}
}

/// The file a backup directory is published with, written last (and fsynced) before
/// the directory is renamed to its final name: its presence is what makes a backup
/// directory complete.
pub const BACKUP_MANIFEST: &str = "MANIFEST.json";

/// The [`BackupManifest::format`] of a control-plane backup. A restore refuses any
/// other, so a backup a newer release writes (with frames, S16) is never restored as
/// if it were control plane only.
pub const MANIFEST_FORMAT: &str = "weftdb-control-plane-backup/1";

/// Prefix of the directory a backup is built in: `.partial-{label}-{nonce}`.
pub const PARTIAL_PREFIX: &str = ".partial-";
/// Prefix a backup is renamed to before retention removes its files:
/// `.deleting-{label}-{nonce}`.
pub const DELETING_PREFIX: &str = ".deleting-";
/// Prefix of the throwaway directory a restore drill restores into.
pub const RESTORE_DRILL_PREFIX: &str = ".restore-drill-";
/// The names only an unfinished backup, prune or drill leaves behind.
///
/// An entry under a backup base whose name starts with one of these is never a backup:
/// it is never counted, listed or restored, and [`sweep_backup_staging`] removes it once
/// stale.
pub const STAGING_PREFIXES: [&str; 3] = [PARTIAL_PREFIX, DELETING_PREFIX, RESTORE_DRILL_PREFIX];

/// How long the daemon leaves a staging entry untouched before sweeping it: one hour.
///
/// Long enough that a backup or drill still running (a manual one, beside the daemon)
/// is never swept from under itself, short enough that a crash's litter does not
/// linger.
pub const STAGING_SWEEP_AGE: Duration = Duration::from_hours(1);

/// Whether `name` is reserved for a staging entry (see [`STAGING_PREFIXES`]).
#[must_use]
pub fn is_staging_name(name: &str) -> bool {
	STAGING_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Whether `dir`'s own name is reserved for a staging entry.
fn is_staging_dir(dir: &Path) -> bool {
	dir.file_name().and_then(OsStr::to_str).is_some_and(is_staging_name)
}

/// One snapshot file of a [`BackupManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
	/// The file name, one of [`CONTROL_PLANE_FILES`].
	pub name: String,
	/// Its size in bytes when the backup was published.
	pub bytes: u64,
	/// The user tables the snapshot held, in name order.
	pub tables: Vec<String>,
	/// The rows its verification read.
	pub rows: i64,
}

/// The `MANIFEST.json` a published backup directory carries: what was verified, so a
/// restore can check the files are still the ones the backup wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
	/// [`MANIFEST_FORMAT`] for a backup this release writes.
	pub format: String,
	/// When the backup was taken, in Unix milliseconds.
	pub created_ms: u64,
	/// Every snapshot file in the directory.
	pub files: Vec<ManifestFile>,
}

impl BackupManifest {
	/// The manifest for snapshots verified as `reports`, each `(file name, report)`.
	#[must_use]
	pub fn new(created_ms: u64, reports: &[(&str, &SnapshotReport)]) -> Self {
		let files = reports.iter().map(|(name, report)| ManifestFile { name: (*name).to_string(), bytes: report.bytes, tables: report.table_names.clone(), rows: report.rows }).collect();
		Self { format: MANIFEST_FORMAT.to_string(), created_ms, files }
	}

	/// Read `dir`'s manifest, or `None` if it has none (a backup from before manifests).
	///
	/// # Errors
	///
	/// The manifest exists but cannot be read, does not parse, names a format this
	/// release does not know, or does not list exactly the [`CONTROL_PLANE_FILES`].
	pub async fn read(dir: &Path) -> Result<Option<Self>> {
		let path = dir.join(BACKUP_MANIFEST);
		let bytes = match tokio::fs::read(&path).await {
			Ok(bytes) => bytes,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
		};
		let manifest: Self = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
		if manifest.format != MANIFEST_FORMAT {
			bail!("{} declares format {:?}; this release restores only {MANIFEST_FORMAT:?}", path.display(), manifest.format);
		}
		let listed: BTreeSet<&str> = manifest.files.iter().map(|file| file.name.as_str()).collect();
		if listed.len() != manifest.files.len() || listed != CONTROL_PLANE_FILES.into_iter().collect::<BTreeSet<&str>>() {
			bail!("{} lists {listed:?}, not exactly the control-plane files {CONTROL_PLANE_FILES:?}", path.display());
		}
		Ok(Some(manifest))
	}

	fn file(&self, name: &str) -> Option<&ManifestFile> {
		self.files.iter().find(|file| file.name == name)
	}
}

/// Whether `path` is a regular file (following symlinks), `false` if nothing is there.
async fn is_file(path: &Path) -> io::Result<bool> {
	match tokio::fs::metadata(path).await {
		Ok(meta) => Ok(meta.is_file()),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
		Err(e) => Err(e),
	}
}

/// Whether `dir` holds a complete backup: the test retention counts by.
///
/// A complete backup carries a [`BACKUP_MANIFEST`] (written last, so a backup that has
/// one was fully written and verified), or, from before manifests, all four
/// [`CONTROL_PLANE_FILES`]. A directory with a staging name is never complete, whatever
/// it holds.
///
/// This is a cheap structural test. [`restore_control_plane`] still verifies every
/// file before it trusts one.
///
/// # Errors
///
/// A stat failure other than `NotFound`.
pub async fn is_complete_backup(dir: &Path) -> io::Result<bool> {
	if is_staging_dir(dir) {
		return Ok(false);
	}
	if is_file(&dir.join(BACKUP_MANIFEST)).await? {
		return Ok(true);
	}
	for name in CONTROL_PLANE_FILES {
		if !is_file(&dir.join(name)).await? {
			return Ok(false);
		}
	}
	Ok(true)
}

/// `dir`'s parent and its own name, for building a sibling of it. A bare name's parent
/// is the current directory.
pub(crate) fn split_dir(dir: &Path) -> io::Result<(PathBuf, OsString)> {
	let name = dir.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{} does not end in a directory name", dir.display())))?;
	let parent = dir.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
	Ok((parent.to_path_buf(), name.to_os_string()))
}

/// `{prefix}{name}-{nonce}`: a staging name that no other run can be using.
pub(crate) fn staging_name(prefix: &str, name: &OsStr) -> OsString {
	let mut staged = OsString::from(prefix);
	staged.push(name);
	staged.push(format!("-{:016x}", fastrand::u64(..)));
	staged
}

/// Remove the backup directory `dir` without ever leaving it half-removed under a backup's
/// name.
///
/// It is renamed to `.deleting-{name}-{nonce}`, the base is fsynced so the rename is
/// durable, and only then are its files removed. A crash after the rename leaves a
/// `.deleting-*` entry, which is never counted or restored and which
/// [`sweep_backup_staging`] removes; removing in place could leave a backup name holding
/// some of its files (design section 9, `prune-partial-remove`).
///
/// # Errors
///
/// Any rename, sync or removal error, or an injected fault at `prune-renamed` /
/// `prune-removed`. The backup is then either intact under its name or under a
/// `.deleting-*` name.
pub async fn retire_backup(fs: &dyn StoreFs, dir: &Path) -> io::Result<()> {
	let (base, name) = split_dir(dir)?;
	let doomed = base.join(staging_name(DELETING_PREFIX, &name));
	fs.rename(dir, &doomed).await?;
	fault::hit(FaultPoint::PruneRenamed).await?;
	fs.sync_dir(&base).await?;
	fs.remove_dir_all(&doomed).await?;
	fault::hit(FaultPoint::PruneRemoved).await?;
	Ok(())
}

/// What [`sweep_backup_staging`] did.
#[derive(Debug, Default)]
pub struct StagingSweep {
	/// The staging entries removed.
	pub removed: Vec<PathBuf>,
	/// The stale staging entries that could not be removed, each with its error. The
	/// sweep carries on past them, and the next sweep retries.
	pub failed: Vec<(PathBuf, io::Error)>,
}

/// The newest modification time of `path` and, for a directory, of its entries: a
/// build still writing into a big snapshot file updates the file's mtime but not its
/// directory's, and must not look stale.
async fn newest_mtime(path: &Path) -> io::Result<SystemTime> {
	let meta = tokio::fs::symlink_metadata(path).await?;
	let mut newest = meta.modified()?;
	if meta.is_dir() {
		let mut entries = tokio::fs::read_dir(path).await?;
		while let Some(entry) = entries.next_entry().await? {
			if let Ok(modified) = entry.metadata().await.and_then(|meta| meta.modified()) {
				newest = newest.max(modified);
			}
		}
	}
	Ok(newest)
}

/// Remove every staging entry directly under `base` (see [`STAGING_PREFIXES`]: unfinished
/// backups, prunes and drills) that nothing has modified for at least `older_than`. A
/// missing `base` is an empty one.
///
/// The age guard keeps the sweep from removing a backup or drill that another task is
/// still writing; the daemon passes [`STAGING_SWEEP_AGE`]. An entry is judged by the
/// newest mtime of itself and its direct entries.
///
/// # Errors
///
/// Only a failure to list `base`. Failures on single entries are reported in
/// [`StagingSweep::failed`].
pub async fn sweep_backup_staging(fs: &dyn StoreFs, base: &Path, older_than: Duration) -> io::Result<StagingSweep> {
	let entries = match fs.read_dir(base).await {
		Ok(entries) => entries,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StagingSweep::default()),
		Err(e) => return Err(e),
	};
	let now = SystemTime::now();
	let mut sweep = StagingSweep::default();
	for entry in entries {
		if !entry.name.to_str().is_some_and(is_staging_name) {
			continue;
		}
		let path = base.join(&entry.name);
		let stale = match newest_mtime(&path).await {
			// A clock that moved backwards makes the entry look new: keep it.
			Ok(modified) => now.duration_since(modified).is_ok_and(|age| age >= older_than),
			// Gone already: another sweep or prune finished it.
			Err(e) if e.kind() == io::ErrorKind::NotFound => false,
			Err(e) => {
				sweep.failed.push((path, e));
				continue;
			}
		};
		if !stale {
			continue;
		}
		let removed = if entry.is_dir { fs.remove_dir_all(&path).await } else { fs.remove_file(&path).await };
		match removed {
			Ok(()) => sweep.removed.push(path),
			Err(e) => sweep.failed.push((path, e)),
		}
	}
	Ok(sweep)
}

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
/// The `.weftseg` measurement frames under `segments/` are **not** part of this (the backup
/// is control-plane only, per hard-constraint #3): restoring into a root whose `segments/`
/// still holds the frames reconstitutes a working store, which is the intended
/// control-plane-corruption recovery. A restore into an empty root yields a valid but
/// frame-less store whose index rows point at missing files.
///
/// See [`restore_control_plane_with`] for what is checked and in what order.
///
/// # Errors
///
/// As [`restore_control_plane_with`].
pub async fn restore_control_plane(backup_dir: &Path, root: &Path) -> Result<RestoreReport> {
	restore_control_plane_with(&RealFs, backup_dir, root).await
}

/// [`restore_control_plane`] through `fs`.
///
/// A directory with a [`BACKUP_MANIFEST`] must match it: exactly the control-plane
/// files, each the recorded size, each holding the recorded tables and rows. One without
/// a manifest (from before manifests) must hold all four files. Either way each file
/// must hold its store's expected tables, so an empty database fails. A directory with
/// a staging name (see [`STAGING_PREFIXES`]) is refused outright: it is an unfinished
/// backup, prune or drill, whatever it holds.
///
/// Every file is copied to `<name>.tmp` in `root` and synced through the handle that
/// wrote it, then verified there; only once all four verify are they renamed to their
/// final names, after which `root` is fsynced. So a failure or crash part-way leaves
/// only `.tmp` files (a retried restore replaces them), never a partial control plane
/// under the names a store opens.
///
/// # Errors
///
/// - `backup_dir` has a staging name, or is missing any of the four files.
/// - Its manifest cannot be read, or the files do not match it.
/// - Any target file already exists in `root`.
/// - A copy, rename or sync fails, or any restored file fails verification.
pub async fn restore_control_plane_with(fs: &dyn StoreFs, backup_dir: &Path, root: &Path) -> Result<RestoreReport> {
	if is_staging_dir(backup_dir) {
		bail!("{} is an unfinished backup, prune or drill (its name starts with one of {STAGING_PREFIXES:?}), not a backup", backup_dir.display());
	}
	let manifest = BackupManifest::read(backup_dir).await?;
	// Pre-flight both directions before writing anything, so a partial restore cannot
	// leave a half-populated control plane behind.
	for name in CONTROL_PLANE_FILES {
		let src = backup_dir.join(name);
		if !is_file(&src).await.with_context(|| format!("checking {}", src.display()))? {
			bail!("backup dir {} is missing {name} — not a complete control-plane backup", backup_dir.display());
		}
		if let Some(file) = manifest.as_ref().and_then(|manifest| manifest.file(name)) {
			let len = tokio::fs::metadata(&src).await.with_context(|| format!("checking {}", src.display()))?.len();
			if len != file.bytes {
				bail!("backup file {} holds {len} bytes but its manifest recorded {}: the backup was changed after it was taken", src.display(), file.bytes);
			}
		}
		let dest = root.join(name);
		if dest.exists() {
			bail!("refusing to overwrite an existing control plane: {} already exists (restore into a fresh root)", dest.display());
		}
	}
	tokio::fs::create_dir_all(root).await.with_context(|| format!("creating restore root {}", root.display()))?;

	let mut staged = Vec::with_capacity(CONTROL_PLANE_FILES.len());
	for (k, name) in (0u32..).zip(CONTROL_PLANE_FILES) {
		let src = backup_dir.join(name);
		let tmp = root.join(format!("{name}.tmp"));
		// A restore that crashed may have left this name behind.
		fs.remove_file(&tmp).await.with_context(|| format!("clearing {}", tmp.display()))?;
		fs.copy_new(&src, &tmp, SyncPolicy::Full).await.with_context(|| format!("restoring {} -> {}", src.display(), tmp.display()))?;
		fault::hit(FaultPoint::RestoreCopied(k)).await?;
		// Verify the copy, not the source: what matters is that the file the store will
		// open is readable. The rename below keeps the very same file.
		let recorded = manifest.as_ref().and_then(|manifest| manifest.file(name));
		let mut expected: Vec<&str> = expected_tables(name).unwrap_or_default().to_vec();
		expected.extend(recorded.iter().flat_map(|file| file.tables.iter().map(String::as_str)));
		let report = verify_snapshot(&tmp, &expected).await.with_context(|| format!("verifying restored {name}"))?;
		if let Some(file) = recorded {
			if report.rows != file.rows {
				bail!("restored {name} holds {} rows but its manifest recorded {}", report.rows, file.rows);
			}
		}
		staged.push((tmp, root.join(name), report));
	}
	let mut restored = Vec::with_capacity(staged.len());
	for (tmp, dest, mut report) in staged {
		fs.rename(&tmp, &dest).await.with_context(|| format!("renaming {} -> {}", tmp.display(), dest.display()))?;
		report.dest = dest;
		restored.push(report);
	}
	fault::hit(FaultPoint::RestoreRenamed).await?;
	fs.sync_dir(root).await.with_context(|| format!("syncing restore root {}", root.display()))?;
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

		let report = snapshot_with_verify(&conn, &dest, VerifyMode::SnapshotOnly, &["gadget", "widget"]).await.unwrap();
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
		let report = verify_snapshot(&online, &["widget"]).await.unwrap();
		assert_eq!(report.rows, 10, "the snapshot holds its point-in-time image, not the grown source");
		assert_eq!(count_rows(&conn, "widget").await.unwrap(), 17, "the source really did move on");

		// And the source-matching verify is exactly what would have failed here.
		let stale = dir.path().join("stale.db");
		let err = snapshot_with_verify(&conn, &stale, VerifyMode::SourceMatch, &["widget"]).await;
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
		assert!(verify_snapshot(&bogus, &[]).await.is_err(), "a non-database file must fail verification");
	}

	/// The two sidecar paths Turso creates beside `db` on open.
	fn sidecars(db: &Path) -> [PathBuf; 2] {
		let mk = |suffix: &str| {
			let mut name = db.as_os_str().to_os_string();
			name.push(suffix);
			PathBuf::from(name)
		};
		[mk("-wal"), mk("-log")]
	}

	#[tokio::test]
	async fn reopening_a_snapshot_really_does_leave_empty_sidecars() {
		// The premise of the sweep, pinned so the fix cannot silently become a no-op if
		// Turso ever changes its sidecar naming: a vacuumed-then-reopened copy has zero-byte
		// `-wal` / `-log` files beside it. (Measured: `VACUUM INTO` itself already creates
		// the empty `-wal`; the verifying reopen adds the `-log`. Either way the sweep runs
		// after verification, so it catches both.)
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;
		let dest = dir.path().join("probe.db");
		vacuum_into(&conn, &dest).await.unwrap();
		for sidecar in sidecars(&dest).into_iter().filter(|p| p.exists()) {
			assert_eq!(std::fs::metadata(&sidecar).unwrap().len(), 0, "a sidecar a bare VACUUM INTO creates is empty: {}", sidecar.display());
		}

		let db = Builder::new_local(dest.to_str().unwrap()).build().await.unwrap();
		let c = db.connect().unwrap();
		assert_eq!(count_rows(&c, "widget").await.unwrap(), 7);
		drop(c);
		drop(db);
		let litter: Vec<_> = sidecars(&dest).into_iter().filter(|p| p.exists()).collect();
		assert!(!litter.is_empty(), "the reopen leaves at least one sidecar — the litter the sweep exists for");
		for sidecar in &litter {
			assert_eq!(std::fs::metadata(sidecar).unwrap().len(), 0, "{} is empty", sidecar.display());
		}
		assert_eq!(remove_empty_sidecars(&dest).await.unwrap(), litter.len(), "the sweep removes exactly the litter");
		for sidecar in sidecars(&dest) {
			assert!(!sidecar.exists(), "{} is gone", sidecar.display());
		}
		assert!(dest.exists(), "the snapshot itself is untouched");
	}

	#[tokio::test]
	async fn both_verify_modes_leave_no_empty_sidecars_behind() {
		let dir = tempfile::tempdir().unwrap();
		let conn = seed_db(&dir.path().join("src.db")).await;

		let snapshot_only = dir.path().join("snapshot_only.db");
		let report = snapshot_with_verify(&conn, &snapshot_only, VerifyMode::SnapshotOnly, &[]).await.unwrap();
		assert_eq!(report.rows, 10);
		for sidecar in sidecars(&snapshot_only) {
			assert!(!sidecar.exists(), "SnapshotOnly left {} behind", sidecar.display());
		}

		let source_match = dir.path().join("source_match.db");
		let report = snapshot_with_verify(&conn, &source_match, VerifyMode::SourceMatch, &[]).await.unwrap();
		assert_eq!(report.rows, 10);
		for sidecar in sidecars(&source_match) {
			assert!(!sidecar.exists(), "SourceMatch left {} behind", sidecar.display());
		}

		// The backup directory holds exactly the two snapshots: nothing else was left in it.
		let mut entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n != "src.db" && !n.starts_with("src.db-")).collect();
		entries.sort();
		assert_eq!(entries, vec!["snapshot_only.db".to_string(), "source_match.db".to_string()]);
	}

	#[tokio::test]
	async fn the_sweep_never_removes_a_non_empty_sidecar() {
		// A `-wal` with bytes in it holds un-checkpointed frames — data. The sweep must
		// leave it alone even though it sits exactly where litter would.
		let dir = tempfile::tempdir().unwrap();
		let db = dir.path().join("live.db");
		tokio::fs::write(&db, b"main file").await.unwrap();
		let [wal, log] = sidecars(&db);
		tokio::fs::write(&wal, b"frames that were never checkpointed").await.unwrap();
		tokio::fs::write(&log, b"").await.unwrap();

		assert_eq!(remove_empty_sidecars(&db).await.unwrap(), 1, "only the empty one goes");
		assert!(wal.exists(), "the non-empty WAL survives");
		assert_eq!(tokio::fs::read(&wal).await.unwrap(), b"frames that were never checkpointed");
		assert!(!log.exists(), "the empty log is removed");
		// Idempotent: a second sweep finds nothing to do.
		assert_eq!(remove_empty_sidecars(&db).await.unwrap(), 0);
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

	/// The regression for `backup-vacuum-into-partial-dest`: a database file that opens
	/// but holds none of its tables (an empty file, or a copy torn back to its header)
	/// scanned as a valid snapshot of zero rows, so an empty `catalog.db` verified.
	#[tokio::test]
	async fn an_empty_database_fails_verification_against_its_expected_tables() {
		let dir = tempfile::tempdir().unwrap();
		let empty = dir.path().join("catalog.db");
		let db = Builder::new_local(empty.to_str().unwrap()).build().await.unwrap();
		drop(db.connect().unwrap());
		drop(db);

		let err = verify_snapshot(&empty, crate::CatalogStore::TABLES).await.unwrap_err();
		assert!(format!("{err:#}").contains("missing the table(s)"), "got: {err:#}");
		assert!(verify_snapshot(&empty, &[]).await.is_ok(), "only the expected table set tells an empty catalog from a valid one");

		// The source-matching mode requires them too, even when the source is as empty.
		let conn = Builder::new_local(dir.path().join("src.db").to_str().unwrap()).build().await.unwrap().connect().unwrap();
		let err = snapshot_with_verify(&conn, &dir.path().join("copy.db"), VerifyMode::SourceMatch, crate::CatalogStore::TABLES).await.unwrap_err();
		assert!(format!("{err:#}").contains("missing the table(s)"), "got: {err:#}");
	}

	#[test]
	fn every_control_plane_file_has_an_expected_table_set() {
		for name in CONTROL_PLANE_FILES {
			assert!(expected_tables(name).is_some_and(|tables| !tables.is_empty()), "{name}");
		}
		assert_eq!(expected_tables("other.db"), None);
		for name in [".partial-backup-1-00ff", ".deleting-backup-1-00ff", ".restore-drill-17"] {
			assert!(is_staging_name(name), "{name}");
		}
		for name in ["backup-1", "nightly", ".hidden", "partial-1"] {
			assert!(!is_staging_name(name), "{name}");
		}
	}

	#[tokio::test]
	async fn a_manifest_must_name_a_known_format_and_exactly_the_control_plane_files() {
		let dir = tempfile::tempdir().unwrap();
		assert_eq!(BackupManifest::read(dir.path()).await.unwrap(), None, "no manifest: a legacy backup");

		let report = |name: &str| SnapshotReport { dest: dir.path().join(name), tables: 1, table_names: vec!["t".into()], rows: 3, bytes: 4096, mode: VerifyMode::SnapshotOnly };
		let reports: Vec<_> = CONTROL_PLANE_FILES.iter().map(|name| report(name)).collect();
		let named: Vec<_> = CONTROL_PLANE_FILES.iter().copied().zip(reports.iter()).collect();
		let manifest = BackupManifest::new(42, &named);
		std::fs::write(dir.path().join(BACKUP_MANIFEST), serde_json::to_vec(&manifest).unwrap()).unwrap();
		assert_eq!(BackupManifest::read(dir.path()).await.unwrap(), Some(manifest.clone()), "it round-trips");

		let mut future = manifest.clone();
		future.format = "weftdb-store-backup/2".into();
		std::fs::write(dir.path().join(BACKUP_MANIFEST), serde_json::to_vec(&future).unwrap()).unwrap();
		assert!(BackupManifest::read(dir.path()).await.is_err(), "an unknown format is refused, not restored as control plane only");

		let mut short = manifest;
		short.files.pop();
		std::fs::write(dir.path().join(BACKUP_MANIFEST), serde_json::to_vec(&short).unwrap()).unwrap();
		assert!(BackupManifest::read(dir.path()).await.is_err(), "a manifest missing a file is refused");

		std::fs::write(dir.path().join(BACKUP_MANIFEST), b"{\"format\": \"weftdb-control-pl").unwrap();
		assert!(BackupManifest::read(dir.path()).await.is_err(), "a torn manifest is refused");
	}

	#[tokio::test]
	async fn completeness_needs_a_manifest_or_all_four_files_and_never_a_staging_name() {
		let dir = tempfile::tempdir().unwrap();
		let legacy = dir.path().join("backup-1");
		std::fs::create_dir(&legacy).unwrap();
		for name in &CONTROL_PLANE_FILES[..3] {
			std::fs::write(legacy.join(name), b"x").unwrap();
		}
		assert!(!is_complete_backup(&legacy).await.unwrap(), "three of four files: a pre-manifest backup that never finished");
		std::fs::write(legacy.join(CONTROL_PLANE_FILES[3]), b"x").unwrap();
		assert!(is_complete_backup(&legacy).await.unwrap(), "all four files: a complete pre-manifest backup");

		let manifested = dir.path().join("backup-2");
		std::fs::create_dir(&manifested).unwrap();
		std::fs::write(manifested.join(BACKUP_MANIFEST), b"{}").unwrap();
		assert!(is_complete_backup(&manifested).await.unwrap(), "the manifest is written last, so it marks a complete backup");

		let partial = dir.path().join(".partial-backup-3-0123456789abcdef");
		std::fs::rename(&manifested, &partial).unwrap();
		assert!(!is_complete_backup(&partial).await.unwrap(), "a staging directory is never a backup, whatever it holds");
		assert!(!is_complete_backup(&dir.path().join("missing")).await.unwrap());
	}

	#[tokio::test]
	async fn retiring_a_backup_renames_it_away_before_removing_it() {
		let dir = tempfile::tempdir().unwrap();
		let victim = dir.path().join("backup-1");
		std::fs::create_dir(&victim).unwrap();
		std::fs::write(victim.join("catalog.db"), b"x").unwrap();
		std::fs::create_dir(dir.path().join("backup-2")).unwrap();

		// Stop it between the rename and the removal, as a crash would.
		let armed = fault::arm(FaultPoint::PruneRenamed, fault::FaultAction::ReturnErr);
		let err = retire_backup(&RealFs, &victim).await.unwrap_err();
		drop(armed);
		assert_eq!(fault::injected_point(&err), Some(FaultPoint::PruneRenamed));
		let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
		assert!(!victim.exists(), "the backup name is gone before any file is removed");
		assert!(names.iter().any(|n| n.starts_with(".deleting-backup-1-")), "{names:?}");

		let swept = sweep_backup_staging(&RealFs, dir.path(), Duration::ZERO).await.unwrap();
		assert_eq!(swept.removed.len(), 1, "the sweep finishes the interrupted prune");
		assert!(swept.failed.is_empty());
		let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
		assert_eq!(names, vec!["backup-2".to_string()], "only the other backup is left");

		// Stopped after the removal, before anything else: nothing is left to sweep.
		let armed = fault::arm(FaultPoint::PruneRemoved, fault::FaultAction::ReturnErr);
		let err = retire_backup(&RealFs, &dir.path().join("backup-2")).await.unwrap_err();
		drop(armed);
		assert_eq!(fault::injected_point(&err), Some(FaultPoint::PruneRemoved));
		assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "the backup and its `.deleting-*` name are both gone");

		std::fs::create_dir(dir.path().join("backup-3")).unwrap();
		retire_backup(&RealFs, &dir.path().join("backup-3")).await.unwrap();
		assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "an uninterrupted retire leaves nothing");
	}

	#[tokio::test]
	async fn the_sweep_removes_only_stale_staging_entries() {
		let dir = tempfile::tempdir().unwrap();
		let base = dir.path();
		for name in [".partial-backup-1-00", ".deleting-backup-2-00", ".restore-drill-3", "backup-4", "nightly"] {
			std::fs::create_dir(base.join(name)).unwrap();
			std::fs::write(base.join(name).join("catalog.db"), b"x").unwrap();
		}
		std::fs::write(base.join(".partial-stray-file"), b"x").unwrap();

		let swept = sweep_backup_staging(&RealFs, base, STAGING_SWEEP_AGE).await.unwrap();
		assert!(swept.removed.is_empty() && swept.failed.is_empty(), "nothing is an hour old yet: a backup or drill may still be writing it");

		// Back-date one staging directory past the age, including the file inside it,
		// whose mtime counts too.
		let old = SystemTime::now() - 2 * STAGING_SWEEP_AGE;
		let stale = base.join(".partial-backup-1-00");
		std::fs::File::open(stale.join("catalog.db")).unwrap().set_modified(old).unwrap();
		std::fs::File::open(&stale).unwrap().set_modified(old).unwrap();
		let swept = sweep_backup_staging(&RealFs, base, STAGING_SWEEP_AGE).await.unwrap();
		assert_eq!(swept.removed, vec![stale.clone()]);
		assert!(!stale.exists());

		let swept = sweep_backup_staging(&RealFs, base, Duration::ZERO).await.unwrap();
		assert_eq!(swept.removed.len(), 3, "every other staging entry, file or directory");
		let mut left: Vec<String> = std::fs::read_dir(base).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
		left.sort();
		assert_eq!(left, vec!["backup-4".to_string(), "nightly".to_string()], "backups are never swept");
		assert!(sweep_backup_staging(&RealFs, &base.join("missing"), Duration::ZERO).await.unwrap().removed.is_empty(), "a missing base is empty");
	}
}
