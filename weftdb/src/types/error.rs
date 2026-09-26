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
