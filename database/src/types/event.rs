use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::database::helpers::safe_i64_to_f64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ManifestationId(Uuid); // Wrapper for event ID

impl Default for ManifestationId {
	fn default() -> Self {
		Self::new()
	}
}

impl ManifestationId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(id: Uuid) -> Self {
		Self(id)
	}

	#[must_use]
	pub const fn to_uuid(&self) -> Uuid {
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
	id: ManifestationId,
	dataset_id: Uuid,
	start: DateTime<Utc>,
	end: DateTime<Utc>,
}

impl Manifestation {
	/// Create a new manifestation with start and end timestamps
	#[must_use]
	pub fn new(dataset_id: Uuid, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { id: ManifestationId::new(), dataset_id, start, end }
	}

	/// Get the duration of this manifestation
	#[must_use]
	pub fn duration(&self) -> chrono::Duration {
		self.end - self.start
	}

	/// Get the midpoint timestamp of this manifestation
	#[must_use]
	pub fn midpoint(&self) -> DateTime<Utc> {
		let duration = self.duration();
		self.start + duration / 2
	}

	/// Check if this manifestation contains a specific timestamp
	#[must_use]
	pub fn contains(&self, timestamp: DateTime<Utc>) -> bool {
		timestamp >= self.start && timestamp <= self.end
	}

	/// Get the duration in days as a floating point number
	///
	/// # Errors
	/// - if the duration cannot be represented in `f64` without precision loss
	pub fn duration_days(&self) -> std::result::Result<f64, crate::Error> {
		let millis = safe_i64_to_f64(self.duration().num_milliseconds())?;
		Ok(millis / (1000.0 * 60.0 * 60.0 * 24.0))
	}

	/// Get the duration in hours as a floating point number
	///
	/// # Errors
	/// - if the duration cannot be represented in `f64` without precision loss
	pub fn duration_hours(&self) -> std::result::Result<f64, crate::Error> {
		let millis = safe_i64_to_f64(self.duration().num_milliseconds())?;
		Ok(millis / (1000.0 * 60.0 * 60.0))
	}

	/// Get the manifestation ID
	#[must_use]
	pub const fn id(&self) -> &ManifestationId {
		&self.id
	}

	/// Get the dataset ID
	#[must_use]
	pub const fn dataset_id(&self) -> &Uuid {
		&self.dataset_id
	}

	/// Set the dataset ID
	pub const fn set_dataset_id(&mut self, dataset_id: Uuid) {
		self.dataset_id = dataset_id;
	}

	/// Get the start time
	#[must_use]
	pub const fn start(&self) -> &DateTime<Utc> {
		&self.start
	}

	/// Set the start time
	pub const fn set_start(&mut self, start: DateTime<Utc>) {
		self.start = start;
	}

	/// Get the end time
	#[must_use]
	pub const fn end(&self) -> &DateTime<Utc> {
		&self.end
	}

	/// Set the end time
	pub const fn set_end(&mut self, end: DateTime<Utc>) {
		self.end = end;
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
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(id: Uuid) -> Self {
		Self(id)
	}

	#[must_use]
	pub const fn to_uuid(&self) -> Uuid {
		self.0
	}
}

impl std::fmt::Display for EventID {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct EventName(String); // Wrapper for event name

impl EventName {
	#[must_use]
	pub const fn new(name: String) -> Self {
		Self(name)
	}
}

impl std::fmt::Display for EventName {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
	id: EventID,
	name: EventName,
	description: Option<String>,
	manifestations: HashMap<ManifestationId, Manifestation>, // Keyed by manifestation ID
}

impl Event {
	#[must_use]
	pub fn new(name: String, description: Option<String>) -> Self {
		let id = EventID::new();
		Self { id, name: EventName::new(name), description, manifestations: HashMap::new() }
	}

	pub fn add_manifestation(&mut self, manifestation: Manifestation) {
		self.manifestations.insert(manifestation.id().clone(), manifestation);
	}

	/// Get the event ID
	#[must_use]
	pub const fn id(&self) -> &EventID {
		&self.id
	}

	/// Set the event ID
	pub const fn set_id(&mut self, id: EventID) {
		self.id = id;
	}

	/// Get the event name
	#[must_use]
	pub const fn name(&self) -> &EventName {
		&self.name
	}

	/// Set the event name
	pub fn set_name(&mut self, name: EventName) {
		self.name = name;
	}

	/// Get the event description
	#[must_use]
	pub const fn description(&self) -> &Option<String> {
		&self.description
	}

	/// Set the event description
	pub fn set_description(&mut self, description: Option<String>) {
		self.description = description;
	}

	/// Get the manifestations
	#[must_use]
	pub const fn manifestations(&self) -> &HashMap<ManifestationId, Manifestation> {
		&self.manifestations
	}

	/// Get mutable access to manifestations
	pub const fn manifestations_mut(&mut self) -> &mut HashMap<ManifestationId, Manifestation> {
		&mut self.manifestations
	}

	/// Set the manifestations
	pub fn set_manifestations(&mut self, manifestations: HashMap<ManifestationId, Manifestation>) {
		self.manifestations = manifestations;
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Events(HashMap<EventID, Event>);

impl Default for Events {
	fn default() -> Self {
		Self::new()
	}
}

impl Events {
	#[must_use]
	pub fn new() -> Self {
		Self(HashMap::new())
	}

	pub fn insert(&mut self, event_id: EventID, event: Event) {
		self.0.insert(event_id, event);
	}

	#[must_use]
	pub fn get(&self, event_id: &EventID) -> Option<&Event> {
		self.0.get(event_id)
	}

	pub fn get_mut(&mut self, event_id: &EventID) -> Option<&mut Event> {
		self.0.get_mut(event_id)
	}

	pub fn keys(&self) -> impl Iterator<Item = &EventID> {
		self.0.keys()
	}

	pub fn values(&self) -> impl Iterator<Item = &Event> {
		self.0.values()
	}

	pub fn iter(&self) -> impl Iterator<Item = (&EventID, &Event)> {
		self.0.iter()
	}

	#[must_use]
	pub fn len(&self) -> usize {
		self.0.len()
	}

	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	pub fn clear(&mut self) {
		self.0.clear();
	}

	#[must_use]
	pub fn get_event_by_name(&self, name: &str) -> Option<&Event> {
		self.0.values().find(|event| event.name.to_string() == name)
	}
}
