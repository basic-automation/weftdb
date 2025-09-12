use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::pattern::Occurrence;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifestation {
	pub dataset_id: Uuid,
	pub start: DateTime<Utc>,
	pub end: DateTime<Utc>,
}

impl Manifestation {
	/// Create a new manifestation with start and end timestamps
	pub fn new(dataset_id: Uuid, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { dataset_id, start, end }
	}

	/// Get the duration of this manifestation
	pub fn duration(&self) -> chrono::Duration {
		self.end - self.start
	}

	/// Get the midpoint timestamp of this manifestation
	pub fn midpoint(&self) -> DateTime<Utc> {
		let duration = self.duration();
		self.start + duration / 2
	}

	/// Check if this manifestation contains a specific timestamp
	pub fn contains(&self, timestamp: DateTime<Utc>) -> bool {
		timestamp >= self.start && timestamp <= self.end
	}

	/// Get the duration in days as a floating point number
	pub fn duration_days(&self) -> f64 {
		self.duration().num_milliseconds() as f64 / (1000.0 * 60.0 * 60.0 * 24.0)
	}

	/// Get the duration in hours as a floating point number
	pub fn duration_hours(&self) -> f64 {
		self.duration().num_milliseconds() as f64 / (1000.0 * 60.0 * 60.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
	pub distance: BigDecimal,
	pub average: ErrVal,
	pub sum: ErrVal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrVal {
	pub value: BigDecimal,
	pub error: BigDecimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correlation {
	pub dictionary_id: Uuid,
	pub pattern_id: Uuid,
	error_rate: BigDecimal,
	signal: Signal,
	occurrences: Vec<Occurrence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
	pub id: Uuid,
	pub name: String,
	pub description: Option<String>,
	pub manifestations: Vec<Manifestation>,
	pub correlations: Vec<Correlation>,
}

impl Event {
	pub fn new(id: Uuid, name: String, description: Option<String>, manifestations: Vec<Manifestation>, correlations: Vec<Correlation>) -> Self {
		Self { id, name, description, manifestations, correlations }
	}
}
