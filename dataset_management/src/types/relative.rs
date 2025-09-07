use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

use crate::types::MeasurementVector;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relative {
	vector: MeasurementVector,
	max_x: BigDecimal,
	max_y: BigDecimal,
}

impl Relative {
	pub fn new(vector: MeasurementVector, max_x: BigDecimal, max_y: BigDecimal) -> Self {
		Self { vector, max_x, max_y }
	}

	pub fn vector(&self) -> &MeasurementVector {
		&self.vector
	}

	pub fn max_x(&self) -> &BigDecimal {
		&self.max_x
	}

	pub fn max_y(&self) -> &BigDecimal {
		&self.max_y
	}

	pub fn set_vector(&mut self, vector: MeasurementVector) {
		self.vector = vector;
	}

	pub fn set_max_x(&mut self, max_x: BigDecimal) {
		self.max_x = max_x;
	}

	pub fn set_max_y(&mut self, max_y: BigDecimal) {
		self.max_y = max_y;
	}
}
