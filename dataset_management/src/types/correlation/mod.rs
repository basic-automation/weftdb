use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::event::EventID;
// Import Distance from signal module for error rate types
use super::signal::Distance;
use crate::{types::pattern::Occurrence, PatternID, SignalType};

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

pub type AvgErrorRate = Distance;
pub type SumErrorRate = Distance;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correlation {
	pub id: CorrelationID,
	pub dictionary_id: Uuid,
	pub pattern_id: PatternID,
	pub event_id: EventID,
	pub error_rate: HashMap<SignalType, (AvgErrorRate, SumErrorRate)>, // Average and sum error rates per signal type
	pub occurrences: Vec<Occurrence>,
}

impl Correlation {
	pub fn new(dictionary_id: Uuid, pattern_id: PatternID, event_id: EventID, error_rate: (AvgErrorRate, SumErrorRate), occurrences: Vec<Occurrence>) -> Self {
		let id = CorrelationID::new();
		let mut error_rate_map = HashMap::new();
		// Initialize error rates for all signal types that will be created
		error_rate_map.insert(SignalType::Custom("PredictStart".to_string()), error_rate.clone());
		error_rate_map.insert(SignalType::Custom("PredictMid".to_string()), error_rate.clone());
		error_rate_map.insert(SignalType::Custom("PredictEnd".to_string()), error_rate);
		Self { id, dictionary_id, pattern_id, event_id, error_rate: error_rate_map, occurrences }
	}

	pub fn error_rate(&self) -> &HashMap<SignalType, (AvgErrorRate, SumErrorRate)> {
		&self.error_rate
	}

	pub fn get_error_rate(&self, signal_type: &SignalType) -> Option<&(AvgErrorRate, SumErrorRate)> {
		self.error_rate.get(signal_type)
	}

	pub fn set_error_rate(&mut self, signal_type: SignalType, error_rate: (AvgErrorRate, SumErrorRate)) {
		self.error_rate.insert(signal_type, error_rate);
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

	pub fn get_by_id(&self, correlation_id: &CorrelationID) -> Option<&Correlation> {
		self.0.values().find(|correlation| &correlation.id == correlation_id)
	}

	pub fn get_for_event(&self, event_id: &EventID) -> Vec<(&PatternID, &Correlation)> {
		self.0.iter().filter_map(|((eid, pid), correlation)| if eid == event_id { Some((pid, correlation)) } else { None }).collect()
	}

	pub fn get_for_event_name(&self, event_name: &str) -> Vec<(&PatternID, &Correlation)> {
		self.0.iter().filter_map(|((eid, pid), correlation)| if eid.to_string() == event_name { Some((pid, correlation)) } else { None }).collect()
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
