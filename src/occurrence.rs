use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Occurrence {
	pub measurement_bucket: String,
	pub source: String,
	pub start: BigDecimal,
	pub end: BigDecimal,
	pub id: Uuid,
}

impl Occurrence {
	pub fn new(measurement_bucket: &str, source: &str, start: BigDecimal, end: BigDecimal) -> Self {
		Self { measurement_bucket: measurement_bucket.to_string(), source: source.to_string(), start, end, id: Uuid::new_v4() }
	}
}
