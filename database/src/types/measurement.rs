use std::fmt::Display;

use bigdecimal::{BigDecimal, FromPrimitive};
use chrono::{TimeZone, Utc};
use fake::Dummy;
use rand::Rng;
use uuid::Uuid;
use serde::Deserialize;
use serde::Serialize;

use crate::{DatasetId, InputMeasurement};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MeasurementId(Uuid);

impl MeasurementId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	#[must_use]
	pub const fn as_uuid(&self) -> Uuid {
		self.0
	}

	/// Create a `PatternID` from a string representation
	///
	/// # Errors
	/// Returns an error if the string is not a valid UUID
	pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
		Uuid::parse_str(s).map(Self)
	}
}

impl Default for MeasurementId {
	fn default() -> Self {
		Self::new()
	}
}

impl Display for MeasurementId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
	id: MeasurementId,
	dataset_id: DatasetId,
	timestamp: chrono::DateTime<Utc>,
	value: BigDecimal,
}

impl Measurement {
	/// Creates a new measurement from an input measurement with a generated ID
	#[must_use]
	pub fn from_input_measurement(dataset_id: DatasetId, input: &InputMeasurement) -> Self {
		Self { id: MeasurementId::new(), dataset_id, timestamp: input.timestamp(), value: input.value().clone() }
	}

	/// Generate fake measurement data for testing
	#[must_use]
	pub fn fake(dataset_id: DatasetId) -> Self {
		let mut rng = rand::thread_rng();
		Self { id: MeasurementId::new(), dataset_id, timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.gen_range(0..86400)), value: BigDecimal::from_f64(rng.gen_range(0.0..100.0)).unwrap_or_default() }
	}

	#[must_use]
	pub const fn new(id: MeasurementId, dataset_id: DatasetId, timestamp: chrono::DateTime<Utc>, value: BigDecimal) -> Self {
		Self { id, dataset_id, timestamp, value }
	}

	#[must_use]
	pub const fn id(&self) -> MeasurementId {
		self.id
	}

	#[must_use]
	pub const fn dataset_id(&self) -> DatasetId {
		self.dataset_id
	}

	#[must_use]
	pub const fn timestamp(&self) -> chrono::DateTime<Utc> {
		self.timestamp
	}

	#[must_use]
	pub const fn value(&self) -> &BigDecimal {
		&self.value
	}

	pub const fn set_id(&mut self, id: MeasurementId) {
		self.id = id;
	}

	pub const fn set_dataset_id(&mut self, dataset_id: DatasetId) {
		self.dataset_id = dataset_id;
	}

	pub const fn set_timestamp(&mut self, timestamp: chrono::DateTime<Utc>) {
		self.timestamp = timestamp;
	}

	pub fn set_value(&mut self, value: BigDecimal) {
		self.value = value;
	}

	/// Converts the measurement to an input measurement
	#[must_use]
	pub fn to_input_measurement(&self) -> InputMeasurement {
		InputMeasurement::new(self.timestamp, self.value.clone())
	}
}

impl<T> Dummy<T> for Measurement {
	fn dummy_with_rng<R: Rng + ?Sized>(_config: &T, rng: &mut R) -> Self {
		let timestamp = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::seconds(rng.gen_range(0..86400));
		let value = BigDecimal::from_f64(rng.gen_range(0.0..100.0)).unwrap_or_else(|| BigDecimal::from(50));

		Self { id: MeasurementId::new(), dataset_id: DatasetId::new(), timestamp, value }
	}
}
