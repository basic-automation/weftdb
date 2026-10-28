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
//! ([`weft_physical_type::recommend_encoding`] at tolerance `0` — the migration never trades precision),
//! seals the rows in time order in chunks of `rows_per_segment` (a chunk never splits
//! between two rows with the same timestamp), carries the catalog rows for the aspects
//! it read into the store's `metadata.db` (migration `0003_fre14_legacy_catalog`),
//! and writes an [`ImportRecord`] into `store_meta` naming the tree, the counts, the
//! encodings, and what stayed behind.
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
//! - **Memory**: no aspect is ever held whole. The encoding survey streams the
//!   aspect's rows through a cursor (`survey_encoding`: one pass for the scale and
//!   the scale-free candidates, a second for the scaled candidates once the scale is
//!   known), and the seal streams them again in chunks of `rows_per_segment`
//!   (`seal_streaming`). What one chunk holds is `rows_per_segment` rows plus the
//!   rows sharing its last instant, which is the bound the instant-group rule forces:
//!   a group of rows at one instant seals into one segment, so a group larger than the
//!   chunk is the chunk. The survey's decision is exactly
//!   [`weft_physical_type::recommend_encoding`]'s at tolerance `0` — the candidates are tried in the
//!   same order, and each candidate's exactness is a per-value property aggregated
//!   with AND (for exactness) and max (for the scale) — so streaming changes the
//!   memory, not the encoding.
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
//! - **Not carried**: the legacy catalog's transaction log and host paths, the
//!   per-aspect `dictionaries/`, and the ingestion/pipeline/correlation/pattern
//!   databases beside the rows. They are listed with their reasons rather than dropped
//!   silently. The catalog's aspect rows *are* carried — into the store's own
//!   `metadata.db` by migration `0003_fre14_legacy_catalog` (see
//!   [`LegacyCatalogAspect`]) — because a migrated aspect's declared resolution,
//!   creation time and compression config are the state an operator asks about after
//!   the rows have crossed, and they live nowhere else. The catalog's database and
//!   subject rows reduce to the scope the store already registers on open, and their
//!   ids, creation times and host path have no 1.0 consumer; the transaction log is
//!   not replayed, because the import record in `store_meta` is the transaction this
//!   migration records.
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
use weft_physical_type::{AspectSchema, Exactness, PhysicalType, TimeUnit};

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
/// One chunk of a streamed aspect, for sealing: `rows_per_segment` rows, extended
/// past any group of rows sharing the chunk's last instant.
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
///
/// The bound this puts on memory is the chunk plus one instant group: a group larger
/// than `rows_per_segment` is the chunk, because a group seals into one segment or
/// not at all.
struct SealingChunk {
	rows: Vec<(i64, BigDecimal)>,
	limit: usize,
}

impl SealingChunk {
	fn new(limit: usize) -> Self {
		Self { rows: Vec::new(), limit }
	}

	/// Push `row`; when it starts a new instant past the limit, the full chunk is
	/// returned to seal and the row starts the next one. Otherwise nothing is
	/// returned and the row joins the chunk.
	fn push(&mut self, row: (i64, BigDecimal)) -> Option<Vec<(i64, BigDecimal)>> {
		if self.rows.len() >= self.limit {
			if let Some((last, _)) = self.rows.last() {
				if row.0 != *last {
					let full = std::mem::take(&mut self.rows);
					self.rows.push(row);
					return Some(full);
				}
			}
		}
		self.rows.push(row);
		None
	}

	/// The rows still held, if any: an empty aspect seals nothing at all.
	fn finish(self) -> Option<Vec<(i64, BigDecimal)>> {
		if self.rows.is_empty() {
			None
		} else {
			Some(self.rows)
		}
	}
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
	/// A `metadata.db` that is not the catalog 0.1.x wrote: it cannot be opened as a
	/// database, or it lacks the `database`, `subjects` or `aspects` tables the
	/// catalog's DDL created (`v0.1.0:weftdb/src/types/database/mod.rs`), or a row
	/// in one of them does not hold the type its column declares. The readable leg
	/// reads the catalog's aspect rows, so a catalog it cannot read is a tree it
	/// cannot migrate — not a tree whose rows it reads on a guess.
	#[error("{} is not a 0.1.x catalog: {}", .path.display(), .detail)]
	NoCatalogSchema { path: PathBuf, detail: String },
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

/// One aspect's row in the 0.1.x catalog (`metadata.db`'s `aspects` table, joined to
/// `subjects` for the subject's name), as [`discover`] read it.
///
/// This is the catalog's non-row state for the aspect: 0.1.x's own id for it, the
/// resolution it was declared with, when it was created, the span its catalog
/// recorded, and its compression config. The rows themselves come from
/// `measurements.db`; this row is what [`import_into`] carries beside them into the
/// store's `metadata.db` (migration `0003_fre14_legacy_catalog`). A series directory
/// with no catalog row has no entry here: the rows are authoritative — the directory
/// names the series and `measurements.db` holds it — so a missing catalog row
/// degrades provenance, not the read, and the aspect simply gets no catalog row in
/// the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyCatalogAspect {
	/// The subject's name, from the catalog's `subjects` row — the same name as the
	/// subject directory, which is what the import matches on.
	pub subject: String,
	/// The aspect's name, from the catalog's `aspects` row — the same name as the
	/// aspect directory.
	pub name: String,
	/// 0.1.x's id for the aspect (`aspects.id`), if the catalog row carries one.
	pub legacy_aspect_id: Option<String>,
	/// The resolution the aspect was declared with (`aspects.resolution`): the
	/// sampling grid, not a time unit — the store declares `Millis` from the rows,
	/// never from this.
	pub resolution: Option<String>,
	/// When the aspect was created, in milliseconds (`aspects.created_at`).
	pub created_at: Option<i64>,
	/// The span the catalog recorded, as text (`aspects.earliest_measurement`,
	/// `aspects.latest_measurement`): 0.1.x kept these as text, so they cross as
	/// text rather than being parsed into instants the catalog never promised.
	pub earliest_measurement: Option<String>,
	pub latest_measurement: Option<String>,
	/// The compression config the aspect was declared with
	/// (`aspects.compression_config`), if any.
	pub compression_config: Option<String>,
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
	/// The catalog's aspect rows, ordered by `<subject>/<aspect>`: the non-row state
	/// [`import_into`] carries for the aspects it reads. A series with no catalog
	/// row has no entry here.
	pub catalog: Vec<LegacyCatalogAspect>,
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

