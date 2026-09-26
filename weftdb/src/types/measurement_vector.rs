use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementVector {
	location: BigDecimal,
	amplitude: BigDecimal,
}

impl MeasurementVector {
	#[must_use]
	pub const fn new(location: BigDecimal, amplitude: BigDecimal) -> Self {
		Self { location, amplitude }
	}

	#[must_use]
	pub const fn location(&self) -> &BigDecimal {
		&self.location
	}

	#[must_use]
	pub const fn amplitude(&self) -> &BigDecimal {
		&self.amplitude
	}

	pub fn set_location(&mut self, location: BigDecimal) {
		self.location = location;
	}

	pub fn set_amplitude(&mut self, amplitude: BigDecimal) {
		self.amplitude = amplitude;
	}
}
