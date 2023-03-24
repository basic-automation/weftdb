use serde::{Deserialize, Serialize};
use bigdecimal::{BigDecimal, FromPrimitive};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeasurementEvent {
	pub event_type: String,
	pub key: BigDecimal,
	pub bucket: String,
	pub timestamp: BigDecimal,
}

impl MeasurementEvent {
	pub fn new(event_type: &str, key: BigDecimal, bucket: &str) -> Self {
		Self { event_type: event_type.to_string(), key, bucket: bucket.to_string(), timestamp: BigDecimal::from_i64(chrono::Utc::now().timestamp()).unwrap() }
	}
}