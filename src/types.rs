use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use fake::Dummy;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Dummy)]
pub struct Dataset {
	pub id: Uuid,
	pub name: String,
	pub measurments: Vec<Measurement>,
}

#[derive(Debug, Clone, Dummy, PartialEq, Eq)]
pub struct Measurement {
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
}

#[derive(Error, Debug)]
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
