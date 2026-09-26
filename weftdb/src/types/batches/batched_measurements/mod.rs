use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Point;

use self::batched_distance::BatchDistance;
use crate::types::{Analysis, MeasurementVector};

pub mod analysis;
pub mod batched_distance;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchedMeasurement {
	active: bool,
	point: Point,
	distance: Option<BatchDistance>,
	vector: Option<MeasurementVector>,
	analysis: Option<Analysis>,
}

impl BatchedMeasurement {
	#[must_use]
	pub const fn new(point: Point) -> Self {
		Self { active: true, point, distance: None, vector: None, analysis: None }
	}

	pub const fn deactivate(&mut self) {
		self.active = false;
	}

	pub const fn activate(&mut self) {
		self.active = true;
	}

	#[must_use]
	pub const fn is_active(&self) -> bool {
		self.active
	}

	#[must_use]
	pub const fn point(&self) -> &Point {
		&self.point
	}

	pub fn set_point(&mut self, point: Point) {
		self.point = point;
	}

	#[must_use]
	pub const fn distance(&self) -> Option<&BatchDistance> {
		self.distance.as_ref()
	}

	pub fn set_distance(&mut self, distance: BatchDistance) {
		self.distance = Some(distance);
	}

	#[must_use]
	pub const fn vector(&self) -> Option<&MeasurementVector> {
		self.vector.as_ref()
	}

	pub const fn vector_mut(&mut self) -> Option<&mut MeasurementVector> {
		self.vector.as_mut()
	}

	pub fn set_vector(&mut self, vector: MeasurementVector) {
		self.vector = Some(vector);
	}

	#[must_use]
	pub const fn get_measurement_value(&self) -> &BigDecimal {
		&self.point.value
	}

	#[must_use]
	pub const fn get_measurement_timestamp(&self) -> &DateTime<Utc> {
		&self.point.timestamp
	}

	#[must_use]
	pub fn get_vector_location(&self) -> Option<&BigDecimal> {
		self.vector.as_ref().map(crate::types::MeasurementVector::location)
	}

	#[must_use]
	pub fn get_vector_amplitude(&self) -> Option<&BigDecimal> {
		self.vector.as_ref().map(crate::types::MeasurementVector::amplitude)
	}

	#[must_use]
	pub const fn analysis(&self) -> Option<&Analysis> {
		self.analysis.as_ref()
	}

	pub fn set_analysis(&mut self, analysis: Analysis) {
		self.analysis = Some(analysis);
	}
}
