use std::collections::HashMap;

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::event::EventID;
use crate::{types::pattern::Occurrence, PatternID};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct CorrelationID(Uuid); // Wrapper for event ID

impl Default for CorrelationID {
	fn default() -> Self {
		Self::new()
	}
}

impl CorrelationID {
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

impl std::fmt::Display for CorrelationID {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.to_uuid())
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correlation {
	pub id: CorrelationID,
	pub dictionary_id: Uuid,
	pub pattern_id: PatternID,
	pub event_id: EventID,
	pub error_rate: BigDecimal,
	pub occurrences: Vec<Occurrence>,
}

impl Correlation {
	pub fn new(dictionary_id: Uuid, pattern_id: PatternID, event_id: EventID, error_rate: BigDecimal, occurrences: Vec<Occurrence>) -> Self {
		let id = CorrelationID::new();
		Self { id, dictionary_id, pattern_id, event_id, error_rate, occurrences }
	}

	pub fn error_rate(&self) -> &BigDecimal {
		&self.error_rate
	}

	pub fn occurrences(&self) -> &Vec<Occurrence> {
		&self.occurrences
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
	pub fn new() -> Self {
		Correlations(HashMap::new())
	}

	pub fn insert(&mut self, event_id: EventID, pattern_id: PatternID, correlation: Correlation) {
		self.0.insert((event_id, pattern_id), correlation);
	}

	pub fn get(&self, event_id: &EventID, pattern_id: &PatternID) -> Option<&Correlation> {
		self.0.get(&(event_id.clone(), *pattern_id))
	}

	pub fn get_for_event(&self, event_id: &EventID) -> Vec<(&PatternID, &Correlation)> {
		self.0.iter().filter_map(|((eid, pid), correlation)| if eid == event_id { Some((pid, correlation)) } else { None }).collect()
	}

	pub fn count_for_event(&self, event_id: &EventID) -> usize {
		self.0.iter().filter(|((eid, _), _)| eid == event_id).count()
	}

	pub fn remove(&mut self, event_id: &EventID, pattern_id: &PatternID) -> Option<Correlation> {
		self.0.remove(&(event_id.clone(), *pattern_id))
	}

	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

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
