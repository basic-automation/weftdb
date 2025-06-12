use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use fake::Dummy;
use uuid::Uuid;

mod error;
pub use error::Error;

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
	#[must_use]
	pub fn from_input_measurement(dataset_id: Uuid, input: InputMeasurement) -> Self {
		Self { id: Uuid::new_v4(), dataset_id, timestamp: input.timestamp, value: input.value }
	}
}
