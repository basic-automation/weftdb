//! Reading a released 0.1.x **rows-mode** database into the segment store (roadmap
//! **FRE-14b**).
//!
//! 0.1.x kept a measurement history per aspect in its own libSQL database, and this
//! build refused such a tree at the open (FRE-14a, whose probe is `rows_mode_aspects` in
//! `weftdb/src/types/store_format.rs`): the engine never opens Turso, so nothing in it could
//! read those files. This module is the leg that does read them — the input FRE-14 always had
//! to take over.
//!
//! ## What a rows-mode tree is
//!
//! `compat/v010/README.md` records the layout as the generator leaves it:
//!
//! ```text
//! <database>/
//! ├── metadata.db                      # subjects, aspects, the database row — turso, not SQLite
//! └── <subject>/<aspect>/
//!     ├── measurements.db              # the series: id, dataset_id, timestamp, value
//!     ├── dictionaries/
//!     └── pipeline.db correlations.db patterns.db processed_*.db unprocessed_*.db
//! ```
//!
//! The series lives in `measurements`, four columns — `id TEXT NOT NULL` (the 0.1.x
//! transaction id of the write that produced the row, `tx_id.as_uuid()`: not a WeftDB id,
//! and not carried), `dataset_id TEXT NOT NULL` (the dataset's UUID, the same string on
//! every row of one dataset — `v0.1.0:weftdb/src/types/database/inputs.rs:136-142` for both
//! columns), `timestamp INTEGER NOT NULL` and `value TEXT NOT NULL` (the decimal written as
//! text). Neither identifier is a measurement, which is why the value column is picked by
//! name rather than by elimination: `dataset_id` is also a legal aspect name. Rows-mode has
//! no null value: the fixture generator **skips** a grid row with no value rather than
//! writing a sentinel, and says so on stderr, so the tree is the non-null subset of
//! whatever was captured.
//!
//! **The timestamp unit is milliseconds, always.** 0.1.x wrote the column through
//! `timestamp_millis()` (`v0.1.0:weftdb/src/types/database/inputs.rs`, in
//! `capture_measurement` and its siblings), whatever the aspect's recorded `resolution`
//! said: that column describes the sampling grid the aspect was declared with, not the
//! unit of storage. So the integers carry over unchanged and the schema declares
//! [`TimeUnit::Millis`] — no derivation from the values, which would guess wrong for the
//! same reason the release did.
//!
//! ## What this module does with it
//!
//! [`discover`] walks the tree and *reports* it: the series it can read, each one's row
//! count, time span and duplicate timestamps, and everything in the tree it will not
//! carry ([`NotCarried`], with the reason named per path). It opens each
//! `measurements.db` and checks the shape it needs, so a tree that is not what 0.1.x
//! wrote fails there, before any store is opened.
//!
//! [`import_into`] reads one subject's series into a store that is already open: it
//! declares each aspect under the narrowest **lossless** encoding its values fit
//! ([`recommend_encoding`] at tolerance `0` — the migration never trades precision),
//! seals the rows in time order in chunks of `rows_per_segment` (a chunk never splits
//! between two rows with the same timestamp), and writes an [`ImportRecord`] into
//! `store_meta` naming the tree, the counts, the encodings, and what stayed behind.
//!
//! ## What it refuses
//!
//! A tree with no `metadata.db`; a `measurements.db` with no `measurements` table or
//! without a `timestamp`/`value` it can type; a value that is not a decimal; a tree whose
//! own directory is not the database the store is scoped to, which is the only scope 0.1.x
//! recorded anywhere; an aspect this store already declares under a different schema or
//! already holds segments of; and a tree this store has already imported. Every refusal is typed ([`RowsModeError`]) and
//! names the file. The refusal of an already-imported tree is the idempotency the roadmap
//! asks for: the readable leg cannot tell the tree's rows apart from rows already in the
//! store, so a second import of the same tree is a duplicate, not a no-op — and a
//! duplicate is not something it will do quietly.
//!
//! ## Boundaries
//!
//! - **Memory**: one aspect's `(timestamp, value)` columns are held at once, because the
//!   encoding declaration and [`SegmentStore::seal_declared`] both take whole columns. A
//!   bounded-memory pass (survey in SQL, seal from a cursor) is a follow-up, not this slice.
//! - **The tree is not written to, but it is not opened read-only either**: turso may fold
//!   a `-wal` into the database file when it opens one. Nothing here runs a write
//!   statement. An operator who must leave the tree byte-for-byte identical points WeftDB
//!   at a copy — which is what `compat/fixtures/v0.1.0/legacy/` is for.
//! - **The record's write is one `store_meta` row** ([`IMPORT_RECORD_KEY`]) through the
//!   index's transaction path. `MetaSet` replaces rather than compares (its guard check is
//!   `IndexOp::is_guarded`, which covers the row-writing variants and not this one), so two
//!   imports racing
//!   on one store leave the record of whichever wrote last; the segments each sealed are
//!   conflict-guarded inserts, so no measurement is lost or replaced either way.
//! - **A reading that stops partway leaves what crossed.** `ScopeMismatch`, `NoSubject`,
//!   `AlreadyImported` and the catalog refusals all arrive before a byte is written. After
//!   that the aspects are declared and sealed one at a time — there is no transaction across
//!   aspects, since sealing is per aspect — and the record is written last. So a cell that
//!   will not type on the third aspect leaves the first two in the store with no record, and
//!   the next attempt refuses on [`RowsModeError::AspectNotEmpty`] for an aspect it already
//!   holds instead of finishing the job. The shape check in [`discover`] is what makes the
//!   middle failure rare; the stranding itself is 14c's to handle, and it is pinned by
//!   `a_row_that_is_not_a_decimal_stops_the_read_and_names_its_row`.
//! - **Not carried**: the legacy catalog (`metadata.db` — the store declares its own),
//!   the per-aspect `dictionaries/`, and the ingestion/pipeline/correlation/pattern
//!   databases beside the rows. They are listed with their reasons rather than dropped
//!   silently; reading them is the rest of FRE-14's subsystem list, and 1.0 keeps them
//!   behind.
//!
//! [`discover`] deliberately duplicates the path walk of the open's refusal probe: that
//! probe is best-effort by design (an unreadable root is reported as *not* a data
//! directory, so the open fails with its own context), while discovery here has to name
//! what it could not read.

use std::{
	num::NonZeroUsize, path::{Path, PathBuf}, str::FromStr
};

use anyhow::{bail, Context, Result};
use bigdecimal::BigDecimal;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;
use turso::{Builder, Value};
use weft_physical_type::{recommend_encoding, AspectSchema, PhysicalType, TimeUnit};

use crate::{
	aspect_name, types::index_txn::{IndexOp, IndexTxn}, SegmentStore
};

/// The file one aspect's history lives in, under `<subject>/<aspect>/`.
pub const MEASUREMENTS_FILE: &str = "measurements.db";

/// The column every rows-mode row keeps its instant in.
pub const TIMESTAMP_COLUMN: &str = "timestamp";

/// The `store_meta` key an [`ImportRecord`] is written under.
///
/// One row per store, because one store holds one `(database, subject)` scope and a
/// rows-mode tree is read into the store of each subject it carries.
pub const IMPORT_RECORD_KEY: &str = "rows_mode_import";

/// Rows per sealed segment when the caller does not choose.
///
/// Small enough that a migrated aspect starts inside the range the store's own
/// maintenance is shaped for, large enough that a fixture-sized series is one segment.
pub const DEFAULT_ROWS_PER_SEGMENT: usize = 10_000;

/// The name of the unit the migrated timestamps are declared in ([`TimeUnit::Millis`]).
pub const TIMESTAMP_UNIT: &str = "millis";

/// The unit every timestamp an import writes is declared in.
///
/// 0.1.x put milliseconds into `measurements.timestamp` — `capture_measurement` and
/// `capture_new_measurement` both bind `measurement.timestamp().timestamp_millis()` into the insert
/// (`v0.1.0:weftdb/src/types/database/inputs.rs:142`, `:187`), over a column that is only ever
/// `timestamp INTEGER NOT NULL` (`v0.1.0:weftdb/src/types/aspect.rs:475`) — while its HTTP layer
/// served the aspect's declared unit, which the fixture records as `seconds`
/// (`compat/fixtures/v0.1.0/aspects.json`), and so divides the stored number by a thousand. The
/// number we carry is the stored one, and declaring it `Millis` keeps that number meaning what it
/// says: the unit is declared, never inferred from how big the numbers happen to look.
///
/// The trap to keep out of this module and out of 14c: the *other* tree in the same fixture — the
/// Storage-v2 root the same release wrote from the same value grid — is second-scale, because 0.1.x's
/// batch path kept seconds while its rows-mode path kept millis. Copy that unit here and every
/// migrated row lands a factor of a thousand early; apply this one there and every migrated row lands
/// a thousand times late. Both sides produce plausible-looking numbers, which is why both name their
/// unit in code with the line that says so.
pub const TIMESTAMP_TIME_UNIT: TimeUnit = TimeUnit::Millis;

