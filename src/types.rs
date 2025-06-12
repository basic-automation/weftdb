use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use fake::Dummy;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Dummy)]
pub struct Dataset {
	pub id: Uuid,
	pub name: String,
	pub measurements: Vec<Measurement>,
}

#[derive(Debug, Clone, Dummy, PartialEq, Eq)]
pub struct InputMeasurement {
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
}

#[derive(Debug, Clone, Dummy, PartialEq, Eq)]
pub struct Measurement {
	pub id: Uuid,
	pub dataset_id: Uuid,
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
}

impl Measurement {
	#[must_use] pub fn from_input_measurement(dataset_id: Uuid, input: InputMeasurement) -> Self {
		Self { id: Uuid::new_v4(), dataset_id, timestamp: input.timestamp, value: input.value }
	}
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
