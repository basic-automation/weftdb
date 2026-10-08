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
