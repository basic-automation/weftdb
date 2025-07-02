use fake::{Dummy, Faker};
use rand::Rng;
use uuid::Uuid;

use crate::Measurement;

#[derive(Debug, Clone)]
pub struct Dataset {
	id: Uuid,
	name: String,
	measurements: Vec<Measurement>,
}

impl Dataset {
	/// Creates a new dataset with a generated ID
	#[must_use]
	pub fn new(name: String) -> Self {
		Self { id: Uuid::new_v4(), name, measurements: vec![] }
	}

	/// Adds a measurement to the dataset
	pub fn add_measurement(&mut self, measurement: Measurement) {
		self.measurements.push(measurement);
	}

	/// Returns the ID of the dataset
	#[must_use]
	pub const fn id(&self) -> Uuid {
		self.id
	}

	/// Sets the ID of the dataset
	pub const fn set_id(&mut self, id: Uuid) {
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
}

// Implement Dummy manually for the types that need it
impl Dummy<Faker> for Dataset {
	fn dummy_with_rng<R: Rng + ?Sized>(_config: &Faker, _rng: &mut R) -> Self {
		use fake::{faker::lorem::en::Word, Fake};

		Self {
			id: Uuid::new_v4(),
			name: Word().fake(),
			measurements: vec![], // Start with empty measurements
		}
	}
}