	/// The catalog rows of `subject`, in order.
	#[must_use]
	pub fn catalog_for_subject(&self, subject: &str) -> Vec<&LegacyCatalogAspect> {
		self.catalog.iter().filter(|aspect| aspect.subject == subject).collect()
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
	// The catalog is read, not just noticed: its aspect rows are the non-row state
	// the import carries, so a catalog that is not what 0.1.x wrote refuses here,
	// before any store is opened, rather than importing rows whose provenance is
	// unknown. A series with no catalog row still reads — the directory names it —
	// it just gets no catalog row in the store.
	let catalog = read_catalog(&root.join("metadata.db")).await?;

	let mut aspects = Vec::new();
	let mut not_carried = vec![NotCarried { path: "metadata.db".to_string(), reason: "the 0.1.x catalog: its aspect rows cross into the store's metadata.db (migration 0003_fre14_legacy_catalog); its transaction log and host paths stay behind, because 1.0 does not replay 0.1.x's log and the import record is the transaction this migration records.".to_string() }];
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
	Ok(LegacyTree { root, database, aspects, catalog, not_carried })
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
	let conn = open(database, "series").await?;
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

/// Reads the catalog's aspect rows out of `metadata.db`: the non-row state the import
/// carries.
///
/// 0.1.x's catalog DDL (`v0.1.0:weftdb/src/types/database/mod.rs`) creates
/// `database`, `subjects` and `aspects` tables; the aspects join to their subject by
/// `subject_id`, and the subject's name is what the import matches a series directory
/// against. A catalog without those tables, or a row whose cell is not the type its
/// column declares, is [`NoCatalogSchema`](RowsModeError::NoCatalogSchema): the
/// migration cannot carry state it cannot read. A row with no subject name (a
/// `subject_id` the catalog does not hold) or no aspect name names no series
/// directory, so it is passed over rather than refused: the rows are authoritative,
/// and a dangling catalog row is provenance for nothing.
///
/// The catalog's `database` rows are not read: the tree's directory names the
/// database, and the store registers that scope on open. The `transactions` table
/// is not read either: 1.0 does not replay 0.1.x's log.
async fn read_catalog(catalog: &Path) -> Result<Vec<LegacyCatalogAspect>> {
	let conn = open(catalog, "catalog").await.map_err(|error| anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("it cannot be opened: {error:#}") }))?;
	for table in ["database", "subjects", "aspects"] {
		let columns = catalog_columns(&conn, catalog, table).await?;
		if columns.is_empty() {
			return Err(anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("it has no `{table}` table, which 0.1.x's catalog DDL creates") }));
		}
	}
	let mut rows = conn.query("SELECT s.name, a.name, a.id, a.resolution, a.created_at, a.earliest_measurement, a.latest_measurement, a.compression_config FROM aspects a LEFT JOIN subjects s ON a.subject_id = s.id ORDER BY s.name, a.name", ()).await.map_err(|error| anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("its `aspects`/`subjects` tables cannot be read: {error}") }))?;
	let mut found = Vec::new();
	while let Some(row) = rows.next().await.map_err(|error| anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("its `aspects`/`subjects` tables cannot be read: {error}") }))? {
		let subject = match catalog_text(&row, 0, catalog, "subjects.name")? {
			Some(name) => name,
			None => continue,
		};
		let name = match catalog_text(&row, 1, catalog, "aspects.name")? {
			Some(name) => name,
			None => continue,
		};
		found.push(LegacyCatalogAspect { subject, name, legacy_aspect_id: catalog_text(&row, 2, catalog, "aspects.id")?, resolution: catalog_text(&row, 3, catalog, "aspects.resolution")?, created_at: catalog_int(&row, 4, catalog, "aspects.created_at")?, earliest_measurement: catalog_text(&row, 5, catalog, "aspects.earliest_measurement")?, latest_measurement: catalog_text(&row, 6, catalog, "aspects.latest_measurement")?, compression_config: catalog_text(&row, 7, catalog, "aspects.compression_config")? });
	}
	Ok(found)
}