/// Chunk rows for sealing so that rows sharing one instant never straddle a segment.
///
/// 0.1.x let a series hold several rows at the same instant, and this module carries them: they are
/// what [`ImportedAspect::duplicate_timestamps`] counts. The store does not treat them the same way
/// twice over. A range read returns them all, and each segment keeps its own internal duplicates
/// through maintenance — but when two segments' *windows* intersect, `reconcile_segment` folds them
/// into one under `merge_newer_wins`, and at an instant both members hold only the more recently
/// sealed row survives (`weftdb/src/types/segment_store.rs:3666-3672`). So a chunk boundary that cut
/// a group of same-instant rows in two would hand that group to a maintenance pass that drops all but
/// one of them, silently, some time after the import looked clean. Keeping the group in one segment
/// keeps the tree's answer at that instant intact. It costs a segment that is a few rows longer than
/// the caller asked for, which is the cheaper side of the trade.
fn chunks_keeping_instants_together<'a>(rows: &'a [(i64, BigDecimal)], rows_per_segment: usize) -> Vec<&'a [(i64, BigDecimal)]> {
	let mut chunks = Vec::new();
	let mut start = 0usize;
	while start < rows.len() {
		let mut end = (start + rows_per_segment).min(rows.len());
		if end < rows.len() {
			let boundary = rows[end - 1].0;
			while end < rows.len() && rows[end].0 == boundary {
				end += 1;
			}
		}
		chunks.push(&rows[start..end]);
		start = end;
	}
	chunks
}

/// Why a rows-mode tree was refused, or why an import stopped.
///
/// It arrives inside an [`anyhow::Error`]; `downcast_ref::<RowsModeError>()` recovers it
/// whatever context the read added.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
#[non_exhaustive]
pub enum RowsModeError {
	/// The tree root holds no `metadata.db`, which is where 0.1.x kept its subjects, its
	/// aspects and its database row. Without it there is nothing to say the directory is a
	/// database at all rather than a directory that happens to hold `*.db` files.
	#[error("{} is not a 0.1.x database: it holds no `metadata.db`, where 0.1.x kept its subjects, aspects and database row", .root.display())]
	NoCatalog { root: PathBuf },
	/// A `measurements.db` that is not the series database 0.1.x wrote: no `measurements`
	/// table, or none carrying its `timestamp INTEGER NOT NULL` / `value TEXT NOT NULL` pair.
	/// The readable leg asks the database which columns it has instead of assuming, and the
	/// refusal names the columns it did find.
	#[error("{} is not a 0.1.x series database: {}", .path.display(), .detail)]
	NoSeries { path: PathBuf, detail: String },
	/// A row whose `timestamp` is not an integer. 0.1.x wrote `timestamp INTEGER NOT NULL`;
	/// anything else is a database this module does not know how to read, and guessing would
	/// put a wrong instant in the store under a name the operator would trust. A `value` cell
	/// that is not the text of a decimal is [`ValueNotDecimal`](Self::ValueNotDecimal).
	///
	/// `rowid` is the row's position in the read, counting from one — the 0.1.x row id is not
	/// part of what the readable leg carries, and a row that fails is reported by where it
	/// was found. It is 0 when the check that failed was an aggregate over the series rather
	/// than one row.
	#[error("{} row {rowid}: {}", .path.display(), .detail)]
	RowShape { path: PathBuf, rowid: i64, detail: String },
	/// A `value` that does not parse as a decimal. 0.1.x stored `BigDecimal` as its text,
	/// so a value that does not parse is not a value at all.
	#[error("{} row {rowid}: `value` holds {found:?}, which is not a decimal number; 0.1.x wrote the decimal as text", .path.display())]
	ValueNotDecimal { path: PathBuf, rowid: i64, found: String },
	/// The store already holds an [`ImportRecord`] naming this tree. Re-reading it would
	/// add the same rows a second time: the readable leg has no way to tell a migrated row
	/// apart from one it sealed before, so a second run refuses rather than doubling.
	#[error("this store already imported {tree} at {imported_at} ({aspects} aspect(s), {rows} row(s)); the readable leg cannot tell those rows apart from the tree's, so it will not read the same tree twice")]
	AlreadyImported { tree: String, imported_at: String, aspects: usize, rows: u64 },
	/// The aspect already has segments in this store, sealed by something other than this
	/// import. Its rows and the tree's cannot be told apart afterwards, so the import stops
	/// rather than mixing them.
	#[error("aspect {aspect:?} already holds {segments} segment(s) this import did not seal; the readable leg cannot tell the tree's rows apart from those")]
	AspectNotEmpty { aspect: String, segments: usize },
	/// The aspect is already declared in this store under a schema other than the tree's
	/// rows need. Re-declaring it would silently re-encode rows the store already holds.
	#[error("aspect {aspect:?} is declared {declared} in this store, but its rows need {wanted}; the readable leg will not re-declare an aspect it did not create")]
	SchemaConflict { aspect: String, declared: String, wanted: String },
	/// The tree's database directory has one name and the store is scoped to another.
	/// 0.1.x named a database by that directory, and two databases can hold the same
	/// subject name: reading one into the other's store would put its rows where the
	/// operator would look for the other's.
	#[error("{tree:?} is 0.1.x database {tree_database:?}, but this store is scoped to database {store_database:?} (subject {subject:?}); open the store of the database you mean")]
	ScopeMismatch { tree_database: String, store_database: String, subject: String, tree: String },
	/// The tree holds no `measurements.db` two levels down under `subject`, so it has no
	/// series of that subject's to read.
	#[error("{} holds no series of subject {subject:?}: no `{MEASUREMENTS_FILE}` at {subject}/<aspect>/", .tree.display())]
	NoSubject { tree: PathBuf, subject: String },
	/// The tree root has no final directory name to take as the database name — the store's
	/// scope needs one, and 0.1.x named its database by that directory.
	#[error("{} has no directory name to read as the database name", .root.display())]
	NoDatabaseName { root: PathBuf },
}

/// One aspect's series inside a rows-mode tree, as [`discover`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyAspect {
	/// The tree's first level: the 0.1.x subject directory.
	pub subject: String,
	/// The tree's second level, which is the aspect's name — 0.1.x kept no aspect name
	/// inside `measurements.db` (its rows carry the transaction id and the dataset id), so
	/// the directory is the only record of what the series is called.
	pub name: String,
	/// The `measurements.db` itself.
	pub database: PathBuf,
	/// Rows in `measurements`.
	pub rows: u64,
	/// The earliest timestamp, in milliseconds, or [`None`] for an empty series.
	pub first_timestamp: Option<i64>,
	/// The latest timestamp, in milliseconds, or [`None`] for an empty series.
	pub last_timestamp: Option<i64>,
	/// Rows whose timestamp another row of the same series already has — `rows` less the
	/// distinct timestamps. 0.1.x wrote with no unique constraint on `timestamp` and
	/// deduplicated at the application level, so a tree can hold more than one row per
	/// instant; the readable leg carries them as they are, and says how many.
	pub duplicate_timestamps: u64,
}

/// A path in the tree this module will not carry, with the reason named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotCarried {
	/// Tree-relative, with `/` separators.
	pub path: String,
	/// Why it stays behind, in the words of the subsystem that owned it.
	#[serde(rename = "not_carried_because")]
	pub reason: String,
}

/// What [`discover`] read of a rows-mode tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyTree {
	/// The root, as given.
	pub root: PathBuf,
	/// The database name: 0.1.x named a database by its top directory, and that is the
	/// name the store's scope takes. Same-named subjects of two databases stay apart
	/// because they land in stores of two databases.
	pub database: String,
	/// The series the readable leg can read, ordered by `<subject>/<aspect>`.
	pub aspects: Vec<LegacyAspect>,
	/// Everything in the tree it will not carry, ordered by path.
	pub not_carried: Vec<NotCarried>,
}

impl LegacyTree {
	/// The tree's subjects, in the order their series appear.
	#[must_use]
	pub fn subjects(&self) -> Vec<String> {
		let mut seen: Vec<String> = Vec::new();
		for aspect in &self.aspects {
			if !seen.contains(&aspect.subject) {
				seen.push(aspect.subject.clone());
			}
		}
		seen
	}

