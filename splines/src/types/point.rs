use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
	pub timestamp: DateTime<Utc>,
	pub value: BigDecimal,
}
