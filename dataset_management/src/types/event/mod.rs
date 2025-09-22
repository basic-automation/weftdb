use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ManifestationId(Uuid); // Wrapper for event ID

impl Default for ManifestationId {
	fn default() -> Self {
		Self::new()
	}
}

impl ManifestationId {
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	pub fn from_uuid(id: Uuid) -> Self {
		Self(id)
	}

	pub fn to_uuid(&self) -> Uuid {
		self.0
	}
}

impl std::fmt::Display for ManifestationId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.to_uuid())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifestation {
	pub id: ManifestationId,
	pub dataset_id: Uuid,
	pub start: DateTime<Utc>,
	pub end: DateTime<Utc>,
}

impl Manifestation {
	/// Create a new manifestation with start and end timestamps
	pub fn new(dataset_id: Uuid, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { id: ManifestationId::new(), dataset_id, start, end }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct EventID(Uuid); // Wrapper for event ID

impl Default for EventID {
	fn default() -> Self {
		Self::new()
	}
}

impl EventID {
	pub fn new() -> Self {
		EventID(Uuid::new_v4())
	}

	pub fn from_uuid(id: Uuid) -> Self {
		EventID(id)
	}

	pub fn to_uuid(&self) -> Uuid {
		self.0
	}
}

impl std::fmt::Display for EventID {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
	pub id: EventID,
	pub name: String,
	pub description: Option<String>,
	pub manifestations: HashMap<(DateTime<Utc>, DateTime<Utc>), Manifestation>, // Keyed by (start, end) tuple
}

impl Event {
	pub fn new(name: String, description: Option<String>) -> Self {
		let id = EventID::new();
		Self { id, name, description, manifestations: HashMap::new() }
	}

	pub fn add_manifestation(&mut self, manifestation: Manifestation) {
		self.manifestations.entry((manifestation.start, manifestation.end)).or_insert(manifestation);
	}
}