	/// The series of `subject`, in order.
	#[must_use]
	pub fn for_subject(&self, subject: &str) -> Vec<&LegacyAspect> {
		self.aspects.iter().filter(|aspect| aspect.subject == subject).collect()
	}
}

/// What an import did for one aspect: the counts, the encoding it chose, and the span it
/// wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedAspect {
	/// The aspect's name, which is the tree's directory name.
	pub aspect: String,
	/// Rows read from the tree and sealed into the store.
	pub rows: u64,
	/// Segments sealed for it, each `rows_per_segment` rows or fewer.
	pub segments: u64,
	/// The encoding its values were declared in — the narrowest that held them exactly.
	pub encoding: String,
	/// Its first and last timestamp, in milliseconds; [`None`] only for an empty series.
	pub first_timestamp: Option<i64>,
	pub last_timestamp: Option<i64>,
	/// Rows sharing a timestamp with another row, carried as they are.
	pub duplicate_timestamps: u64,
}

/// The record an import leaves in the store's `store_meta`, naming the tree it read, what
/// it wrote, and what it left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRecord {
	/// The tree, as it was addressed.
	pub source: String,
	/// The store's scope, so the record says which side of a two-database tree it belongs to.
	pub database: String,
	pub subject: String,
	/// The unit every migrated timestamp is declared in ([`TIMESTAMP_UNIT`]).
	pub timestamp_unit: String,
	/// Rows per sealed segment.
	pub rows_per_segment: usize,
	/// What was read, per aspect.
	pub aspects: Vec<ImportedAspect>,
	/// What stayed in the tree, with reasons.
	pub not_carried: Vec<NotCarried>,
	/// When the import finished, RFC 3339 UTC.
	pub imported_at: String,
}

impl ImportRecord {
	/// Rows the import sealed, across every aspect.
	#[must_use]
	pub fn rows(&self) -> u64 {
		self.aspects.iter().map(|aspect| aspect.rows).sum()
	}

	/// Segments the import sealed, across every aspect.
	#[must_use]
	pub fn segments(&self) -> u64 {
		self.aspects.iter().map(|aspect| aspect.segments).sum()
	}
}

/// Reads the tree at `tree` and reports it, without touching a store.
///
/// The root must be a 0.1.x database directory: `metadata.db` at the top, and one
/// database per aspect at `<subject>/<aspect>/measurements.db`. Each of those is opened
/// and asked for the series the readable leg needs, so a file that is not what 0.1.x
/// wrote is refused here, before anything is sealed.
///
/// # Errors
///
/// [`NoCatalog`](RowsModeError::NoCatalog) for a root with no `metadata.db`,
/// [`NoDatabaseName`](RowsModeError::NoDatabaseName) for a root with no directory name to
/// read as the database, [`NoSeries`](RowsModeError::NoSeries) or
/// [`RowShape`](RowsModeError::RowShape) for a database that is not the series layout, and
/// an I/O failure for a directory that cannot be read.
pub async fn discover(tree: &Path) -> Result<LegacyTree> {
	let root = tree.to_path_buf();
	if !tokio::fs::try_exists(&root).await.with_context(|| format!("looking at the tree root {}", root.display()))? {
		bail!("there is no rows-mode tree at {}", root.display());
	}
	if !tokio::fs::metadata(&root).await.with_context(|| format!("looking at the tree root {}", root.display()))?.is_dir() {
		bail!("{} is a file, not a rows-mode tree", root.display());
	}
	if !tokio::fs::try_exists(root.join("metadata.db")).await.with_context(|| format!("looking for the 0.1.x catalog in {}", root.display()))? {
		return Err(anyhow::Error::new(RowsModeError::NoCatalog { root }));
	}
	let database = root.file_name().and_then(|name| name.to_str()).map(str::to_string).ok_or_else(|| anyhow::Error::new(RowsModeError::NoDatabaseName { root: root.clone() }))?;

	let mut aspects = Vec::new();
	let mut not_carried = vec![NotCarried { path: "metadata.db".to_string(), reason: "the 0.1.x catalog: its subjects, its aspects and its database row. The store declares its own catalog, and the readable leg takes the aspect's name from its directory.".to_string() }];
	// Symlinks are passed over, not followed, as in the open's refusal probe: following one
	// would read somebody else's tree into a store the operator scoped to this one.
	for subject in directories(&root).await? {
		let subject_name = subject.file_name().to_string_lossy().into_owned();
		for sibling in files(&subject.path()).await? {
			let name = sibling.file_name().to_string_lossy().into_owned();
			if is_sidecar(&name) || !name.ends_with(".db") {
				continue;
			}
			not_carried.push(NotCarried { path: format!("{subject_name}/{name}"), reason: SUBJECT_LEVEL.to_string() });
		}
		for aspect in directories(&subject.path()).await? {
			let aspect_name = aspect.file_name().to_string_lossy().into_owned();
			let database_path = aspect.path().join(MEASUREMENTS_FILE);
			if !tokio::fs::try_exists(&database_path).await.with_context(|| format!("looking for {MEASUREMENTS_FILE} in {}", aspect.path().display()))? {
				not_carried.push(NotCarried { path: format!("{subject_name}/{aspect_name}"), reason: if aspect_name == "dictionaries" { DICTIONARIES.to_string() } else { not_a_series(&aspect.path()).await } });
				continue;
			}
			for sibling in files(&aspect.path()).await? {
				let name = sibling.file_name().to_string_lossy().into_owned();
				if is_sidecar(&name) || !name.ends_with(".db") || name == MEASUREMENTS_FILE {
					continue;
				}
				not_carried.push(NotCarried { path: format!("{subject_name}/{aspect_name}/{name}"), reason: ASPECT_SIBLING.to_string() });
			}
			for nested in directories(&aspect.path()).await? {
				let nested_name = nested.file_name().to_string_lossy().into_owned();
				not_carried.push(NotCarried { path: format!("{subject_name}/{aspect_name}/{nested_name}"), reason: if nested_name == "dictionaries" { DICTIONARIES.to_string() } else { ASPECT_SIBLING.to_string() } });
			}
			aspects.push(survey(&subject_name, &aspect_name, &database_path).await?);
		}
	}
	aspects.sort_by(|one, other| (one.subject.as_str(), one.name.as_str()).cmp(&(other.subject.as_str(), other.name.as_str())));
	not_carried.sort_by(|one, other| one.path.cmp(&other.path));
	Ok(LegacyTree { root, database, aspects, not_carried })
}

/// The reason a database beside the rows stays behind.
const ASPECT_SIBLING: &str = "0.1.x kept its ingestion, pipeline, correlation and pattern state beside the rows (`compat/v010/README.md` records these files per aspect); the readable leg reads only `measurements.db`, and 1.0 leaves those subsystems behind.";

/// The reason a database at the subject level stays behind.
const SUBJECT_LEVEL: &str = "it sits where the layout puts no aspect's series: 0.1.x kept one database per aspect at `<subject>/<aspect>/measurements.db`.";

/// The reason an aspect's `dictionaries/` stays behind.
const DICTIONARIES: &str = "0.1.x's compression dictionaries for the aspect beside them; the segment store's encodings are declared per aspect, not learned and stored beside the data.";

/// Why a directory with no `measurements.db` in it is not a series.
async fn not_a_series(dir: &Path) -> String {
	let holds_databases = files(dir).await.unwrap_or_default().iter().any(|entry| entry.file_name().to_string_lossy().ends_with(".db"));
	if holds_databases {
		format!("it holds `*.db` files but no {MEASUREMENTS_FILE}, so the readable leg finds no series of ours in it")
	} else {
		format!("it holds no {MEASUREMENTS_FILE}, so it is not a series directory")
	}
}

/// Whether `name` is one of a libSQL database's sidecars, which travel with the database
/// they belong to and are never listed on their own.
#[must_use]
fn is_sidecar(name: &str) -> bool {
	["-wal", "-shm", "-log", ".wal", ".shm"].iter().any(|suffix| name.ends_with(suffix))
}

/// The child entries of `dir`, sorted by file name.
async fn entries(dir: &Path) -> Result<Vec<tokio::fs::DirEntry>> {
	let mut reader = tokio::fs::read_dir(dir).await.with_context(|| format!("reading the 0.1.x directory {}", dir.display()))?;
	let mut found = Vec::new();
	while let Some(entry) = reader.next_entry().await.with_context(|| format!("reading the 0.1.x directory {}", dir.display()))? {
		found.push(entry);
	}
	found.sort_by_key(|entry| entry.file_name());
	Ok(found)
}

