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
}
