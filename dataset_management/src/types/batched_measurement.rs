use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use database::Point;

use crate::types::{Analysis, Distance, MeasurementVector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchedMeasurement {
	active: bool,
	point: Point,
	distance: Option<Distance>,
	vector: Option<MeasurementVector>,
	analysis: Option<Analysis>,
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

	pub fn is_active(&self) -> bool {
		self.active
	}

	pub fn point(&self) -> &Point {
		&self.point
	}

	pub fn set_point(&mut self, point: Point) {
		self.point = point;
	}

	pub fn get_measurement_value(&self) -> &BigDecimal {
		&self.point.value
	}

	pub fn get_measurement_timestamp(&self) -> &DateTime<Utc> {
		&self.point.timestamp
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
