use bigdecimal::{BigDecimal, FromPrimitive};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeasurementEvent {
	pub event_type: String,
	pub key: BigDecimal,
	pub dataset_name: Option<String>,
	pub timestamp: BigDecimal,
}

impl MeasurementEvent {
	pub fn new(event_type: &str, key: BigDecimal, dataset_name: Option<String>) -> Self {
		Self { event_type: event_type.to_string(), key, dataset_name, timestamp: BigDecimal::from_i64(chrono::Utc::now().timestamp()).unwrap() }
	}
}
