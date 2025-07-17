use std::fmt::Debug;

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};

#[derive(Clone, PartialEq, Eq)]
pub struct Point {
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
}

impl Debug for Point {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Point {{ timestamp: {}, value: {} }}", self.timestamp, self.value.to_plain_string())
	}
}
