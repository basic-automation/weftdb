use thiserror::Error as ThisError;

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

/// Check if an anyhow error contains a transient MVCC error
pub fn is_transient_mvcc_error(err: &anyhow::Error) -> bool {
	err.downcast_ref::<Error>().is_some_and(Error::is_transient_mvcc)
}

/// Why a [`SegmentStore`](crate::SegmentStore) refused to open its root (freeze design
/// §4.3, FRE-12a), before it wrote anything a newer or damaged store could not survive.
///
/// It arrives inside an [`anyhow::Error`]; `downcast_ref::<StoreError>()` recovers it,
/// whatever context the open added.
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
}