/// The child **directories** of `dir`. A symlink is passed over, not followed: following one
/// would read a tree the operator did not scope this store to.
async fn directories(dir: &Path) -> Result<Vec<tokio::fs::DirEntry>> {
	let mut kept = Vec::new();
	for entry in entries(dir).await? {
		if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
			continue;
		}
		kept.push(entry);
	}
	Ok(kept)
}

/// The child **files** of `dir`, sidecars included: the callers ask whether a name is one.
async fn files(dir: &Path) -> Result<Vec<tokio::fs::DirEntry>> {
	let mut kept = Vec::new();
	for entry in entries(dir).await? {
		if !entry.file_type().await.is_ok_and(|kind| !kind.is_dir()) {
			continue;
		}
		kept.push(entry);
	}
	Ok(kept)
}

/// Reports one aspect's series, by reading the aggregates the tree itself holds.
///
/// This is the cheap half: it asks the database for its row count and span, and does not
/// carry its values. [`import_into`] reads those when it writes.
async fn survey(subject: &str, name: &str, database: &Path) -> Result<LegacyAspect> {
	let conn = open(database).await?;
	let columns = columns_of(&conn, database).await?;
	pick_value_column(database, &columns)?;
	#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
	let total = u64::try_from(scalar(&conn, database, "SELECT count(*) FROM measurements").await?).unwrap_or(u64::MAX);
	let mut rows = conn.query("SELECT min(timestamp), max(timestamp) FROM measurements", ()).await.with_context(|| format!("asking {} for the span of its series", database.display()))?;
	let row = rows.next().await?.with_context(|| format!("{} answers the span query with no row", database.display()))?;
	let first = match row.get_value(0)? {
		Value::Integer(when) => Some(when),
		Value::Null => None,
		other => return Err(anyhow::Error::new(RowsModeError::RowShape { path: database.to_path_buf(), rowid: 0, detail: format!("`min(timestamp)` is {other:?}, and 0.1.x wrote `timestamp INTEGER NOT NULL`") })),
	};
	let last = match row.get_value(1)? {
		Value::Integer(when) => Some(when),
		Value::Null => None,
		other => return Err(anyhow::Error::new(RowsModeError::RowShape { path: database.to_path_buf(), rowid: 0, detail: format!("`max(timestamp)` is {other:?}, and 0.1.x wrote `timestamp INTEGER NOT NULL`") })),
	};
	#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
	let distinct = u64::try_from(scalar(&conn, database, "SELECT count(DISTINCT timestamp) FROM measurements").await?).unwrap_or(0);
	Ok(LegacyAspect { subject: subject.to_string(), name: name.to_string(), database: database.to_path_buf(), rows: total, first_timestamp: first, last_timestamp: last, duplicate_timestamps: total.saturating_sub(distinct) })
}

/// Opens one `measurements.db` of the tree, to read it.
///
/// Deliberately bare: the crate's control-plane databases open through
/// [`connect`](crate::types::durable::control_plane::connect) and `enable_mvcc_full`, and
/// both **write** to the database they open. The readable leg reads a tree it does not own,
/// and 14a's whole promise is that nothing in it changes; its writes go to the store.
async fn open(database: &Path) -> Result<turso::Connection> {
	// `Builder::new_local` takes a `&str`; a lossy conversion is fine here because the path
	// came off the disk, and a name that is not valid UTF-8 still reads the same file.
	let db = Builder::new_local(&database.to_string_lossy()).build().await.with_context(|| format!("opening the 0.1.x series database {}", database.display()))?;
	Ok(db.connect().with_context(|| format!("connecting to the 0.1.x series database {}", database.display()))?)
}

/// The table every rows-mode database keeps its series in.
const LEGACY_TABLE: &str = "measurements";

/// The columns of `measurements`, each with the type its table declares it as.
///
/// `table_info` answers this whether or not the table holds a row, and it is also the
/// question "is this table here at all": no rows means the table is not there, which is
/// the refusal [`NoSeries`](RowsModeError::NoSeries) makes.
async fn columns_of(conn: &turso::Connection, database: &Path) -> Result<Vec<(String, String)>> {
	let mut rows = conn.query(format!("PRAGMA table_info({LEGACY_TABLE})"), ()).await.with_context(|| format!("asking {} which tables it has", database.display()))?;
	let mut columns = Vec::new();
	while let Some(row) = rows.next().await? {
		let name = match row.get_value(1)? {
			Value::Text(name) => name.to_string(),
			other => bail!("{} names a column of {} as {other:?}", database.display(), LEGACY_TABLE),
		};
		let declared = match row.get_value(2)? {
			Value::Text(declared) => declared.to_string(),
			Value::Null => String::new(),
			other => bail!("{} declares column {name:?} of {} as {other:?}", database.display(), LEGACY_TABLE),
		};
		columns.push((name, declared));
	}
	if columns.is_empty() {
		return Err(anyhow::Error::new(RowsModeError::NoSeries { path: database.to_path_buf(), detail: format!("it has no `{LEGACY_TABLE}` table") }));
	}
	Ok(columns)
}

/// The column the aspect's value sits in: `value`, and only `value`.
///
/// 0.1.x's own server created `measurements (id TEXT NOT NULL, dataset_id TEXT NOT NULL,
/// timestamp INTEGER NOT NULL, value TEXT NOT NULL)` (`v0.1.0:weftdb/src/types/aspect.rs:475`,
/// repeated at `:1195`) and every writer it has names that column in its `INSERT`
/// (`v0.1.0:weftdb/src/types/database/inputs.rs:283`, `:373`); the column list in the fixture's
/// page images matches, and so does its `value TEXT`. An earlier draft of this reader also
/// accepted a column named after the **aspect**, on a theory about a migration rename that does
/// not appear anywhere in 0.1.x's source — and it was worse than unattested. An aspect named
/// `id`, `dataset_id` or `timestamp` would have been read out of the column holding its
/// identifiers, and every row imported from it would have been an identifier posing as a
/// measurement, with nothing afterwards to say so. A column 0.1.x did not write is refused,
/// naming the columns that are there.
fn pick_value_column(database: &Path, columns: &[(String, String)]) -> Result<String> {
	if columns.is_empty() {
		return Err(anyhow::Error::new(RowsModeError::NoSeries { path: database.to_path_buf(), detail: "it has no `measurements` table to describe".to_string() }));
	}
	if !columns.iter().any(|(column, _)| column == TIMESTAMP_COLUMN) {
		return Err(anyhow::Error::new(RowsModeError::NoSeries { path: database.to_path_buf(), detail: format!("it has no `timestamp` column, which is the column 0.1.x stamped its instants into: its columns are {}", listed(columns)) }));
	}
	if !columns.iter().any(|(column, _)| column == "value") {
		return Err(anyhow::Error::new(RowsModeError::NoSeries { path: database.to_path_buf(), detail: format!("it has no `value` column, which is the column 0.1.x wrote its measurements into: its columns are {}", listed(columns)) }));
	}
	Ok("value".to_string())
}

/// Names a list of columns in one sentence.
fn listed(columns: &[(String, String)]) -> String {
	columns.iter().map(|(column, _)| format!("{column:?}")).collect::<Vec<_>>().join(", ")
}

/// One integer the database answers with, where zero rows or `NULL` means zero.
async fn scalar(conn: &turso::Connection, database: &Path, sql: &str) -> Result<i64> {
	let mut rows = conn.query(sql, ()).await.with_context(|| format!("asking {} for `{sql}`", database.display()))?;
	let Some(row) = rows.next().await? else { return Ok(0) };
	match row.get_value(0)? {
		Value::Integer(number) => Ok(number),
		Value::Null => Ok(0),
		other => bail!("{} answers `{sql}` with {other:?}, not a number", database.display()),
	}
}