/// The column names of the catalog's `table`, or a typed refusal when the table
/// cannot be listed at all.
async fn catalog_columns(conn: &turso::Connection, catalog: &Path, table: &str) -> Result<Vec<String>> {
	let mut rows = conn.query(format!("PRAGMA table_info({table})"), ()).await.map_err(|error| anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("its `{table}` table cannot be listed: {error}") }))?;
	let mut columns = Vec::new();
	while let Some(row) = rows.next().await.map_err(|error| anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("its `{table}` table cannot be listed: {error}") }))? {
		if let Value::Text(name) = row.get_value(1).map_err(|error| anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("its `{table}` table names a column as {error}") }))? {
			columns.push(name);
		}
	}
	Ok(columns)
}

/// One text cell of a catalog row: the text, `None` for NULL, and a typed refusal for
/// anything else — 0.1.x wrote text or NULL here, and a number in its place is a
/// catalog this migration does not know how to read.
fn catalog_text(row: &turso::Row, index: usize, catalog: &Path, column: &str) -> Result<Option<String>> {
	match row.get_value(index)? {
		Value::Text(text) => Ok(Some(text)),
		Value::Null => Ok(None),
		other => Err(anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("`{column}` holds {other:?}, and 0.1.x wrote text or NULL there") })),
	}
}

/// One integer cell of a catalog row, on the same terms as [`catalog_text`].
fn catalog_int(row: &turso::Row, index: usize, catalog: &Path, column: &str) -> Result<Option<i64>> {
	match row.get_value(index)? {
		Value::Integer(number) => Ok(Some(number)),
		Value::Null => Ok(None),
		other => Err(anyhow::Error::new(RowsModeError::NoCatalogSchema { path: catalog.to_path_buf(), detail: format!("`{column}` holds {other:?}, and 0.1.x wrote an integer or NULL there") })),
	}
}

