use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum Error {
	#[error("Error reading directory: {0}")]
	ReadingDirectoryError(String),

	#[error("Error creating directory: {0}")]
	CreatingDirectoryError(String),

	#[error("Error creating database: {0}")]
	CreatingDatabaseError(String),

	#[error("Error connecting to database: {0}")]
	ConnectingDatabaseError(String),

	#[error("Invalid path: {0}")]
	InvalidPathError(String),

	#[error("Error initalizing existing database connections: {0}")]
	InitializingExistingConnectionsError(Box<Error>),

	#[error("Error in database execution: {0}")]
	DatabaseExecutionError(String),

	#[error("Unable to parse UUID: {0}")]
	UuidParseError(String),

	#[error("All measurements must have the same dataset_id")]
	InconsistentDatasetIdsError,

	#[error("Start time must be before end time")]
	InvalidTimeRangeError,

	#[error("At least two measurements are required for interpolation or extrapolation")]
	InsufficientMeasurementsError,

	#[error("Need at least 2 points for cubic spline")]
	InsufficientPointsForCubicSplineError,
}
