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

	/// The stable id of the manifestation of `event` that starts at `start` in `dataset_id`.
	///
	/// Timestamps count at the millisecond precision the event tables keep. The end is not
	/// part of the id: a manifestation still under way (the current month of the monthly
	/// detector) is detected again with a later end on every run, and is the same
	/// manifestation, widened (see [`fold_spans`]).
	#[must_use]
	pub fn for_span(event: &EventID, dataset_id: Uuid, start: DateTime<Utc>) -> Self {
		let key = format!("{}/{dataset_id}/{}", event.to_uuid(), start.timestamp_millis());
		Self(Uuid::new_v5(&MANIFESTATION_ID_NAMESPACE, key.as_bytes()))
	}
}

/// Whether a span starting at `b_start` is the same occurrence as `[a_start, a_end]`.
///
/// The later span must start no earlier than `a_start`. It coincides on the same start (a manifestation still under way, detected
/// again with a later end), or an overlap (the same crossing found again on an
/// interpolation grid a fraction of a step away, as after a restart re-scans from the
/// first measurement). Spans that only touch at an end point are distinct occurrences.
#[must_use]
pub fn spans_coincide<T: Ord>(a_start: &T, a_end: &T, b_start: &T) -> bool {
	b_start == a_start || b_start < a_end
}

/// Fold `spans` (key, start, end, payload) so each occurrence appears once per key.
///
/// Spans of one key that [`spans_coincide`] become their union, keeping the payload of the span
/// that starts first. Answers the folded spans ordered by start. Nothing is dropped but
/// the repeats: the union covers every folded span.
#[must_use]
pub fn fold_spans<K: Eq + std::hash::Hash + Clone, T: Ord + Copy, P>(mut spans: Vec<(K, T, T, P)>) -> Vec<(K, T, T, P)> {
	spans.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));
	let mut out: Vec<(K, T, T, P)> = Vec::with_capacity(spans.len());
	let mut last: HashMap<K, usize> = HashMap::new();
	for (key, start, end, payload) in spans {
		if let Some(&index) = last.get(&key) {
			let held = &mut out[index];
			if spans_coincide(&held.1, &held.2, &start) {
				held.2 = held.2.max(end);
				continue;
			}
		}
		last.insert(key.clone(), out.len());
		out.push((key, start, end, payload));
	}
	out
}

/// Namespace of [`EventID::for_name`] ids (a fixed, arbitrary UUID).
const EVENT_ID_NAMESPACE: Uuid = Uuid::from_u128(0x5f0e_2a7c_9d41_4b8e_a3c6_71d2_e0b9_4f15);
/// Namespace of [`ManifestationId::for_span`] ids (a fixed, arbitrary UUID).
const MANIFESTATION_ID_NAMESPACE: Uuid = Uuid::from_u128(0x8c3b_6e19_f2a4_4d07_b5e8_2c91_7a6d_03fe);

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

	/// Create a manifestation with a specific ID (for loading from database)
	#[must_use]
	pub const fn with_id(id: ManifestationId, dataset_id: Uuid, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
		Self { id, dataset_id, start, end }
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

	/// The stable id of the event named `name` on aspect `aspect_id`.
	///
	/// An event is one recurring thing (a detector's crossing, peak, drawdown...) that
	/// collects a manifestation each time it recurs; its average distance, and so the expiry
	/// of its forward signals, needs two or more of them on one event. Every run of a detector
	/// lands on this id, across restarts, so its manifestations accumulate on one event
	/// instead of each run adding an event of its own.
	#[must_use]
	pub fn for_name(aspect_id: Uuid, name: &str) -> Self {
		let key = format!("{aspect_id}/{name}");
		Self(Uuid::new_v5(&EVENT_ID_NAMESPACE, key.as_bytes()))
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
	pub fn new(id: Option<EventID>, name: String, description: Option<String>, manifestations: Option<HashMap<ManifestationId, Manifestation>>) -> Self {
		let id = id.unwrap_or_default();
		Self { id, name: EventName::new(name), description, manifestations: manifestations.unwrap_or_default() }
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

	/// This event under its stable identity on `aspect_id`: the id [`EventID::for_name`]
	/// gives its name, and each manifestation under the id [`ManifestationId::for_span`]
	/// gives its start. Manifestations of one occurrence (see [`spans_coincide`]) become
	/// one, their union. Storing the result again, or the same detection from a later run,
	/// adds nothing already stored.
	#[must_use]
	pub fn into_stable(mut self, aspect_id: Uuid) -> Self {
		let id = EventID::for_name(aspect_id, &self.name.0);
		let spans: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>, ())> = self.manifestations.into_values().map(|m| (m.dataset_id, m.start, m.end, ())).collect();
		self.manifestations = fold_spans(spans)
			.into_iter()
			.map(|(dataset_id, start, end, ())| {
				let stable = ManifestationId::for_span(&id, dataset_id, start);
				(stable.clone(), Manifestation { id: stable, dataset_id, start, end })
			})
			.collect();
		self.id = id;
		self
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

#[cfg(test)]
mod tests {
	use chrono::TimeZone;

	use super::*;

	#[test]
	fn the_same_detection_gets_the_same_ids() {
		let aspect = Uuid::new_v4();
		let dataset = Uuid::new_v4();
		let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
		let detect = || {
			let mut event = Event::new(None, "crossing".into(), None, None);
			event.add_manifestation(Manifestation::new(dataset, start, start + chrono::Duration::minutes(5)));
			// The same span twice, under another random id.
			event.add_manifestation(Manifestation::new(dataset, start, start + chrono::Duration::minutes(5)));
			event.into_stable(aspect)
		};
		let (a, b) = (detect(), detect());
		assert_eq!(a.id(), b.id());
		assert_eq!(a.id(), &EventID::for_name(aspect, "crossing"));
		assert_eq!(a.manifestations().len(), 1, "one span is one manifestation");
		assert_eq!(a.manifestations().keys().collect::<Vec<_>>(), b.manifestations().keys().collect::<Vec<_>>());
		assert_ne!(EventID::for_name(Uuid::new_v4(), "crossing"), *a.id(), "another aspect's event is another event");
	}

	#[test]
	fn one_occurrence_seen_twice_folds_into_its_union() {
		let dataset = Uuid::new_v4();
		let at = |m: i64| Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::minutes(m);
		let mut event = Event::new(None, "crossing".into(), None, None);
		// A month under way, detected with a later end.
		event.add_manifestation(Manifestation::new(dataset, at(0), at(10)));
		event.add_manifestation(Manifestation::new(dataset, at(0), at(20)));
		// A crossing found again on a grid a fraction of a step away.
		event.add_manifestation(Manifestation::new(dataset, at(60), at(65)));
		event.add_manifestation(Manifestation::new(dataset, at(62), at(67)));
		// Touching at an end point, or in another dataset: distinct occurrences.
		event.add_manifestation(Manifestation::new(dataset, at(67), at(70)));
		event.add_manifestation(Manifestation::new(Uuid::new_v4(), at(0), at(20)));
		let event = event.into_stable(Uuid::new_v4());
		let mut spans: Vec<(DateTime<Utc>, DateTime<Utc>)> = event.manifestations().values().filter(|m| *m.dataset_id() == dataset).map(|m| (*m.start(), *m.end())).collect();
		spans.sort();
		assert_eq!(spans, vec![(at(0), at(20)), (at(60), at(67)), (at(67), at(70))]);
		assert_eq!(event.manifestations().len(), 4);
	}
}
