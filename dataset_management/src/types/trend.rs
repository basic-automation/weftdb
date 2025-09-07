use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use splimes::Point;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

	pub fn set_slope(&mut self, slope: BigDecimal) {
		self.slope = slope;
	}

	pub fn set_destination(&mut self, destination: Point) {
		self.destination = destination;
	}
}
