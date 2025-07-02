use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use database::Measurement;

use crate::types::{Analysis, Distance, MeasurementVector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedMeasurement {
	active: bool,
	measurement: Measurement,
	distance: Option<Distance>,
	vector: Option<MeasurementVector>,
	analysis: Option<Analysis>,
}

impl BatchedMeasurement {
	pub fn new(measurement: Measurement) -> Self {
		Self { active: true, measurement, distance: None, vector: None, analysis: None }
	}

	pub fn deactivate(&mut self) {
		self.active = false;
	}

	pub fn activate(&mut self) {
		self.active = true;
	}

	pub fn is_active(&self) -> bool {
		self.active
	}

	pub fn measurement(&self) -> &Measurement {
		&self.measurement
	}

	pub fn set_measurement(&mut self, measurement: Measurement) {
		self.measurement = measurement;
	}

	pub fn get_measurement_value(&self) -> &BigDecimal {
		&self.measurement.value
	}

	pub fn get_measurement_timestamp(&self) -> &DateTime<Utc> {
		&self.measurement.timestamp
	}

	pub fn distance(&self) -> Option<&Distance> {
		self.distance.as_ref()
	}

	pub fn set_distance(&mut self, distance: Distance) {
		self.distance = Some(distance);
	}

	pub fn vector(&self) -> Option<&MeasurementVector> {
		self.vector.as_ref()
	}

	pub fn get_vector_location(&self) -> Option<&BigDecimal> {
		self.vector.as_ref().map(|v| v.location())
	}

	pub fn get_vector_amplitude(&self) -> Option<&BigDecimal> {
		self.vector.as_ref().map(|v| v.amplitude())
	}

	pub fn set_vector(&mut self, vector: MeasurementVector) {
		self.vector = Some(vector);
	}

	pub fn analysis(&self) -> Option<&Analysis> {
		self.analysis.as_ref()
	}

	pub fn set_analysis(&mut self, analysis: Analysis) {
		self.analysis = Some(analysis);
	}
}
