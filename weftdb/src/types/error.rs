use thiserror::Error as ThisError;
use weft_physical_type::AspectSchema;

/// One declaration in the shape a refusal names it: the encoding (with its
/// scale, where it carries one), the tolerance and the timestamp unit.
fn describe_schema(schema: &AspectSchema) -> String {
	let value = match schema.value.scale() {
		Some(scale) => format!("{}:{scale}", schema.value.name()),
		None => schema.value.name().to_string(),
	};
	format!("{value} (tolerance {}, timestamps in {})", schema.value_tolerance, schema.timestamp_unit.name())
}

/// Upsert keys as an error message names them: the wire list, or `none (timestamp and
/// the full tag set)` for an undeclared aspect.
fn describe_upsert_keys(keys: Option<&crate::UpsertKeys>) -> String {
	keys.map_or_else(|| "none (timestamp and the full tag set)".to_string(), ToString::to_string)
}

#[derive(Debug, ThisError)]
pub enum Error {
	#[error("Insufficient measurements provided for interpolation")]
	InsufficientMeasurementsError,

	#[error("Different dataset IDs found in measurements")]
	DifferentDatasetIdsError,

	#[error("Inconsistent dataset IDs found in measurements")]
	InconsistentDatasetIdsError,

	#[error("Insufficient points for cubic spline interpolation")]
	InsufficientPointsForCubicSplineError,

	#[error("Invalid time range: start time must be before end time")]
	InvalidTimeRangeError,

	#[error("Database error: {0}")]
	DatabaseError(String),

	#[error("Interpolation error: {0}")]
	InterpolationError(String),

	#[error("Cache error: {0}")]
	CacheError(String),

	#[error("Invalid ID: {0}")]
	InvalidIdError(String),

	#[error("Numeric conversion error: {0}")]
	NumericConversionError(String),

	#[error("Transient MVCC error (retryable): {0}")]
	TransientMvccError(String),
}

impl Error {
	/// Check if this error is a transient MVCC error that should be retried
	#[must_use]
	pub const fn is_transient_mvcc(&self) -> bool {
		matches!(self, Self::TransientMvccError(_))
	}
}

/// Turso's message for an MVCC write-write conflict (`LimboError::WriteWriteConflict`),
/// which its Rust binding passes on as a plain [`turso::Error::Error`].
const WRITE_WRITE_CONFLICT: &str = "Write-write conflict";

/// Check if an anyhow error is a transient MVCC error: one that a retry of the whole
/// transaction can get past.
///
/// That is an [`Error::TransientMvccError`] or, anywhere in the error's chain (so also
/// under a `context`), one of Turso's: `Busy` (lock contention), `BusySnapshot` (a stale
/// snapshot or an aborted commit dependency), or a write-write conflict, after which
/// Turso has already rolled the transaction back.
pub fn is_transient_mvcc_error(err: &anyhow::Error) -> bool {
	err.chain().any(|cause| cause.downcast_ref::<Error>().is_some_and(Error::is_transient_mvcc) || cause.downcast_ref::<turso::Error>().is_some_and(is_transient_turso_error))
}

