use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

// Import Distance from signal module for error rate types
use crate::types::{dictionary::DictionaryId, signal::Distance};
use crate::{
	types::{aspect::AspectId, event::EventID, subject::SubjectId}, Occurrence, PatternID, SignalType
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct CorrelationID(Uuid); // Wrapper for event ID

impl Default for CorrelationID {
	fn default() -> Self {
		Self::new()
	}
}

impl CorrelationID {
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

impl std::fmt::Display for CorrelationID {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.to_uuid())
	}
}

pub type ErrorRate = Distance;

// Custom serialization for HashMap<SignalType, ErrorRate> to handle enum keys in JSON
fn serialize_error_rates<S>(error_rates: &HashMap<SignalType, ErrorRate>, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	use serde::ser::SerializeMap;
	let mut map = serializer.serialize_map(Some(error_rates.len()))?;
	for (signal_type, error_rate) in error_rates {
		// Convert SignalType to string for JSON key
		let key = match signal_type {
			SignalType::Custom(ref name) => format!("Custom({name})"),
		};
		map.serialize_entry(&key, error_rate)?;
	}
	map.end()
}

// Custom deserialization for HashMap<SignalType, ErrorRate> to handle enum keys from JSON
fn deserialize_error_rates<'de, D>(deserializer: D) -> Result<HashMap<SignalType, ErrorRate>, D::Error>
where
	D: Deserializer<'de>,
{
	use serde::de::Error;
	let string_map: HashMap<String, ErrorRate> = HashMap::deserialize(deserializer)?;
	let mut result = HashMap::new();

	for (key, value) in string_map {
		// Convert string key back to SignalType
		let signal_type = if let Some(name) = key.strip_prefix("Custom(").and_then(|s| s.strip_suffix(')')) {
			SignalType::Custom(name.to_string())
		} else {
			return Err(D::Error::custom(format!("Invalid SignalType key: {key}")));
		};
		result.insert(signal_type, value);
	}

	Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correlation {
	id: CorrelationID,
	dictionary_id: DictionaryId,
	subject_id: SubjectId,
	aspect_id: AspectId,
	pattern_id: PatternID,
	event_id: EventID,
	#[serde(serialize_with = "serialize_error_rates", deserialize_with = "deserialize_error_rates")]
	error_rate: HashMap<SignalType, ErrorRate>,
	occurrences: Vec<Occurrence>,
}

impl Correlation {
	#[must_use]
	#[allow(clippy::too_many_arguments)]
	pub fn new(id: Option<CorrelationID>, dictionary_id: DictionaryId, subject_id: SubjectId, aspect_id: &AspectId, pattern_id: PatternID, event_id: EventID, error_rate: HashMap<SignalType, ErrorRate>, occurrences: Vec<Occurrence>) -> Self {
		let id = id.unwrap_or_default();
		Self { id, dictionary_id, subject_id, aspect_id: *aspect_id, pattern_id, event_id, error_rate, occurrences }
	}

	#[must_use]
	pub const fn error_rate(&self) -> &HashMap<SignalType, ErrorRate> {
		&self.error_rate
	}

	#[must_use]
	pub fn get_error_rate(&self, signal_type: &SignalType) -> Option<&ErrorRate> {
		self.error_rate.get(signal_type)
	}

	pub fn set_error_rate(&mut self, signal_type: SignalType, error_rate: ErrorRate) {
		self.error_rate.insert(signal_type, error_rate);
	}

	#[must_use]
	pub const fn occurrences(&self) -> &Vec<Occurrence> {
		&self.occurrences
	}

	#[must_use]
	pub const fn id(&self) -> &CorrelationID {
		&self.id
	}

	#[must_use]
	pub const fn dictionary_id(&self) -> &DictionaryId {
		&self.dictionary_id
	}

	#[must_use]
	pub const fn aspect_id(&self) -> &AspectId {
		&self.aspect_id
	}

	#[must_use]
	pub const fn pattern_id(&self) -> &PatternID {
		&self.pattern_id
	}

	#[must_use]
	pub const fn event_id(&self) -> &EventID {
		&self.event_id
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correlations(HashMap<(EventID, PatternID), Correlation>); // Keyed by (event_id, pattern_id) tuple

impl Default for Correlations {
	fn default() -> Self {
		Self::new()
	}
}

impl Correlations {
	#[must_use]
	pub fn new() -> Self {
		Self(HashMap::new())
	}

	pub fn insert(&mut self, event_id: EventID, pattern_id: PatternID, correlation: Correlation) {
		self.0.insert((event_id, pattern_id), correlation);
	}

	#[must_use]
	pub fn get(&self, event_id: &EventID, pattern_id: &PatternID) -> Option<&Correlation> {
		self.0.get(&(event_id.clone(), *pattern_id))
	}

	#[must_use]
	pub fn get_by_id(&self, correlation_id: &CorrelationID) -> Option<&Correlation> {
		self.0.values().find(|correlation| correlation.id() == correlation_id)
	}

	#[must_use]
	pub fn get_for_event(&self, event_id: &EventID) -> Vec<(&PatternID, &Correlation)> {
		self.0.iter().filter_map(|((eid, pid), correlation)| if eid == event_id { Some((pid, correlation)) } else { None }).collect()
	}

	#[must_use]
	pub fn get_for_event_name(&self, event_name: &str) -> Vec<(&PatternID, &Correlation)> {
		self.0.iter().filter_map(|((eid, pid), correlation)| if eid.to_string() == event_name { Some((pid, correlation)) } else { None }).collect()
	}

	#[must_use]
	pub fn count_for_event(&self, event_id: &EventID) -> usize {
		self.0.iter().filter(|((eid, _), _)| eid == event_id).count()
	}

	pub fn remove(&mut self, event_id: &EventID, pattern_id: &PatternID) -> Option<Correlation> {
		self.0.remove(&(event_id.clone(), *pattern_id))
	}

	#[must_use]
	pub fn len(&self) -> usize {
		self.0.len()
	}

	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	#[must_use]
	pub fn get_index(&self, index: usize) -> Option<&Correlation> {
		self.0.values().nth(index)
	}

	pub fn values(&self) -> impl Iterator<Item = &Correlation> {
		self.0.values()
	}

	pub fn iter(&self) -> impl Iterator<Item = ((&EventID, &PatternID), &Correlation)> {
		self.0.iter().map(|((event_id, pattern_id), correlation)| ((event_id, pattern_id), correlation))
	}
}
