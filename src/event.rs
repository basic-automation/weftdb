use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MeasurementEvent {
	pub event_type: String,
	pub key: i64,
	pub bucket: String,
	pub timestamp: i64,
}

impl MeasurementEvent {
	pub fn new(event_type: &str, key: i64, bucket: &str) -> Self {
		Self { event_type: event_type.to_string(), key, bucket: bucket.to_string(), timestamp: chrono::Utc::now().timestamp() }
	}
}