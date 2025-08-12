use bigdecimal::BigDecimal;
use database::Measurement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trend {
	destination: Point,
	slope: BigDecimal,
}

impl Trend {
	pub fn new(destination: Point, slope: BigDecimal) -> Self {
		Self { destination, slope }
	}

	pub fn destination(&self) -> &Point {
		&self.destination
	}

	pub fn slope(&self) -> &BigDecimal {
		&self.slope
	}

	pub fn set_destination(&mut self, destination: Point) {
		self.destination = destination;
	}

	pub fn set_slope(&mut self, slope: BigDecimal) {
		self.slope = slope;
	}
}