/// Reads one aspect's rows, oldest first, as numbers the store can hold.
///
/// 0.1.x wrote no ordering guarantee and no unique constraint, so the read asks for one:
/// `ORDER BY timestamp`. A tie keeps the database's own row order, which is arrival order,
/// and the store sorts a seal anyway — the readable leg's read is stable rather than lucky.
async fn read_rows(conn: &turso::Connection, database: &Path, value_column: &str) -> Result<Vec<(i64, BigDecimal)>> {
	let sql = format!("SELECT {TIMESTAMP_COLUMN}, {} FROM {LEGACY_TABLE} ORDER BY {TIMESTAMP_COLUMN}", quote(value_column));
	let mut rows = conn.query(sql, ()).await.with_context(|| format!("reading the rows of {}", database.display()))?;
	let mut series = Vec::new();
	let mut position = 0i64;
	while let Some(row) = rows.next().await? {
		position += 1;
		let when = match row.get_value(0)? {
			Value::Integer(when) => when,
			other => {
				return Err(anyhow::Error::new(RowsModeError::RowShape { path: database.to_path_buf(), rowid: position, detail: format!("`{TIMESTAMP_COLUMN}` holds {other:?}, and 0.1.x wrote `timestamp INTEGER NOT NULL`") }));
			}
		};
		let value = match row.get_value(1)? {
			Value::Text(text) => {
				let shown = format!("{text}");
				BigDecimal::from_str(&shown).map_err(|error| RowsModeError::ValueNotDecimal { path: database.to_path_buf(), rowid: position, found: format!("{shown:?} ({error})") })?
			}
			// 0.1.x wrote its `BigDecimal` as the text of it, so a numeric or NULL value cell
			// is not a row 0.1.x wrote. Carrying the number anyway would put a value in the
			// store whose exactness nobody can vouch for.
			other => {
				return Err(anyhow::Error::new(RowsModeError::ValueNotDecimal { path: database.to_path_buf(), rowid: position, found: format!("{other:?}") }));
			}
		};
		series.push((when, value));
	}
	Ok(series)
}

/// Wraps a column name in double quotes, doubling any inside it. The column names this module
/// interpolates come from its own constants, but a name that came from a database would need
/// this before it went into a statement.
fn quote(name: &str) -> String {
	format!("\"{}\"", name.replace('"', "\"\""))
}

/// How a schema reads in a refusal sentence: the encoding, its tolerance, and the unit.
fn schema_text(schema: &AspectSchema) -> String {
	format!("{} (tolerance {}, timestamps in {})", encoding_name(schema.value), schema.value_tolerance, schema.timestamp_unit.name())
}

/// Names an encoding the way a record and a refusal should: `scaled_i64` for the plain
/// ones, `scaled_i64:4` where the encoding carries a scale.
fn encoding_name(physical_type: PhysicalType) -> String {
	match physical_type {
		PhysicalType::ScaledI64 { scale } | PhysicalType::ScaledI128 { scale } => format!("{}:{scale}", physical_type.name()),
		other => other.name().to_string(),
	}
}

/// The `store_meta` key the import record of one scope sits under.
///
/// It names the scope because one store root can hold scoped stores of several subjects and
/// databases, and a record has to say which import wrote which of them.
#[must_use]
pub fn record_key(database: &str, subject: &str) -> String {
	format!("{IMPORT_RECORD_KEY}:{database}:{subject}")
}

/// The import record of `store`'s own scope, if an import wrote one.
///
/// # Errors
///
/// Propagates a read failure of the store's index, and reports a record that is not the
/// JSON this module writes as an error naming the key.
pub async fn read_import_record(store: &SegmentStore) -> Result<Option<ImportRecord>> {
	let key = record_key(store.database(), store.subject());
	let Some(text) = store.index().meta(&key).await.with_context(|| format!("reading {key} from the store's own index"))? else {
		return Ok(None);
	};
	Ok(Some(serde_json::from_str(&text).with_context(|| format!("{} names {key}, and it is not the import record this module wrote", store.root().display()))?))
}

/// Writes `record` into the store's own index, under the key of the scope it describes.
async fn write_import_record(store: &SegmentStore, record: &ImportRecord) -> Result<()> {
	let key = record_key(&record.database, &record.subject);
	let value = serde_json::to_string(record).with_context(|| format!("rendering the import record of {key}"))?;
	store.index().apply(&IndexTxn::new(vec![IndexOp::MetaSet { key: key.clone(), value }])).await.with_context(|| format!("recording {key} in the store's own index"))?;
	Ok(())
}

