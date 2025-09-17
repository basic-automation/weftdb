use std::collections::HashMap;

use anyhow::Result;
use bigdecimal::{BigDecimal, Zero};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::types::{correlation::CorrelationID, event::ManifestationId};

#[derive(Debug, Clone, Eq, Serialize, Deserialize, Hash)]
pub enum SignalType {
	Custom(String),
}

impl PartialEq for SignalType {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(SignalType::Custom(s1), SignalType::Custom(s2)) => s1 == s2,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distance {
	pub value: BigDecimal,
	pub units: Resolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
	pub correlation_id: CorrelationID,
	pub manifestation_id: ManifestationId,
	pub manifestation_date: DateTime<Utc>,
	pub signal_type: SignalType,
	pub distance: Distance,
}

impl Signal {
	pub fn new(correlation_id: CorrelationID, manifestation_id: ManifestationId, manifestation_date: DateTime<Utc>, signal_type: SignalType, distance: Distance) -> Self {
		Self { correlation_id, manifestation_id, manifestation_date, signal_type, distance }
	}

	pub fn distance(&self) -> &Distance {
		&self.distance
	}

	/// probability of the signal at a given date
	/// distance from the event maniestation divided by signal distance, after adjusting for resolution
	pub fn probability(&self, date: DateTime<Utc>, error_rate: BigDecimal) -> Result<BigDecimal> {
		if self.distance.value.is_zero() {
			return Ok(BigDecimal::from(0)); // Avoid division by zero
		}

		let time_diff_duration = self.manifestation_date - date;
		let time_diff = time_diff_duration.num_seconds();

		// Convert time_diff to BigDecimal
		let time_diff_bd = BigDecimal::from(time_diff);

		// Probability is (time_diff / distance) + error_rate
		let probability = (time_diff_bd / &self.distance.value) + error_rate;
		Ok(probability)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrVal {
	pub value: BigDecimal,
	pub error: BigDecimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signals(HashMap<(CorrelationID, ManifestationId, SignalType), Signal>); // Keyed by (CorrelationID, ManifestationId, SignalType) tuple

impl Default for Signals {
	fn default() -> Self {
		Self::new()
	}
}

impl Signals {
	pub fn new() -> Self {
		Signals(HashMap::new())
	}

	pub fn insert(&mut self, signal: Signal) {
		let key = (signal.correlation_id.clone(), signal.manifestation_id.clone(), signal.signal_type.clone());
		self.0.insert(key, signal);
	}

	pub fn get(&self, correlation_id: &CorrelationID, manifestation_id: &ManifestationId, signal_type: &SignalType) -> Option<&Signal> {
		self.0.get(&(correlation_id.clone(), manifestation_id.clone(), signal_type.clone()))
	}

	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	pub fn values(&self) -> impl Iterator<Item = &Signal> {
		self.0.values()
	}

	pub fn probability(&self, correlation_id: &CorrelationID, manifestation_id: &ManifestationId, signal_type: &SignalType, date: DateTime<Utc>, error_rate: &BigDecimal) -> Result<Option<BigDecimal>> {
		if let Some(signal) = self.get(correlation_id, manifestation_id, signal_type) {
			let prob = signal.probability(date, error_rate.clone())?;
			Ok(Some(prob))
		} else {
			Ok(None)
		}
	}
}