/// Opens one database of the tree, to read it.
///
/// Deliberately bare: the crate's control-plane databases open through
/// [`connect`](crate::types::durable::control_plane::connect) and `enable_mvcc_full`, and
/// both **write** to the database they open. The readable leg reads a tree it does not own,
/// and 14a's whole promise is that nothing in it changes; its writes go to the store.
/// `role` names the database in errors (`series` for a `measurements.db`, `catalog`
/// for `metadata.db`).
async fn open(database: &Path, role: &str) -> Result<turso::Connection> {
	// `Builder::new_local` takes a `&str`; a lossy conversion is fine here because the path
	// came off the disk, and a name that is not valid UTF-8 still reads the same file.
	let db = Builder::new_local(&database.to_string_lossy()).build().await.with_context(|| format!("opening the 0.1.x {role} database {}", database.display()))?;
	Ok(db.connect().with_context(|| format!("connecting to the 0.1.x {role} database {}", database.display()))?)
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

/// The rows of one aspect, oldest first, as a cursor: the query every streaming pass
/// of an aspect runs.
///
/// 0.1.x wrote no ordering guarantee and no unique constraint, so the read asks for one:
/// `ORDER BY timestamp`. A tie keeps the database's own row order, which is arrival order,
/// and the store sorts a seal anyway — the readable leg's read is stable rather than lucky.
fn rows_sql(value_column: &str) -> String {
	format!("SELECT {TIMESTAMP_COLUMN}, {} FROM {LEGACY_TABLE} ORDER BY {TIMESTAMP_COLUMN}", quote(value_column))
}

/// One row of that cursor, as numbers the store can hold. `position` counts from one
/// in the read's order, which is what a refusal names: the 0.1.x row id is not part of
/// what the readable leg carries, and a row that fails is reported by where it was found.
fn parse_row(row: &turso::Row, database: &Path, position: i64) -> Result<(i64, BigDecimal)> {
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
	Ok((when, value))
}

/// Whether `value` encodes exactly under `physical_type`: the encode succeeds and its
/// reconstruction is the value itself. This is the per-value half of
/// [`weft_physical_type::recommend_encoding`]'s decision at tolerance `0` — a column is exact under a
/// candidate exactly when every value is.
fn encodes_exactly(physical_type: PhysicalType, value: &BigDecimal) -> bool {
	matches!(physical_type.encode(value), Ok(encoded) if encoded.exactness == Exactness::Exact)
}

/// The fractional digits [`weft_physical_type::recommend_encoding`] needs for its scaled candidates: the
/// largest fractional-digit count in the column, on the same terms as its
/// `max_fractional_digits` — the normalized exponent, clamped at zero, saturating at
/// `u8::MAX`.
fn fractional_digits(value: &BigDecimal) -> u8 {
	let (_, exponent) = value.normalized().as_bigint_and_exponent();
	u8::try_from(exponent.max(0)).unwrap_or(u8::MAX)
}

/// The narrowest encoding that holds one aspect's values exactly, decided without
/// holding the aspect: the streaming half of [`weft_physical_type::recommend_encoding`] at tolerance `0`.
///
/// The first pass streams every row once, validating it ([`parse_row`]) and
/// aggregating three things: the scale (the max of [`fractional_digits`]), and
/// whether `F32`, `F64` and `Decimal128` encode every value exactly. Those three
/// candidates do not depend on the scale, so one pass settles them. The scaled
/// candidates do depend on it, so a second pass — only when neither float is exact —
/// streams the rows again and tests `ScaledI64` and `ScaledI128` at that scale.
/// The choice then follows [`weft_physical_type::recommend_encoding`]'s candidate order, and is the same
/// choice: each candidate's exactness is a per-value property, so "every value exact"
/// computed in a stream is the same fact as "the column encodes exact" computed whole.
/// An empty aspect has no value to disqualify a candidate, so it takes `F32`, as the
/// whole-column call does.
async fn survey_encoding(conn: &turso::Connection, database: &Path, value_column: &str) -> Result<PhysicalType> {
	let sql = rows_sql(value_column);
	let mut rows = conn.query(&sql, ()).await.with_context(|| format!("reading the rows of {}", database.display()))?;
	let mut scale = 0u8;
	let mut f32_exact = true;
	let mut f64_exact = true;
	let mut decimal128_exact = true;
	let mut position = 0i64;
	while let Some(row) = rows.next().await? {
		position += 1;
		let (_, value) = parse_row(&row, database, position)?;
		scale = scale.max(fractional_digits(&value));
		if f32_exact && !encodes_exactly(PhysicalType::F32, &value) {
			f32_exact = false;
		}
		if f64_exact && !encodes_exactly(PhysicalType::F64, &value) {
			f64_exact = false;
		}
		if decimal128_exact && !encodes_exactly(PhysicalType::Decimal128, &value) {
			decimal128_exact = false;
		}
	}
	drop(rows);
	if f32_exact {
		return Ok(PhysicalType::F32);
	}
	if f64_exact {
		return Ok(PhysicalType::F64);
	}
	let mut scaled_i64_exact = true;
	let mut scaled_i128_exact = true;
	let mut rows = conn.query(&sql, ()).await.with_context(|| format!("reading the rows of {}", database.display()))?;
	let mut position = 0i64;
	while let Some(row) = rows.next().await? {
		position += 1;
		let (_, value) = parse_row(&row, database, position)?;
		if scaled_i64_exact && !encodes_exactly(PhysicalType::ScaledI64 { scale }, &value) {
			scaled_i64_exact = false;
		}
		if scaled_i128_exact && !encodes_exactly(PhysicalType::ScaledI128 { scale }, &value) {
			scaled_i128_exact = false;
		}
		if !scaled_i64_exact && !scaled_i128_exact {
			break;
		}
	}
	if scaled_i64_exact {
		return Ok(PhysicalType::ScaledI64 { scale });
	}
	if scaled_i128_exact {
		return Ok(PhysicalType::ScaledI128 { scale });
	}
	if decimal128_exact {
		return Ok(PhysicalType::Decimal128);
	}
	Ok(PhysicalType::BigDecimalText)
}

/// Seals one aspect's rows into `store` in chunks of `rows_per_segment`, streaming
/// them from a cursor rather than holding the aspect: each row joins the current
/// chunk ([`SealingChunk`]), and a full chunk is sealed as soon as the next row
/// starts a new instant past the limit. Returns the rows sealed and the segments
/// sealed, in that order. An empty aspect seals nothing.
async fn seal_streaming(store: &SegmentStore, aspect: &str, conn: &turso::Connection, database: &Path, value_column: &str, rows_per_segment: usize) -> Result<(u64, u64)> {
	let mut rows = conn.query(rows_sql(value_column), ()).await.with_context(|| format!("reading the rows of {}", database.display()))?;
	let mut chunk = SealingChunk::new(rows_per_segment);
	let mut sealed_rows = 0u64;
	let mut sealed_segments = 0u64;
	let mut position = 0i64;
	while let Some(row) = rows.next().await? {
		position += 1;
		let parsed = parse_row(&row, database, position)?;
		if let Some(full) = chunk.push(parsed) {
			seal_chunk(store, aspect, &full).await?;
			sealed_rows += u64::try_from(full.len()).unwrap_or(u64::MAX);
			sealed_segments += 1;
		}
	}
	if let Some(rest) = chunk.finish() {
		seal_chunk(store, aspect, &rest).await?;
		sealed_rows += u64::try_from(rest.len()).unwrap_or(u64::MAX);
		sealed_segments += 1;
	}
	Ok((sealed_rows, sealed_segments))
}

/// Seals one chunk's rows into `store` under `aspect`, in the chunk's own order.
async fn seal_chunk(store: &SegmentStore, aspect: &str, chunk: &[(i64, BigDecimal)]) -> Result<()> {
	let timestamps: Vec<i64> = chunk.iter().map(|(when, _)| *when).collect();
	let values: Vec<BigDecimal> = chunk.iter().map(|(_, value)| value.clone()).collect();
	store.seal_declared(aspect, &timestamps, &values).await.with_context(|| format!("sealing {} row(s) of {aspect:?} into {}", timestamps.len(), store.root().display()))?;
	Ok(())
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
/// in batches of `rows_per_segment`, oldest first — both the survey and the seal stream
/// the aspect from a cursor, so no aspect is ever held whole. When the last batch is in,
/// the catalog rows of the aspects read go into the store's `metadata.db`, and the
/// import's own record goes into the store's index, naming the tree, the per-aspect
/// counts, the encodings, and what stayed in the tree.
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
		let conn = open(&found.database, "series").await?;
		let columns = columns_of(&conn, &found.database).await?;
		let value_column = pick_value_column(&found.database, &columns)?;

		// The narrowest encoding that holds the tree's values of this aspect exactly, at
		// tolerance zero: the readable leg trades no precision across the version boundary,
		// so the choice is the store's own recommendation rather than a schema carried over
		// from 0.1.x, which kept its values as text. The survey streams the aspect
		// rather than holding it, and it validates every value before anything is
		// written for this aspect — the same order the whole-column read kept.
		let chosen = survey_encoding(&conn, &found.database, &value_column).await?;
		let wanted = AspectSchema { value: chosen, value_tolerance: BigDecimal::from(0), timestamp_unit: TIMESTAMP_TIME_UNIT };
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

		let (rows_sealed, sealed) = seal_streaming(store, aspect, &conn, &found.database, &value_column, rows_per_segment).await?;
		imported.push(ImportedAspect { aspect: aspect.to_string(), rows: rows_sealed, segments: sealed, encoding: encoding_name(chosen), first_timestamp: found.first_timestamp, last_timestamp: found.last_timestamp, duplicate_timestamps: found.duplicate_timestamps });
	}

	// The catalog rows of the aspects this reading carried, into the store's
	// `metadata.db`: a catalog row for an aspect this subject's directories do not
	// hold as a series is not migrated, because there are no rows of it to migrate.
	let catalog_rows: Vec<LegacyCatalogAspect> = legacy.catalog_for_subject(subject).into_iter().filter(|catalog| imported.iter().any(|read| read.aspect == catalog.name)).cloned().collect();
	store.record_legacy_catalog(&legacy.database, subject, &catalog_rows).await.with_context(|| format!("recording the tree's catalog rows in {}", store.root().display()))?;

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

	use weft_physical_type::recommend_encoding;

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

	/// The catalog that makes a directory a 0.1.x database at all: the tables 0.1.x's
	/// DDL created (`v0.1.0:weftdb/src/types/database/mod.rs`) and the database row
	/// for the directory's name. Subject and aspect rows are added by
	/// [`write_catalog_aspect`], so a test that needs none gets a valid but empty
	/// catalog — and discovery still has to read it, because the import now carries
	/// the catalog's aspect rows.
	async fn write_catalog(root: &Path) {
		tokio::fs::create_dir_all(root).await.expect("the tree root is there");
		let path = root.join("metadata.db");
		let db = Builder::new_local(&path.to_string_lossy()).build().await.expect("a local database of the 0.1.x shape");
		let conn = db.connect().expect("a connection to it");
		conn.execute("CREATE TABLE IF NOT EXISTS transactions (id TEXT NOT NULL, message TEXT NOT NULL, created_at INTEGER NOT NULL)", ()).await.expect("0.1.x created its transactions table with this DDL");
		conn.execute("CREATE TABLE IF NOT EXISTS database (id TEXT NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL, metadata_path TEXT NOT NULL)", ()).await.expect("0.1.x created its database table with this DDL");
		conn.execute("CREATE TABLE IF NOT EXISTS subjects (id TEXT NOT NULL, database_id TEXT NOT NULL, name TEXT NOT NULL, created_at INTEGER NOT NULL)", ()).await.expect("0.1.x created its subjects table with this DDL");
		conn.execute("CREATE TABLE IF NOT EXISTS aspects (id TEXT NOT NULL, subject_id TEXT NOT NULL, database_id TEXT NOT NULL, name TEXT NOT NULL, table_name TEXT NOT NULL, resolution TEXT NOT NULL, created_at INTEGER NOT NULL, earliest_measurement TEXT, latest_measurement TEXT, compression_config TEXT)", ()).await.expect("0.1.x created its aspects table with this DDL");
		let name = root.file_name().expect("the tree's directory names its database").to_string_lossy().into_owned();
		conn.execute("INSERT INTO database (id, name, created_at, metadata_path) VALUES (?1, ?2, ?3, ?4)", turso::params!["legacy-database-id", name, 1767225600000i64, path.to_string_lossy().into_owned()]).await.expect("0.1.x recorded its database row");
		let mut checkpoint = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await.expect("the rows are in the database file");
		while checkpoint.next().await.expect("the checkpoint answers once").is_some() {}
		drop(conn);
	}

	/// Adds one subject row to the catalog [`write_catalog`] made, the way 0.1.x
	/// recorded a subject it was about to track aspects of.
	async fn write_catalog_subject(root: &Path, subject: &str, created_at: i64) {
		let path = root.join("metadata.db");
		let db = Builder::new_local(&path.to_string_lossy()).build().await.expect("a local database of the 0.1.x shape");
		let conn = db.connect().expect("a connection to it");
		conn.execute("INSERT INTO subjects (id, database_id, name, created_at) VALUES (?1, ?2, ?3, ?4)", turso::params![format!("subject-{subject}"), "legacy-database-id", subject, created_at]).await.expect("0.1.x recorded its subject row");
		let mut checkpoint = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await.expect("the rows are in the database file");
		while checkpoint.next().await.expect("the checkpoint answers once").is_some() {}
		drop(conn);
	}

	/// Adds one aspect row to the catalog [`write_catalog`] made, the way 0.1.x's own
	/// `track_aspect` recorded it: the aspect's name, its resolution, its creation
	/// time, the span its catalog recorded, and its compression config. The subject
	/// row must already be there ([`write_catalog_subject`]). A test that wants an
	/// aspect with no catalog row simply does not call this for it.
	#[allow(clippy::too_many_arguments)]
	async fn write_catalog_aspect(root: &Path, subject: &str, aspect: &str, resolution: &str, created_at: i64, earliest: &str, latest: &str, compression_config: Option<&str>) {
		let path = root.join("metadata.db");
		let db = Builder::new_local(&path.to_string_lossy()).build().await.expect("a local database of the 0.1.x shape");
		let conn = db.connect().expect("a connection to it");
		conn.execute("INSERT INTO aspects (id, subject_id, database_id, name, table_name, resolution, created_at, earliest_measurement, latest_measurement, compression_config) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", turso::params![format!("aspect-{subject}-{aspect}"), format!("subject-{subject}"), "legacy-database-id", aspect, aspect, resolution, created_at, earliest, latest, compression_config.map_or(turso::Value::Null, |text| turso::Value::Text(text.to_string()))]).await.expect("0.1.x recorded its aspect row");
		let mut checkpoint = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await.expect("the rows are in the database file");
		while checkpoint.next().await.expect("the checkpoint answers once").is_some() {}
		drop(conn);
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
		write_catalog(&root).await;
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

	/// Runs `rows` through a [`SealingChunk`] of `limit`, collecting every chunk it
	/// hands back, in order.
	fn chunked(rows: &[(i64, BigDecimal)], limit: usize) -> Vec<Vec<(i64, BigDecimal)>> {
		let mut chunk = SealingChunk::new(limit);
		let mut chunks = Vec::new();
		for row in rows {
			if let Some(full) = chunk.push(row.clone()) {
				chunks.push(full);
			}
		}
		if let Some(rest) = chunk.finish() {
			chunks.push(rest);
		}
		chunks
	}

	#[test]
	fn a_chunk_never_splits_rows_that_share_an_instant() {
		let rows: Vec<(i64, BigDecimal)> = [0i64, 1, 1, 1, 2, 3, 3, 4].iter().map(|when| (*when, BigDecimal::from(*when))).collect();
		let chunks = chunked(&rows, 2);
		let instants: Vec<Vec<i64>> = chunks.iter().map(|chunk| chunk.iter().map(|(when, _)| *when).collect()).collect();
		assert_eq!(instants, vec![vec![0, 1, 1, 1], vec![2, 3, 3], vec![4]], "the four rows at instant 1 go into one segment though the caller asked for two rows each: a segment a few rows longer than asked is cheaper than the reconciliation that folds two segments and keeps only the newer row at an instant they share");
		assert_eq!(chunks.iter().map(|chunk| chunk.len()).sum::<usize>(), rows.len(), "and no row is lost or sealed twice on the way");
		assert!(chunks.iter().all(|chunk| !chunk.is_empty()), "a chunk always has rows, so no seal call is asked to write an empty segment");
		assert!(chunked(&[], 3).is_empty(), "an empty series seals nothing at all");
		let even: Vec<(i64, BigDecimal)> = [1i64, 2, 3, 4].iter().map(|when| (*when, BigDecimal::from(*when))).collect();
		assert_eq!(chunked(&even, 2).iter().map(|chunk| chunk.len()).collect::<Vec<_>>(), vec![2, 2], "a series with one row per instant still gets the chunk size it asked for");
	}

	/// The streaming survey and the whole-column recommendation must agree: the
	/// survey exists to save memory, not to change the encoding. Each case runs both
	/// over the same series database, and the survey's answer is the recommendation's.
	#[tokio::test]
	async fn the_streaming_survey_picks_the_encoding_the_whole_column_call_picks() {
		let cases: Vec<Vec<&str>> = vec![vec![], vec!["1", "2", "3"], vec!["1.25", "2.50"], vec!["0.1", "0.2", "0.3"], vec!["123456789.123456789"], vec!["1000000000000000000000000000000"], vec!["10000000000000000000000000000000000000000"], vec!["0.123456789012345678901234567890"], vec!["-3.5000", "0.0000", "99.5000"], vec!["0"]];
		for values in cases {
			let dir = tempfile::tempdir().unwrap();
			let path = dir.path().join("measurements.db");
			let rows: Vec<(i64, &str)> = values.iter().enumerate().map(|(position, value)| (1767225600000 + i64::try_from(position).unwrap_or(0), *value)).collect();
			write_series(&path, "value", &rows).await;
			let conn = open(&path, "series").await.expect("the series database opens");
			let columns = columns_of(&conn, &path).await.expect("the series database describes itself");
			let value_column = pick_value_column(&path, &columns).expect("the series database has the column 0.1.x wrote");
			let surveyed = survey_encoding(&conn, &path, &value_column).await.expect("the survey reads the series");
			let whole: Vec<BigDecimal> = values.iter().map(|text| BigDecimal::from_str(text).expect("a decimal the tree would hold")).collect();
			let recommended = recommend_encoding(&whole, &BigDecimal::from(0));
			assert_eq!(surveyed, recommended.physical_type, "the survey and the whole-column call disagree on {values:?}: the stream is supposed to save memory, not change the encoding");
		}
		// And two answers pinned, not just agreed: small integers are f32, and a
		// decimal no binary float holds exactly is the scaled integer at its scale.
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("measurements.db");
		write_series(&path, "value", &[(1767225600000, "1"), (1767225601000, "2")]).await;
		let conn = open(&path, "series").await.expect("the series database opens");
		let columns = columns_of(&conn, &path).await.expect("the series database describes itself");
		let value_column = pick_value_column(&path, &columns).expect("the series database has the column 0.1.x wrote");
		assert_eq!(survey_encoding(&conn, &path, &value_column).await.expect("the survey reads the series"), PhysicalType::F32, "small integers are f32");
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("measurements.db");
		write_series(&path, "value", &[(1767225600000, "0.1")]).await;
		let conn = open(&path, "series").await.expect("the series database opens");
		let columns = columns_of(&conn, &path).await.expect("the series database describes itself");
		let value_column = pick_value_column(&path, &columns).expect("the series database has the column 0.1.x wrote");
		assert_eq!(survey_encoding(&conn, &path, &value_column).await.expect("the survey reads the series"), PhysicalType::ScaledI64 { scale: 1 }, "a tenth no binary float holds exactly is the scaled integer at scale 1");
	}

	/// A streamed aspect seals in chunks without losing a row at a shared instant:
	/// two thousand rows in groups of ten at one instant, sealed a hundred at a time
	/// minus five, so every chunk boundary falls inside a group and has to extend
	/// past it. The segment count, the row count and every shared instant come back
	/// exactly, which is the whole point of streaming: the aspect never sat in memory
	/// as one column, and no group was cut in two.
	#[tokio::test]
	async fn a_streaming_read_seals_a_large_aspect_in_chunks_without_losing_a_shared_instant() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		write_catalog(&root).await;
		let mut rows: Vec<(i64, String)> = Vec::new();
		for group in 0..200i64 {
			for member in 0..10i64 {
				rows.push((1767225600000 + group * 1000, format!("{}.5000", group * 10 + member)));
			}
		}
		let borrowed: Vec<(i64, &str)> = rows.iter().map(|(when, value)| (*when, value.as_str())).collect();
		write_series(&root.join("pump-01/temperature/measurements.db"), "value", &borrowed).await;
		let store = store_at(&dir.path().join("store"), "plant", "pump-01").await;
		let record = import_into(&store, &root, "pump-01", NonZeroUsize::new(95).unwrap()).await.expect("a two-thousand-row aspect streams in");
		let aspect = record.aspects.iter().find(|entry| entry.aspect == "temperature").expect("the aspect crossed");
		assert_eq!((aspect.rows, aspect.segments), (2000, 20), "every chunk extends past the group it cut into, so twenty hundred-row segments hold two thousand rows");
		let (timestamps, values) = store.read_time_range("temperature", i64::MIN, i64::MAX).await.expect("the aspect reads back");
		assert_eq!(timestamps.len(), 2000, "no row was lost at a chunk boundary");
		let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
		for when in &timestamps {
			*counts.entry(*when).or_default() += 1;
		}
		assert_eq!(counts.len(), 200, "two hundred instants crossed");
		assert!(counts.values().all(|count| *count == 10), "every instant still holds its ten rows: {counts:?}");
		// Compared normalized: the store's decode does not preserve the trailing
		// zeros the tree's text had, so `0.5000` reads back as `0.5` — the same
		// value, not the same spelling.
		let mut read_values: Vec<String> = values.iter().flatten().map(|value| value.normalized().to_plain_string()).collect();
		read_values.sort();
		let mut want: Vec<String> = rows.iter().map(|(_, value)| BigDecimal::from_str(value).expect("a decimal the tree would hold").normalized().to_plain_string()).collect();
		want.sort();
		assert_eq!(read_values, want, "the values are the ones the tree held, to the last digit");
	}

	/// The catalog's non-row state crosses with the rows: the aspect rows for the
	/// subject land in the store's `metadata.db` under migration
	/// `0003_fre14_legacy_catalog`, and an aspect with no catalog row gets no row
	/// there — the rows are authoritative, so a missing catalog row degrades
	/// provenance rather than stopping the read.
	#[tokio::test]
	async fn the_catalogs_non_row_state_crosses_with_the_rows() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		write_catalog(&root).await;
		write_catalog_subject(&root, "pump-01", 1767225600000).await;
		write_catalog_aspect(&root, "pump-01", "temperature", "seconds", 1767225600000, "1767225600000", "1767225602000", Some("{\"codec\":\"zstd\"}")).await;
		write_catalog_aspect(&root, "pump-01", "vibration", "minutes", 1767225601000, "1767225600000", "1767225601000", None).await;
		write_series(&root.join("pump-01/temperature/measurements.db"), "value", &[(1767225600000, "100.2500"), (1767225601000, "101.0000"), (1767225602000, "99.5000")]).await;
		write_series(&root.join("pump-01/vibration/measurements.db"), "value", &[(1767225600000, "0.1250"), (1767225601000, "0.2500")]).await;
		write_series(&root.join("pump-01/pressure/measurements.db"), "value", &[(1767225600000, "7.5000")]).await;
		let tree = discover(&root).await.expect("the tree reads with its catalog");
		assert_eq!(tree.catalog.iter().map(|row| (row.subject.as_str(), row.name.as_str())).collect::<Vec<_>>(), [("pump-01", "temperature"), ("pump-01", "vibration")], "discovery reports the catalog rows, in order");
		let temperature = tree.catalog.iter().find(|row| row.name == "temperature").expect("the temperature row is there");
		assert_eq!((temperature.legacy_aspect_id.as_deref(), temperature.resolution.as_deref(), temperature.created_at, temperature.earliest_measurement.as_deref(), temperature.latest_measurement.as_deref(), temperature.compression_config.as_deref()), (Some("aspect-pump-01-temperature"), Some("seconds"), Some(1767225600000), Some("1767225600000"), Some("1767225602000"), Some("{\"codec\":\"zstd\"}")), "the row carries what 0.1.x recorded, not just the name");
		let at = dir.path().join("store");
		let store = store_at(&at, "plant", "pump-01").await;
		import_into(&store, &root, "pump-01", NonZeroUsize::new(2).unwrap()).await.expect("the tree reads in, catalog rows and all");
		drop(store);
		let db = Builder::new_local(&at.join("metadata.db").to_string_lossy()).build().await.expect("the store's metadata.db opens");
		let conn = db.connect().expect("a connection to it");
		let mut rows = conn.query("SELECT database, subject, aspect, legacy_aspect_id, resolution, created_at, earliest_measurement, latest_measurement, compression_config FROM legacy_catalog ORDER BY aspect", ()).await.expect("the migration's table reads");
		let mut carried = Vec::new();
		while let Some(row) = rows.next().await.expect("a catalog row") {
			let text = |index: usize| match row.get_value(index).expect("a cell") {
				Value::Text(value) => Some(value),
				Value::Null => None,
				other => panic!("a text cell holds {other:?}"),
			};
			let number = |index: usize| match row.get_value(index).expect("a cell") {
				Value::Integer(value) => Some(value),
				Value::Null => None,
				other => panic!("an integer cell holds {other:?}"),
			};
			carried.push((text(0), text(1), text(2), text(3), text(4), number(5), text(6), text(7), text(8)));
		}
		assert_eq!(carried, vec![(Some("plant".to_string()), Some("pump-01".to_string()), Some("temperature".to_string()), Some("aspect-pump-01-temperature".to_string()), Some("seconds".to_string()), Some(1767225600000), Some("1767225600000".to_string()), Some("1767225602000".to_string()), Some("{\"codec\":\"zstd\"}".to_string())), (Some("plant".to_string()), Some("pump-01".to_string()), Some("vibration".to_string()), Some("aspect-pump-01-vibration".to_string()), Some("minutes".to_string()), Some(1767225601000), Some("1767225600000".to_string()), Some("1767225601000".to_string()), None),], "the store's metadata.db holds the catalog rows of the aspects that crossed, and no row for the aspect with no catalog row");
	}

	/// A `metadata.db` that is not the catalog 0.1.x wrote refuses discovery: a
	/// database without the catalog's tables, and bytes that are not a database at
	/// all. The readable leg reads the catalog now, so a catalog it cannot read is
	/// a tree it cannot migrate.
	#[tokio::test]
	async fn a_catalog_without_the_0_1_x_tables_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		tokio::fs::create_dir_all(&root).await.expect("the tree root is there");
		let path = root.join("metadata.db");
		let db = Builder::new_local(&path.to_string_lossy()).build().await.expect("a database file");
		let conn = db.connect().expect("a connection to it");
		conn.execute("CREATE TABLE foo (x INTEGER)", ()).await.expect("an unrelated table");
		drop(conn);
		let error = discover(&root).await.expect_err("a database without the catalog's tables is not a catalog");
		assert!(matches!(error.downcast_ref::<RowsModeError>(), Some(RowsModeError::NoCatalogSchema { .. })), "{error}");
		assert!(error.to_string().contains("`database`"), "the refusal names the table it looked for: {error}");

		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("plant");
		tokio::fs::create_dir_all(&root).await.expect("the tree root is there");
		tokio::fs::write(root.join("metadata.db"), b"the 0.1.x catalog, as a stand-in").await.expect("a catalog-shaped file");
		let error = discover(&root).await.expect_err("bytes that are not a database are not a catalog");
		assert!(matches!(error.downcast_ref::<RowsModeError>(), Some(RowsModeError::NoCatalogSchema { .. })), "{error}");
	}
}