/// Reads the rows-mode tree at `tree` into `store`, and records what it did.
///
/// The tree is read and never changed; the store takes the rows. Each aspect is declared
/// under the narrowest encoding that holds its values exactly (tolerance zero), then sealed
/// in batches of `rows_per_segment`, oldest first. When the last batch is in, the import's
/// own record goes into the store's index, naming the tree, the per-aspect counts, the
/// encodings, and what stayed in the tree.
///
/// The guard against a second reading of the same tree is that record: a migrated row and a
/// row the store sealed before look the same to it afterwards, so an import that found a
/// record for its own scope refuses rather than doubling the rows.
///
/// # Errors
///
/// Whatever [`discover`] refuses, plus [`ScopeMismatch`](RowsModeError::ScopeMismatch) when
/// the tree's database directory and the store's scope disagree,
/// [`NoSubject`](RowsModeError::NoSubject) when the tree holds no series of `subject`,
/// [`AlreadyImported`](RowsModeError::AlreadyImported) when this scope already has a record,
/// [`AspectNotEmpty`](RowsModeError::AspectNotEmpty) when the aspect already holds segments
/// this reading did not seal, [`SchemaConflict`](RowsModeError::SchemaConflict) when it is
/// declared under a schema the tree's values do not fit,
/// [`RowShape`](RowsModeError::RowShape) and
/// [`ValueNotDecimal`](RowsModeError::ValueNotDecimal) for a row that is not what 0.1.x
/// wrote, and any seal or index failure of the store itself.
///
/// A failure partway leaves the segments it already sealed, and no record: the next attempt
/// refuses on [`AspectNotEmpty`](RowsModeError::AspectNotEmpty), naming the aspect it
/// stopped on, rather than writing the tree's rows a second time. Freeing them is FRE-01's
/// rollback boundary, which is why no half-write is cleaned up here.
pub async fn import_into(store: &SegmentStore, tree: &Path, subject: &str, rows_per_segment: NonZeroUsize) -> Result<ImportRecord> {
	let rows_per_segment = rows_per_segment.get();
	if store.subject() != subject {
		bail!("this store is scoped to subject {:?}, and the readable leg reads one subject at a time; open a store of the subject you mean", store.subject());
	}
	let legacy = discover(tree).await?;
	if legacy.database != store.database() {
		return Err(anyhow::Error::new(RowsModeError::ScopeMismatch { tree_database: legacy.database.clone(), store_database: store.database().to_string(), subject: subject.to_string(), tree: tree.display().to_string() }));
	}
	aspect_name::validate(subject).with_context(|| format!("the tree's subject directory {:?} cannot name a subject in a store", legacy.database))?;
	let series = legacy.for_subject(subject);
	if series.is_empty() {
		return Err(anyhow::Error::new(RowsModeError::NoSubject { tree: tree.to_path_buf(), subject: subject.to_string() }));
	}
	if let Some(existing) = read_import_record(store).await? {
		return Err(anyhow::Error::new(RowsModeError::AlreadyImported { tree: existing.source.clone(), imported_at: existing.imported_at.clone(), aspects: existing.aspects.len(), rows: existing.rows() }));
	}

	let mut imported = Vec::new();
	for found in series {
		let aspect = found.name.as_str();
		aspect_name::validate(aspect).with_context(|| format!("the tree's aspect directory {:?} cannot name an aspect in a store", found.name))?;
		let conn = open(&found.database).await?;
		let columns = columns_of(&conn, &found.database).await?;
		let value_column = pick_value_column(&found.database, &columns)?;
		let rows = read_rows(&conn, &found.database, &value_column).await?;

		// The narrowest encoding that holds the tree's values of this aspect exactly, at
		// tolerance zero: the readable leg trades no precision across the version boundary,
		// so the choice is the store's own recommendation rather than a schema carried over
		// from 0.1.x, which kept its values as text.
		let chosen = recommend_encoding(&rows.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>(), &BigDecimal::from(0));
		let wanted = AspectSchema { value: chosen.physical_type, value_tolerance: BigDecimal::from(0), timestamp_unit: TIMESTAMP_TIME_UNIT };
		// The aspect's own state in the store decides, before anything is written: rows
		// already sealed under it stop the reading (a migrated row and a row the store
		// sealed before are indistinguishable afterwards), and a schema already declared
		// has to be the one the tree's values need.
		let segments = store.aspect_stats(aspect).await.with_context(|| format!("asking {} what it holds of {aspect:?}", store.root().display()))?.segment_count;
		if segments > 0 {
			return Err(anyhow::Error::new(RowsModeError::AspectNotEmpty { aspect: aspect.to_string(), segments }));
		}
		if let Some(declared) = store.schema_for(aspect).await? {
			if declared != wanted {
				return Err(anyhow::Error::new(RowsModeError::SchemaConflict { aspect: aspect.to_string(), declared: schema_text(&declared), wanted: schema_text(&wanted) }));
			}
		} else {
			store.declare(aspect, &wanted).await.with_context(|| format!("declaring {aspect:?} in {}", store.root().display()))?;
		}

		let mut sealed = 0u64;
		for chunk in chunks_keeping_instants_together(&rows, rows_per_segment) {
			let timestamps: Vec<i64> = chunk.iter().map(|(when, _)| *when).collect();
			let chunk_values: Vec<BigDecimal> = chunk.iter().map(|(_, value)| value.clone()).collect();
			store.seal_declared(aspect, &timestamps, &chunk_values).await.with_context(|| format!("sealing {} row(s) of {aspect:?} into {}", timestamps.len(), store.root().display()))?;
			sealed += 1;
		}
		imported.push(ImportedAspect { aspect: aspect.to_string(), rows: u64::try_from(rows.len()).unwrap_or(u64::MAX), segments: sealed, encoding: encoding_name(chosen.physical_type), first_timestamp: rows.first().map(|(when, _)| *when), last_timestamp: rows.last().map(|(when, _)| *when), duplicate_timestamps: found.duplicate_timestamps });
	}

	// The record speaks for this scope: it lists what stayed in the tree under this
	// subject's directory, and the tree-level files, and not another subject's series.
	let not_carried = legacy.not_carried.iter().filter(|entry| entry.path.split_once('/').map_or(true, |(head, _)| head == subject)).cloned().collect();
	let record = ImportRecord { source: tree.display().to_string(), database: legacy.database.clone(), subject: subject.to_string(), timestamp_unit: TIMESTAMP_UNIT.to_string(), rows_per_segment, aspects: imported, not_carried, imported_at: Utc::now().to_rfc3339() };
	write_import_record(store, &record).await?;
	Ok(record)
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;

	/// Writes one `measurements.db` the way 0.1.x wrote it: the table its own server
	/// created, the instant as an integer, and the value as the text of a `BigDecimal`.
	async fn write_series(database: &Path, value_column: &str, rows: &[(i64, &str)]) {
		if let Some(parent) = database.parent() {
			tokio::fs::create_dir_all(parent).await.expect("the tree's directories are there");
		}
		let db = Builder::new_local(&database.to_string_lossy()).build().await.expect("a local database of the 0.1.x shape");
		let conn = db.connect().expect("a connection to it");
		// The table 0.1.x's server ran, column for column
		// (`v0.1.0:weftdb/src/types/aspect.rs:475`), identifiers included. A caller that wants a
		// shape 0.1.x did *not* write passes another value column, which is what the refusal test
		// asks for.
		conn.execute("CREATE TABLE measurements (id TEXT NOT NULL, dataset_id TEXT NOT NULL, timestamp INTEGER NOT NULL, value TEXT NOT NULL)", ()).await.expect("0.1.x created its table with this DDL");
		if value_column != "value" {
			conn.execute(format!("ALTER TABLE measurements RENAME COLUMN value TO {}", quote(value_column)), ()).await.expect("a column name 0.1.x did not use");
		}
		// The two identifiers are written because the DDL will not accept a row without them:
		// 0.1.x put a transaction id and its database id in every row. The readable leg reads
		// neither — it takes the aspect's name from the directory and the instant and value from
		// the columns — so the values here only have to satisfy the table the release created.
		for (position, (when, value)) in rows.iter().enumerate() {
			conn.execute(format!("INSERT INTO measurements (id, dataset_id, timestamp, {}) VALUES (?1, ?2, ?3, ?4)", quote(value_column)), turso::params![format!("legacy-tx-{position}"), "legacy-dataset", when, value]).await.expect("a row goes in");
		}
		// The whole story on disk before the readable leg looks: a checkpointed database is
		// what an operator would hand over, WAL sidecars and all. `wal_checkpoint` answers with
		// a row, so it goes through `query` and the row is drained — `execute` refuses a
		// statement that returns results.
		let mut checkpoint = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await.expect("the rows are in the database file");
		while checkpoint.next().await.expect("the checkpoint answers once").is_some() {}
		drop(conn);
	}

	/// The catalog that makes a directory a 0.1.x database at all. The readable leg never opens
	/// it — it takes the aspect's name from the directory beside it — so the tests leave a
	/// stand-in, and discovery only asks whether it is there.
	async fn write_catalog(root: &Path) {
		tokio::fs::create_dir_all(root).await.expect("the tree root is there");
		tokio::fs::write(root.join("metadata.db"), b"the 0.1.x catalog, as a stand-in").await.expect("a catalog");
	}

	/// The tree most of these tests read: database `plant`, two subjects, one aspect whose
	/// value column named `value`, one subject-level database of 0.1.x's own state, and
	/// a dictionary directory beside the series.
	async fn build_tree(root: &Path) {
		write_catalog(root).await;
		write_series(&root.join("pump-01/temperature/measurements.db"), "value", &[(1767225600000, "100.2500"), (1767225601000, "101.0000"), (1767225602000, "99.5000")]).await;
		write_series(&root.join("pump-01/vibration/measurements.db"), "value", &[(1767225600000, "0.1250"), (1767225601000, "0.2500")]).await;
		write_series(&root.join("pump-02/temperature/measurements.db"), "value", &[(1767225600000, "-3.5000")]).await;
		tokio::fs::write(root.join("pump-01/ingestion.db"), b"0.1.x kept its ingestion state beside the rows").await.expect("a subject-level database");
		tokio::fs::create_dir_all(root.join("pump-02/dictionaries")).await.expect("a dictionary directory");
	}

	/// A store scoped to `database`/`subject`, beside the tree rather than in it.
	async fn store_at(root: &Path, database: &str, subject: &str) -> SegmentStore {
		SegmentStore::open_scoped(root, database, subject).await.expect("a scoped store opens")
	}

	/// Every file under `root`, by tree-relative path, with its bytes.
	async fn tree_state(root: &Path) -> BTreeMap<String, Vec<u8>> {
		let mut files = BTreeMap::new();
		let mut pending = vec![root.to_path_buf()];
		while let Some(dir) = pending.pop() {
			let mut entries = tokio::fs::read_dir(&dir).await.expect("the tree can be read");
			while let Some(entry) = entries.next_entry().await.expect("a directory entry") {
				let path = entry.path();
				if path.is_dir() {
					pending.push(path);
					continue;
				}
				let name = path.strip_prefix(root).expect("a tree-relative path").display().to_string();
				files.insert(name, tokio::fs::read(&path).await.expect("a file's bytes"));
			}
		}
		files
	}

	/// The release's DDL calls the value column `value` whatever the aspect is called
	/// (`v0.1.0:weftdb/src/types/aspect.rs:475-480`), so a database that names the column after
	/// the aspect is a shape this build has no evidence about. Reading "whichever column shares
	/// the aspect's name" was an earlier draft of this reader, and guessed the wrong way it reads
	/// 0.1.x's own identifiers back as measurements — which is the hazard the positive test below
	/// pins down.
	#[tokio::test]
	async fn a_series_database_that_is_not_the_shape_0_1_x_wrote_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		write_catalog(&root).await;
		write_series(&root.join("pump-01/flow/measurements.db"), "flow", &[(1767225600000, "1.5000")]).await;
		let error = discover(&root).await.expect_err("a series database whose value column is not the one 0.1.x created is a shape this build does not guess at");
		assert!(matches!(error.downcast_ref::<RowsModeError>(), Some(RowsModeError::NoSeries { .. })), "{error}");
		let detail = error.to_string();
		assert!(detail.contains("it has no `value` column"), "the refusal says which column it was looking for: {detail}");
		assert!(detail.contains("\"flow\""), "and it names the columns the database does have, so the operator can see what they handed over: {detail}");
		assert!(!detail.contains("value cell"), "it refused the shape before reading anything out of it: {detail}");
	}

	/// The hazard above stated positively, because refusing is only half of it: `dataset_id` is a
	/// legal 0.1.x aspect name *and* the name of the column in the release's DDL that holds 0.1.x's
	/// own dataset identifier. An aspect with that name is a legal series — its name comes from the
	/// directory, its values from `value` — and the identifier column is never a measurement.
	#[tokio::test]
	async fn an_aspect_named_after_an_identifier_column_is_still_read_from_value() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		write_catalog(&root).await;
		write_series(&root.join("pump-01/dataset_id/measurements.db"), "value", &[(1767225600000, "1.5000"), (1767225601000, "2.5000")]).await;
		let tree = discover(&root).await.expect("an aspect called dataset_id is a name 0.1.x allowed, and the directory is what names the series");
		assert_eq!(tree.aspects.iter().map(|aspect| (aspect.subject.as_str(), aspect.name.as_str(), aspect.rows)).collect::<Vec<_>>(), [("pump-01", "dataset_id", 2)], "the aspect keeps its directory's name even where it collides with a column");
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let record = import_into(&store, &root, "pump-01", NonZeroUsize::new(8).unwrap()).await.expect("the series reads in");
		assert_eq!((record.rows(), record.aspects.len()), (2, 1), "nothing of the identifier column crossed");
		let (timestamps, values) = store.read_time_range("dataset_id", i64::MIN, i64::MAX).await.expect("the aspect reads back under its directory name");
		assert_eq!(timestamps, [1767225600000, 1767225601000], "and it reads back under the name the operator will type");
		let want: Vec<BigDecimal> = ["1.5000", "2.5000"].map(|text| BigDecimal::from_str(text).expect("a decimal the tree would hold")).to_vec();
		assert_eq!(values.iter().flatten().cloned().collect::<Vec<_>>(), want, "the values come from `value`, not from the column that shares the aspect's name");
	}

	/// What the readable leg sees before it reads anything: the series, by name, and the
	/// things it will not carry.
	#[tokio::test]
	async fn a_rows_mode_tree_reads_as_a_tree_of_series() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let tree = discover(&root).await.expect("a tree of one database and two subjects is a rows-mode tree");
		assert_eq!(tree.database, "plant", "0.1.x named a database by its directory, and the readable leg takes the store's scope from that name");
		assert_eq!(tree.subjects(), ["pump-01", "pump-02"]);
		let series: Vec<(&str, &str, u64)> = tree.aspects.iter().map(|aspect| (aspect.subject.as_str(), aspect.name.as_str(), aspect.rows)).collect();
		assert_eq!(series, [("pump-01", "temperature", 3), ("pump-01", "vibration", 2), ("pump-02", "temperature", 1)], "one database per aspect, read from the directory that names it");
		assert_eq!(tree.for_subject("pump-01").iter().map(|aspect| aspect.name.as_str()).collect::<Vec<_>>(), ["temperature", "vibration"], "the series of one subject, which is the size of a scoped store's import");
		let temperature = tree.aspects.iter().find(|aspect| aspect.subject == "pump-01").expect("the tree holds pump-01's temperature");
		assert_eq!((temperature.first_timestamp, temperature.last_timestamp), (Some(1767225600000), Some(1767225602000)), "the span is the instants as stored, in milliseconds");
		let reported: Vec<&str> = tree.not_carried.iter().map(|entry| entry.path.as_str()).collect();
		assert_eq!(reported, ["metadata.db", "pump-01/ingestion.db", "pump-02/dictionaries"], "the catalog, 0.1.x's own state beside the rows, and the dictionaries are each named with a reason");
	}

	/// The point of the slice: the rows come across, exactly, and the store can answer for
	/// them afterwards.
	#[tokio::test]
	async fn reading_a_rows_mode_tree_puts_its_rows_in_the_store() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let record = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect("the readable leg reads the tree's pump-01 series");
		assert_eq!(record.database, "plant");
		assert_eq!(record.subject, "pump-01");
		assert_eq!(record.timestamp_unit, "millis", "0.1.x stamped milliseconds into the column, and the declaration says so instead of guessing at the numbers");
		assert_eq!(record.rows_per_segment, 2);
		let read: Vec<(&str, u64, u64)> = record.aspects.iter().map(|aspect| (aspect.aspect.as_str(), aspect.rows, aspect.segments)).collect();
		assert_eq!(read, [("temperature", 3, 2), ("vibration", 2, 1)], "three rows in batches of two is two segments, and two rows is one");
		assert_eq!((record.rows(), record.segments()), (5, 3), "the record totals what the store took");

		let (timestamps, values) = store.read_time_range("temperature", i64::MIN, i64::MAX).await.expect("the aspect reads back");
		assert_eq!(timestamps, [1767225600000, 1767225601000, 1767225602000], "the instants cross the boundary unchanged: no division, and no unit read off how big they look");
		let want: Vec<BigDecimal> = ["100.2500", "101.0000", "99.5000"].map(|text| BigDecimal::from_str(text).expect("a decimal the tree would hold")).to_vec();
		assert_eq!(values.iter().flatten().cloned().collect::<Vec<_>>(), want, "the values are the ones the tree held, to the last digit");

		// What the other subject holds is not in this store, and never was.
		assert_eq!(store.read_time_range("temperature", 1767225600000, 1767225600000).await.expect("a one-instant window").1.iter().flatten().count(), 1, "the window holds pump-01's row and not pump-02's");
		for aspect in ["temperature", "vibration"] {
			let declared = store.schema_for(aspect).await.expect("the store answers for its schema").unwrap_or_else(|| panic!("the leg declared {aspect} before sealing it"));
			let named = record.aspects.iter().find(|entry| entry.aspect == aspect).expect("the record names each aspect it read");
			assert_eq!(named.encoding, encoding_name(declared.value), "the record names the encoding the store actually holds {aspect} in");
			assert_eq!(declared.timestamp_unit, TIMESTAMP_TIME_UNIT, "the unit is declared, per aspect");
			assert_eq!(declared.value_tolerance, BigDecimal::from(0), "exactness across a version boundary, not tolerance");
		}
	}

	/// A second reading of the same tree would double the rows, and a migrated row and a row
	/// the store sealed before look the same to it afterwards. The record is what stops it.
	#[tokio::test]
	async fn a_tree_already_read_refuses_to_be_read_again() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let first = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect("the first reading goes through");
		let second = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("the tree was read once already, and the store knows it");
		assert!(matches!(second.downcast_ref::<RowsModeError>(), Some(RowsModeError::AlreadyImported { aspects: 2, rows: 5, .. })), "{second}");
		assert!(second.to_string().contains(&first.source), "the refusal names the tree it was asked to read again: {second}");
		assert_eq!(store.read_time_range("temperature", i64::MIN, i64::MAX).await.expect("the aspect still reads").0.len(), 3, "the refusal left the rows it would have doubled where they were");
	}

	/// A directory is only a rows-mode tree if 0.1.x's catalog is in it. Without that the
	/// readable leg has no database to name, and an empty or half-copied directory must not
	/// be read into a store on a guess.
	#[tokio::test]
	async fn a_directory_that_is_not_a_rows_mode_tree_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		tokio::fs::create_dir_all(root.join("pump-01/temperature")).await.expect("the tree's directories are there");
		let missing = discover(&root).await.expect_err("there is no catalog, so this is not a tree yet");
		assert!(matches!(missing.downcast_ref::<RowsModeError>(), Some(RowsModeError::NoCatalog { .. })), "{missing}");

		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let nested = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("the readable leg does not read an incomplete tree into a store");
		assert!(nested.to_string().contains("metadata.db"), "the refusal says what the tree is missing: {nested}");
	}

	/// The tree's database directory names the store's scope. A two-database tree — which is
	/// what 0.1.x left behind when an operator kept more than one — must land on the right
	/// side of the boundary, and not on the other one by mistake.
	#[tokio::test]
	async fn a_tree_of_another_database_refuses_to_be_read_into_this_store() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let store = store_at(&dir.path().join("store"), "greenhouse", "pump-01").await;
		let wrong = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("the tree is `plant` and this store is `greenhouse`");
		assert!(matches!(wrong.downcast_ref::<RowsModeError>(), Some(RowsModeError::ScopeMismatch { tree_database: _, store_database: _, .. })), "{wrong}");
		assert!(wrong.to_string().contains("greenhouse") && wrong.to_string().contains("plant"), "the refusal names both sides of the mismatch: {wrong}");
		assert_eq!(store.list_declared_aspects().await.expect("the store answers for its declared aspects").len(), 0, "the refusal declared nothing");
	}

	/// A tree read into a store that already holds the aspect would double its rows, and the
	/// store cannot tell the two apart afterwards. The aspect's own state stops it.
	#[tokio::test]
	async fn a_store_that_already_holds_the_aspect_keeps_the_rows_it_has() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let schema = AspectSchema { value: PhysicalType::ScaledI64 { scale: 4 }, value_tolerance: BigDecimal::from(0), timestamp_unit: TIMESTAMP_TIME_UNIT };
		store.declare("temperature", &schema).await.expect("the aspect is declared before the store takes a row of its own");
		store.seal_declared("temperature", &[1767225699000], &[BigDecimal::from_str("7.0000").expect("a decimal a 0.1.x tree would hold")]).await.expect("a row the store wrote itself");
		let clashing = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("the store holds temperature already, and reading the tree again would add to it");
		assert!(matches!(clashing.downcast_ref::<RowsModeError>(), Some(RowsModeError::AspectNotEmpty { aspect, segments: 1 }) if aspect == "temperature"), "{clashing}");
		let (timestamps, _) = store.read_time_range("temperature", i64::MIN, i64::MAX).await.expect("the aspect reads back");
		assert_eq!(timestamps, [1767225699000], "the row the store wrote is the only one it holds: the tree's rows were not added to it");
		assert!(read_import_record(&store).await.expect("the index answers for its own keys").is_none(), "a reading that stopped wrote no record for a later run to trust");
	}

	/// The same guard from the store's other side: an aspect that is declared but holds nothing.
	/// This is what an operator who set the aspect up for live data walks into, and the field
	/// that matters is the timestamp unit — 0.1.x's rows-mode path stamped millis where its batch
	/// path kept seconds, so a hand-declared aspect is plausible at either scale. Reading past
	/// the declaration would file every row of the tree a thousand-fold away from the instants the
	/// operator sized their queries around, and the aspect's schema is the only thing in the store
	/// that says which scale they meant.
	#[tokio::test]
	async fn an_aspect_declared_at_another_timestamp_scale_refuses_the_reading() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		write_catalog(&root).await;
		write_series(&root.join("pump-01/temperature/measurements.db"), "value", &[(1767225600000, "100.2500"), (1767225601000, "101.0000")]).await;
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		// The schema the tree's own rows call for, in every field but the unit: the refusal is
		// about the unit and nothing else.
		let values: Vec<BigDecimal> = ["100.2500", "101.0000"].map(|text| BigDecimal::from_str(text).expect("a decimal the tree would hold")).to_vec();
		let chosen = recommend_encoding(&values, &BigDecimal::from(0));
		let seconds = AspectSchema { value: chosen.physical_type, value_tolerance: BigDecimal::from(0), timestamp_unit: TimeUnit::Seconds };
		store.declare("temperature", &seconds).await.expect("an aspect the operator declared for live data, at seconds");
		let clash = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("the tree stamps millis and the aspect is declared at seconds");
		assert!(matches!(clash.downcast_ref::<RowsModeError>(), Some(RowsModeError::SchemaConflict { aspect, .. }) if aspect == "temperature"), "{clash}");
		let detail = clash.to_string();
		assert!(detail.contains("seconds") && detail.contains("millis"), "the refusal prints both schemas, so the operator can see which side to change: {detail}");
		let (timestamps, _) = store.read_time_range("temperature", i64::MIN, i64::MAX).await.expect("an empty aspect answers");
		assert!(timestamps.is_empty(), "and nothing was written at either scale: {timestamps:?}");
		assert!(read_import_record(&store).await.expect("the index answers for its own keys").is_none(), "nor was anything recorded as having crossed");
	}

	/// A value cell that is not the text of a decimal is not a row 0.1.x wrote. Carrying the
	/// bytes anyway would put a value in the store whose exactness nobody can vouch for, so
	/// the whole read stops and says which row it stopped on — and what it left behind is part
	/// of the answer, because the leg writes per aspect and writes its record last.
	#[tokio::test]
	async fn a_row_that_is_not_a_decimal_stops_the_read_and_names_its_row() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		tokio::fs::create_dir_all(&root).await.expect("the tree root is there");
		tokio::fs::write(root.join("metadata.db"), b"the 0.1.x catalog, as a stand-in").await.expect("a catalog");
		write_series(&root.join("pump-01/temperature/measurements.db"), "value", &[(1767225600000, "100.2500")]).await;
		write_series(&root.join("pump-01/vibration/measurements.db"), "value", &[(1767225600000, "bearing-worn")]).await;
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let broken = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("one of the tree's value cells is not a number");
		assert!(matches!(broken.downcast_ref::<RowsModeError>(), Some(RowsModeError::ValueNotDecimal { .. })), "the refusal is typed, and it is the value refusals rather than a shape one: {broken}");
		assert!(broken.to_string().contains("bearing-worn"), "the refusal quotes the cell it could not read: {broken}");
		assert!(read_import_record(&store).await.expect("the index answers for its own keys").is_none(), "a reading that stopped wrote no record for a later run to trust");

		// What crossed stays: `temperature` read cleanly, so it is declared and sealed, and the
		// leg's record says nothing about it. A retry of the same tree then refuses on the
		// aspect that is already there rather than finishing the job — the stranding the module
		// doc lists as a boundary, pinned here so the doc stays the one the code keeps.
		let crossed = store.read_time_range("temperature", i64::MIN, i64::MAX).await.expect("the aspect that read cleanly reads back");
		assert_eq!(crossed.0.len(), 1, "the leg writes one aspect at a time, so `temperature` crossed before `vibration` stopped it");
		let retry = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect_err("the half-read tree cannot be read again quietly: the aspect it already sealed would be doubled");
		assert!(matches!(retry.downcast_ref::<RowsModeError>(), Some(RowsModeError::AspectNotEmpty { aspect, .. }) if aspect.as_str() == "temperature"), "the retry refuses on the aspect already in the store, which is the trap 14c has to design around: {retry}");
	}

	/// 14a promised the operator's tree keeps its bytes. The readable leg reads it and writes
	/// nothing: every file it found is still the file it left, at the same path.
	#[tokio::test]
	async fn reading_the_tree_leaves_every_file_it_found_as_it_found_it() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let before = tree_state(&root).await;
		assert_eq!(before.keys().filter(|name| name.ends_with("measurements.db")).count(), 3, "the test's tree holds three series databases");
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect("the readable leg reads the tree's pump-01 series");
		let after = tree_state(&root).await;
		for (path, bytes) in &before {
			assert_eq!(after.get(path), Some(bytes), "reading the tree changed {path}");
		}
		assert_eq!(after.keys().collect::<Vec<_>>(), before.keys().collect::<Vec<_>>(), "the leg added no file to the tree, and deleted none from it");
	}

	/// The record is the store's own, and it survives the store being closed and reopened:
	/// it is what a later run, or an operator asking what came across, reads.
	#[tokio::test]
	async fn a_finished_reading_is_recorded_in_the_store_itself() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		build_tree(&root).await;
		let at = dir.path().join("store");
		let store = store_at(&at, "plant", "pump-01").await;
		assert!(read_import_record(&store).await.expect("an untouched store has no record").is_none(), "a store nobody read a tree into answers with nothing");
		let written = import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect("the readable leg reads the tree's pump-01 series");
		drop(store);
		let reopened = store_at(&at, "plant", "pump-01").await;
		let read_back = read_import_record(&reopened).await.expect("the record is in the store's index, not in memory").expect("a store an import wrote has a record");
		let round_trip = serde_json::to_string(&read_back).expect("the record is JSON by construction");
		assert_eq!(round_trip, serde_json::to_string(&written).expect("the record the leg returned"), "the record the leg returned is the record the store kept");
		assert_eq!(read_back.not_carried.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(), ["metadata.db", "pump-01/ingestion.db"], "the record keeps the tree's catalog and this subject's own state beside the rows; pump-02's dictionary directory is not this reading's to report");
		assert_eq!(read_back.aspects.iter().map(|aspect| aspect.aspect.as_str()).collect::<Vec<_>>(), ["temperature", "vibration"], "the record says which series crossed, and no other subject's");
	}

	#[test]
	fn a_chunk_never_splits_rows_that_share_an_instant() {
		let rows: Vec<(i64, BigDecimal)> = [0i64, 1, 1, 1, 2, 3, 3, 4].iter().map(|when| (*when, BigDecimal::from(*when))).collect();
		let chunks = chunks_keeping_instants_together(&rows, 2);
		let instants: Vec<Vec<i64>> = chunks.iter().map(|chunk| chunk.iter().map(|(when, _)| *when).collect()).collect();
		assert_eq!(instants, vec![vec![0, 1, 1, 1], vec![2, 3, 3], vec![4]], "the four rows at instant 1 go into one segment though the caller asked for two rows each: a segment a few rows longer than asked is cheaper than the reconciliation that folds two segments and keeps only the newer row at an instant they share");
		assert_eq!(chunks.iter().map(|chunk| chunk.len()).sum::<usize>(), rows.len(), "and no row is lost or sealed twice on the way");
		assert!(chunks.iter().all(|chunk| !chunk.is_empty()), "a chunk always has rows, so no seal call is asked to write an empty segment");
		assert!(chunks_keeping_instants_together(&[], 3).is_empty(), "an empty series seals nothing at all");
		let even: Vec<(i64, BigDecimal)> = [1i64, 2, 3, 4].iter().map(|when| (*when, BigDecimal::from(*when))).collect();
		assert_eq!(chunks_keeping_instants_together(&even, 2).iter().map(|chunk| chunk.len()).collect::<Vec<_>>(), vec![2, 2], "a series with one row per instant still gets the chunk size it asked for");
	}
}