/// True for the Turso errors [`is_transient_mvcc_error`] retries.
fn is_transient_turso_error(err: &turso::Error) -> bool {
	match err {
		turso::Error::Busy(_) | turso::Error::BusySnapshot(_) => true,
		turso::Error::Error(message) => message == WRITE_WRITE_CONFLICT,
		_ => false,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn turso_conflicts_are_transient_anywhere_in_the_chain() {
		let conflict = || anyhow::Error::new(turso::Error::Error(WRITE_WRITE_CONFLICT.to_string()));
		assert!(is_transient_mvcc_error(&conflict()));
		assert!(is_transient_mvcc_error(&conflict().context("Failed to set dictionary metadata: Write-write conflict")));
		assert!(is_transient_mvcc_error(&anyhow::Error::new(turso::Error::Busy("database is locked".to_string()))));
		assert!(is_transient_mvcc_error(&anyhow::Error::new(turso::Error::BusySnapshot("database snapshot is stale".to_string()))));
		assert!(is_transient_mvcc_error(&anyhow::Error::new(Error::TransientMvccError("type mismatch".to_string())).context("read")));
	}

	#[test]
	fn other_errors_are_not_transient() {
		assert!(!is_transient_mvcc_error(&anyhow::Error::new(turso::Error::Error("no such table: patterns".to_string()))));
		assert!(!is_transient_mvcc_error(&anyhow::Error::new(turso::Error::Constraint("UNIQUE constraint failed".to_string()))));
		assert!(!is_transient_mvcc_error(&anyhow::Error::new(Error::DatabaseError("Write-write conflict".to_string()))));
		// Only the error itself counts, not a message that quotes one: this is how a
		// conflict looked once the failed `ROLLBACK` after it had replaced it.
		assert!(!is_transient_mvcc_error(&anyhow::anyhow!("Rollback failed: cannot rollback - no transaction is active")));
		assert!(!is_transient_mvcc_error(&anyhow::anyhow!("Failed to set dictionary metadata: Write-write conflict")));
	}
}

/// Why a [`SegmentStore`](crate::SegmentStore) refused an operation: an open it
/// would not perform (freeze design §4.3, FRE-12a), or — since D-S13 — an ingest
/// it would not commit, or — since ROB-13 — a stored read over its decision 12
/// bound. The ingest variants are the typed errors the 1.0 plan
/// (§3 X-ERR, §5 D-S13) assigns to this lane: they land here, in the store's one
/// typed error, rather than in sentences a caller must re-recognise.
///
/// It arrives inside an [`anyhow::Error`]; `downcast_ref::<StoreError>()` recovers it,
/// whatever context the operation added.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
#[non_exhaustive]
pub enum StoreError {
	/// The store's `STORE_FORMAT` marker (or, for a root without one, its `store_meta`
	/// copy in `segment_index.db`) says it can only be written by a WeftDB that knows
	/// layout `min_write` or newer, and this build knows layouts up to `supported`. A newer
	/// WeftDB wrote it; writing to it with this one would break that build's invariants.
	/// Nothing was changed: only `LOCK` and `LOCK.holder` were touched (a root without a
	/// marker had its `segment_index.db` read, and nothing written to it).
	#[error("this store needs store layout {min_write} or newer to be written, and this WeftDB writes layouts up to {supported}: a newer WeftDB wrote it. Open it with that WeftDB, or a newer one")]
	IncompatibleLayout {
		/// The store's write floor (`min_write_layout`).
		min_write: u32,
		/// The newest layout this build reads and writes
		/// ([`SUPPORTED_LAYOUT`](crate::SUPPORTED_LAYOUT)).
		supported: u32,
	},
	/// The store's `STORE_FORMAT` marker says it is in layout `layout`, newer than the
	/// layouts up to `supported` this build knows, but the root holds none of its
	/// control-plane databases: a newer WeftDB began creating the store and stopped before
	/// it created them, or they were removed. This build cannot create a layout it does not
	/// know, so it refuses the root. Nothing was changed: only `LOCK` and `LOCK.holder`
	/// were touched.
	#[error("the store marker says layout {layout}, newer than layout {supported}, the newest this WeftDB knows, and the root holds none of its databases: a newer WeftDB began creating this store. Open it with that WeftDB, or a newer one")]
	NewerStoreWithoutDatabases {
		/// The layout the marker records (`layout_version`).
		layout: u32,
		/// The newest layout this build reads and writes
		/// ([`SUPPORTED_LAYOUT`](crate::SUPPORTED_LAYOUT)).
		supported: u32,
	},
	/// The store's `STORE_FORMAT` marker exists but cannot be read as one: it is not
	/// JSON, lacks a field every marker has, or names another kind of store. Nothing was
	/// opened or changed.
	#[error("the store marker {} cannot be read ({reason}); restore it from a backup, or remove it only if this root holds no WeftDB store", path.display())]
	UnreadableStoreFormat {
		/// The marker file.
		path: std::path::PathBuf,
		/// What is wrong with it.
		reason: String,
	},
	/// The store is layout 1 and its segment index records frames whose paths leave
	/// `segments/` (possible only through the aspect-name traversal fixed before 1.0), so
	/// it cannot be migrated to layout 2 safely. It is refused before any migration
	/// applies and before the `STORE_FORMAT` marker is written. The frames are neither
	/// moved nor quarantined; the store opens again with the WeftDB that wrote it, which
	/// can drop or re-seal those aspects.
	#[error("the segment index records frames outside segments/ for {}: this store cannot be upgraded to layout 2 until they are dropped or re-sealed with the WeftDB that wrote them (nothing was moved or quarantined)", aspects.iter().map(|aspect| format!("{aspect:?}")).collect::<Vec<_>>().join(", "))]
	UnsafeLegacyPath {
		/// The aspects with such frames, in name order, each once.
		aspects: Vec<String>,
	},
	/// The directory opened is not a store but a **WeftDB 0.1.x data directory**: it keeps a
	/// database per aspect at `<subject>/<aspect>/measurements.db`, two levels below a
	/// top-level `metadata.db`. No reader for that layout exists yet, and opening such a directory as a
	/// store would be quietly wrong rather than loud: 0.1.x named its top-level database
	/// `metadata.db`, which is one of this build's [`CONTROL_PLANE_FILES`](crate::types::backup::CONTROL_PLANE_FILES),
	/// so the open would find a control plane, create the three databases missing from it, add
	/// `segments/` and a `STORE_FORMAT` marker next to the old data, and report an empty store
	/// with no error at all. It is refused before any database opens and before any of those
	/// files exist; only `LOCK` and `LOCK.holder` are touched, as with every other refusal.
	#[error("this is a WeftDB 0.1.x data directory, keeping a database per aspect ({}); this version reads stores, and nothing here was changed (only LOCK and LOCK.holder were written)", aspects.iter().map(|aspect| format!("{aspect:?}")).collect::<Vec<_>>().join(", "))]
	RowsModeDataDirectory {
		/// The `<subject>/<aspect>` directories holding a `measurements.db`, in path order,
		/// each once.
		aspects: Vec<String>,
	},
	/// An operation named `aspect`, which has no declared schema in this store
	/// (1.0 plan §3 X-ERR: `require_schema`'s typed `UnknownAspect`, landing with
	/// D-S13 because it is a store-lane edit). Nothing was written.
	#[error("aspect {aspect:?} has no declared schema in this store")]
	UnknownAspect {
		/// The aspect that was named.
		aspect: String,
	},
	/// A declaration named an aspect that is already declared under a
	/// different schema (FRE-7b: declarations are immutable once made). The
	/// only re-declaration a declared aspect accepts is the identical schema
	/// again, or a widening of the value encoding through
	/// `widen_value_encoding` (to `BigDecimalText`, tolerance and timestamp
	/// unit unchanged). Nothing was written; the declared schema stands.
	#[error("aspect {aspect:?} is already declared as {} and declarations are immutable; the offered {} was refused and nothing was written", describe_schema(declared), describe_schema(offered))]
	DeclarationConflict {
		/// The aspect that was re-declared.
		aspect: String,
		/// The schema the aspect is declared under.
		declared: AspectSchema,
		/// The schema the refused declaration offered.
		offered: AspectSchema,
	},
	/// An ingest offered a batch under an idempotency key that this aspect's
	/// ledger already holds for a **different** batch: the recorded fingerprint
	/// is not the offered one (D-S13). Replaying the recorded batch under the
	/// key returns its receipt; a different batch under the same key is a
	/// conflict (the plan §4 code `idempotency_conflict`), and nothing was
	/// written.
	#[error("aspect {aspect:?} already holds idempotency key {key:?} for a different batch (recorded fingerprint {recorded_fingerprint}, offered {offered_fingerprint}); nothing was written")]
	IdempotencyConflict {
		/// The aspect the batch was offered to.
		aspect: String,
		/// The idempotency key, as offered.
		key: String,
		/// The fingerprint the ledger recorded for the key.
		recorded_fingerprint: i64,
		/// The fingerprint of the batch just offered.
		offered_fingerprint: i64,
	},
	/// **Reserved; no longer produced.** An ingest batch carried a non-empty
	/// `groups` tag set before the tags gate flipped (1.0 plan §5 D-S13, §4
	/// C1/A4). The library gate opened in TAG-8: a tagged batch now seals as its
	/// series. The variant stays so that matching code keeps compiling.
	#[error("aspect {aspect:?}: ingest groups are not accepted yet (only the empty tag set, series 0, before the tags gate flips); nothing was written")]
	GroupsUnsupported {
		/// The aspect the batch was offered to.
		aspect: String,
	},
	/// An ingest batch named a series `aspect` has not registered, and the aspect
	/// already holds `limit` series, the most
	/// [`SegmentStoreOptions::max_series_per_aspect`](crate::SegmentStoreOptions::max_series_per_aspect)
	/// allows (TAG-7). Series already registered stay writable; only a new key is
	/// refused. Nothing was committed.
	#[error("aspect {aspect:?} already holds {limit} series, the most it may hold; a batch for a new series was refused and nothing was committed (series already registered stay writable)")]
	SeriesLimit {
		/// The aspect the batch was offered to.
		aspect: String,
		/// The per-aspect series cap in force.
		limit: usize,
	},
	/// An ingest batch's `groups` named a tag key with the reserved `__` prefix
	/// ([`RESERVED_TAG_KEY_PREFIX`](crate::tags::RESERVED_TAG_KEY_PREFIX)), which a
	/// [`TagSet`](crate::TagSet) converted from a stored
	/// [`SeriesKey`](crate::SeriesKey) may carry but an ingest may not write
	/// (TAG-8). Nothing was written.
	#[error("aspect {aspect:?}: the tag key {key:?} starts with `__`, which is reserved for system dimensions; nothing was written")]
	ReservedTagKey {
		/// The aspect the batch was offered to.
		aspect: String,
		/// The reserved key the batch named.
		key: String,
	},
	/// An ingest batch's `groups` named a tag key outside the aspect's declared
	/// [`UpsertKeys`](crate::UpsertKeys) (Phase 4.6): with upsert keys declared, every row's
	/// tags must fall inside them, because a tag outside the key has no per-row storage to
	/// carry it once rows that differ only in it fold into one. Nothing was written.
	#[error("aspect {aspect:?}: the tag key {key:?} is not an upsert key (the aspect declares upsert keys {declared}); nothing was written")]
	NotAnUpsertKey {
		/// The aspect the batch was offered to.
		aspect: String,
		/// The tag key outside the declaration.
		key: String,
		/// The declared upsert keys.
		declared: crate::UpsertKeys,
	},
	/// A declaration named upsert keys other than the ones `aspect` is declared with
	/// (Phase 4.6): like the schema, an upsert-key declaration is immutable once made, and
	/// no declaration is a declaration too. Nothing was written.
	#[error("aspect {aspect:?} is declared with upsert keys {} and they are immutable; the offered {} was refused and nothing was written", describe_upsert_keys(declared.as_ref()), describe_upsert_keys(offered.as_ref()))]
	UpsertKeysConflict {
		/// The aspect that was re-declared.
		aspect: String,
		/// The upsert keys it is declared with ([`None`]: undeclared).
		declared: Option<crate::UpsertKeys>,
		/// The upsert keys the refused declaration offered.
		offered: Option<crate::UpsertKeys>,
	},
	/// A [`SeriesSelector`](crate::SeriesSelector) matched more than one series
	/// of `aspect`, and a store read targets exactly one (TAG-8, tags D4). Narrow
	/// the selector, or select exactly. Nothing was read.
	#[error("aspect {aspect:?}: the series selector matched {matched} series, and a read targets exactly one; narrow the selector")]
	AmbiguousSeries {
		/// The aspect the selector was resolved in.
		aspect: String,
		/// How many series it matched (two or more).
		matched: usize,
	},
	/// An ingest batch held no rows. Nothing was written, and the key is not
	/// recorded: an empty batch has no receipt to replay.
	#[error("aspect {aspect:?}: an ingest batch must hold at least one row; nothing was written")]
	EmptyIngestBatch {
		/// The aspect the batch was offered to.
		aspect: String,
	},
	/// An ingest batch's timestamp and value columns differ in length.
	/// Nothing was written.
	#[error("an ingest batch holds {timestamps} timestamps but {values} values; nothing was written")]
	IngestLengthMismatch {
		/// How many timestamps the batch holds.
		timestamps: usize,
		/// How many values the batch holds.
		values: usize,
	},
	/// A value in an ingest batch is outside the one decimal domain (ROB-6):
	/// its plain form carries `digits` digits, over the bound. The fingerprint
	/// is taken over the bounded plain form, so such a value is refused before
	/// any plain form is materialised. Nothing was written.
	#[error("a value in the ingest batch has a plain form of {digits} digits, over the {max}-digit decimal domain bound; nothing was written")]
	IngestValueOutOfRange {
		/// How many digits the value's plain form carries.
		digits: usize,
		/// The bound it exceeded.
		max: usize,
	},
	/// A stored downsample of `aspect` produced more than `limit` buckets —
	/// the decision 12 grid cap in force
	/// ([`SegmentStoreOptions::max_downsample_buckets`](crate::SegmentStoreOptions::max_downsample_buckets))
	/// — and was refused while the fold was running (ROB-13). The partial
	/// state is dropped; nothing was written. A coarser resolution or a
	/// narrower window fits under the cap.
	#[error("the stored downsample of aspect {aspect:?} produced {buckets} buckets, over the limit of {limit}; narrow the window or use a coarser resolution (nothing was written)")]
	DownsampleTooLarge {
		/// The aspect the downsample named.
		aspect: String,
		/// The bucket count the fold had produced when it was refused (the
		/// first count over the limit, not the grid's full width).
		buckets: usize,
		/// The cap in force.
		limit: usize,
	},
	/// A batch point lookup named more instants than the decision 12 cap in
	/// force ([`SegmentStoreOptions::max_point_batch`](crate::SegmentStoreOptions::max_point_batch))
	/// allows (ROB-13). It was refused before the index was pruned; nothing
	/// was read. Split the batch across lookups.
	#[error("the point lookup of aspect {aspect:?} names {instants} instants, over the limit of {limit} per batch; split the batch (nothing was read)")]
	PointBatchTooLarge {
		/// The aspect the lookup named.
		aspect: String,
		/// How many instants the batch named.
		instants: usize,
		/// The cap in force.
		limit: usize,
	},
}
