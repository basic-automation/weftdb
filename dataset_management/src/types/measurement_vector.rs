use bigdecimal::BigDecimal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementVector {
	location: BigDecimal,
	amplitude: BigDecimal,
}

impl MeasurementVector {
	pub fn new(location: BigDecimal, amplitude: BigDecimal) -> Self {
		Self { location, amplitude }
	}

	pub fn location(&self) -> &BigDecimal {
		&self.location
	}

	pub fn amplitude(&self) -> &BigDecimal {
		&self.amplitude
	}

	pub fn set_location(&mut self, location: BigDecimal) {
		self.location = location;
	}

	pub fn set_amplitude(&mut self, amplitude: BigDecimal) {
		self.amplitude = amplitude;
	}
}
