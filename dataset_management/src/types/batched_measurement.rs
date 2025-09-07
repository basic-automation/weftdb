use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Point;

use crate::types::{Analysis, Distance, MeasurementVector}; // Added Analysis to the import

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchedMeasurement {
	pub active: bool,
	pub point: Point,
	pub distance: Option<Distance>,
	pub vector: Option<MeasurementVector>,
	pub analysis: Option<Analysis>,
}

impl BatchedMeasurement {
	pub fn new(point: Point) -> Self {
		Self { active: true, point, distance: None, vector: None, analysis: None }
	}

	pub fn deactivate(&mut self) {
		self.active = false;
	}

	pub fn activate(&mut self) {
		self.active = true;
	}

	pub const fn is_active(&self) -> bool {
		self.active
	}

	pub const fn point(&self) -> &Point {
		&self.point
	}

	pub fn set_point(&mut self, point: Point) {
		self.point = point;
	}

	pub const fn distance(&self) -> Option<&Distance> {
		self.distance.as_ref()
	}

	pub fn set_distance(&mut self, distance: Distance) {
		self.distance = Some(distance);
	}

	pub const fn vector(&self) -> Option<&MeasurementVector> {
		self.vector.as_ref()
	}

	pub fn vector_mut(&mut self) -> Option<&mut MeasurementVector> {
		self.vector.as_mut()
	}

	pub fn set_vector(&mut self, vector: MeasurementVector) {
		self.vector = Some(vector);
	}

	pub fn get_measurement_value(&self) -> &BigDecimal {
		&self.point.value
	}

	pub const fn get_measurement_timestamp(&self) -> &DateTime<Utc> {
		&self.point.timestamp
	}

	pub fn get_vector_location(&self) -> Option<&BigDecimal> {
		self.vector.as_ref().map(|v| v.location())
	}

	pub fn get_vector_amplitude(&self) -> Option<&BigDecimal> {
		self.vector.as_ref().map(|v| v.amplitude())
	}

	pub const fn analysis(&self) -> Option<&Analysis> {
		self.analysis.as_ref()
	}

	pub fn set_analysis(&mut self, analysis: Analysis) {
		self.analysis = Some(analysis);
	}
}
