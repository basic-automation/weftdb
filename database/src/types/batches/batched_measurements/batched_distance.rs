use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchDistance {
	positive: BigDecimal,
	negative: BigDecimal,
}

impl BatchDistance {
	#[must_use]
	pub const fn new(positive: BigDecimal, negative: BigDecimal) -> Self {
		Self { positive, negative }
	}

	#[must_use]
	pub const fn positive(&self) -> &BigDecimal {
		&self.positive
	}

	#[must_use]
	pub const fn negative(&self) -> &BigDecimal {
		&self.negative
	}

	pub fn set_positive(&mut self, positive: BigDecimal) {
		self.positive = positive;
	}

	pub fn set_negative(&mut self, negative: BigDecimal) {
		self.negative = negative;
	}
}
