use bigdecimal::{BigDecimal, FromPrimitive};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeasurementEvent {
	pub event_type: String,
	pub key: BigDecimal,
	pub source: Option<String>,
	pub timestamp: BigDecimal,
}

impl MeasurementEvent {
	pub fn new(event_type: &str, key: BigDecimal, source: Option<String>) -> Self {
		Self { event_type: event_type.to_string(), key, source, timestamp: BigDecimal::from_i64(chrono::Utc::now().timestamp()).unwrap() }
	}
}
