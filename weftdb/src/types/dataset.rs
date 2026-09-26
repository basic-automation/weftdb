use std::{fmt::Display, str::FromStr};

use fake::{faker::lorem::en::Word, Fake};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Measurement;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DatasetId(Uuid);

impl DatasetId {
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
}

impl Default for DatasetId {
	fn default() -> Self {
		Self::new()
	}
}

impl Display for DatasetId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl FromStr for DatasetId {
	type Err = uuid::Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Uuid::parse_str(s).map(Self)
	}
}

#[derive(Debug, Clone)]
pub struct Dataset {
	id: DatasetId,
	name: String,
	measurements: Vec<Measurement>,
}

impl Dataset {
	/// Creates a new dataset with a generated ID
	#[must_use]
	pub fn new(name: String) -> Self {
		Self { id: DatasetId::new(), name, measurements: vec![] }
	}

	/// Adds a measurement to the dataset
	pub fn add_measurement(&mut self, measurement: Measurement) {
		self.measurements.push(measurement);
	}

	/// Returns the ID of the dataset
	#[must_use]
	pub const fn id(&self) -> &DatasetId {
		&self.id
	}

	/// Sets the ID of the dataset
	pub const fn set_id(&mut self, id: DatasetId) {
		self.id = id;
	}

	/// Returns the name of the dataset
	#[must_use]
	pub const fn name(&self) -> &String {
		&self.name
	}

	/// Sets the name of the dataset
	pub fn set_name(&mut self, name: String) {
		self.name = name;
	}

	/// Returns the measurements in the dataset
	#[must_use]
	pub const fn measurements(&self) -> &Vec<Measurement> {
		&self.measurements
	}

	pub const fn measurements_mut(&mut self) -> &mut Vec<Measurement> {
		&mut self.measurements
	}

	pub fn set_measurements(&mut self, measurements: Vec<Measurement>) {
		self.measurements = measurements;
	}

	/// Generate a random Dataset for testing
	#[must_use]
	pub fn random() -> Self {
		Self {
			id: DatasetId::new(),
			name: Word().fake(),
			measurements: vec![], // Start with empty measurements
		}
	}
}
