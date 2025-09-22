use std::collections::HashMap;

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::{
	types::{
		correlation::{AvgErrorRate, Correlation, CorrelationID, SumErrorRate}, event::ManifestationId
	}, EventID
};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub enum SignalType {
	Custom(String),
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
	pub event_id: EventID,
	pub manifestation_date: DateTime<Utc>,
	pub signal_type: SignalType,
	pub distance: Distance,
}

impl Signal {
	pub fn new(correlation_id: CorrelationID, manifestation_id: ManifestationId, event_id: EventID, manifestation_date: DateTime<Utc>, signal_type: SignalType, distance: Distance) -> Self {
		Self { correlation_id, manifestation_id, event_id, manifestation_date, signal_type, distance }
	}

	pub fn distance(&self) -> &Distance {
		&self.distance
	}

	/// probability of the signal at a given date
	/// distance from the event maniestation divided by signal distance, after adjusting for resolution
	pub fn probability(&self, date: DateTime<Utc>, error_rate: BigDecimal) -> Result<BigDecimal> {
		use bigdecimal::FromPrimitive;

		// Check for division by zero
		if self.distance.value.is_zero() {
			return Ok(BigDecimal::zero()); // Avoid division by zero
		}

		let time_diff = match BigDecimal::from_i64(self.distance.units.difference(&self.manifestation_date, &date)?) {
			Some(diff) => diff.abs(),
			None => bail!("Failed to convert time difference to BigDecimal"),
		};

		// Probability is (time_diff / distance) + error_rate
		let probability = (&time_diff / &self.distance.value) + &error_rate;

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

	pub fn remove(&mut self, correlation_id: &CorrelationID, manifestation_id: &ManifestationId, signal_type: &SignalType) -> Option<Signal> {
		self.0.remove(&(correlation_id.clone(), manifestation_id.clone(), signal_type.clone()))
	}

	/// Remove a signal and calculate error correction for the correlation
	///
	/// This method implements error correction based on the documentation:
	/// "The equation to do this is: (Signal - 1) + Current Error Rate = New Error Rate"
	///
	/// # Parameters
	/// - `correlation_id`: The correlation ID to remove signal for
	/// - `manifestation_id`: The manifestation ID to remove signal for  
	/// - `signal_type`: The signal type to remove
	/// - `correlation`: Mutable reference to the correlation to update with new error rate
	/// - `resolution_time`: The time when the prediction resolves (usually current time)
	/// - `default_correction_rate`: The default correction rate (usually 1)
	///
	/// # Returns
	/// - The removed signal if it existed, None otherwise
	///
	/// # Error Correction Formula
	/// New Error Rate = (Signal Probability - default_correction_rate) + Current Error Rate
	///
	/// Where Signal Probability is calculated at the resolution time.
	pub fn remove_with_error_correction(&mut self, correlation_id: &CorrelationID, manifestation_id: &ManifestationId, signal_type: &SignalType, correlation: &mut Correlation, resolution_time: DateTime<Utc>) -> Result<Option<Signal>> {
		let current_error_rate = match correlation.error_rate.get(signal_type) {
			Some(rate) => rate.clone(),
			None => bail!("No error rate found for signal type in correlation"),
		};

		// Get the signal before removing it
		if let Some(signal) = self.get(correlation_id, manifestation_id, signal_type) {
			// Calculate the signal probability at the resolution time
			let average_signal_probability = signal.probability(resolution_time, current_error_rate.0.clone())?;
			let sum_signal_probability = signal.probability(resolution_time, current_error_rate.1.clone())?;

			// Apply error correction formula: (Signal - 1) + Current Error Rate = New Error Rate
			let new_average_error_rate = (&average_signal_probability - 1) + &current_error_rate.0;
			let new_sum_error_rate = (&sum_signal_probability + 1) + &current_error_rate.1;

			// Update the correlation's error rate
			correlation.error_rate.insert(signal_type.clone(), (new_average_error_rate, new_sum_error_rate));

			// Remove and return the signal
			Ok(self.remove(correlation_id, manifestation_id, signal_type))
		} else {
			Ok(None)
		}
	}

	pub fn probability_average(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, error_rate: &(AvgErrorRate, SumErrorRate)) -> Result<Option<BigDecimal>> {
		let signals = self.get_by_event(event_id, signal_type);
		if !signals.is_empty() {
			let prob = signals.iter().map(|s| s.probability(date, error_rate.0.clone())).collect::<Result<Vec<_>>>()?;
			if prob.is_empty() {
				Ok(None)
			} else {
				let sum: BigDecimal = prob.iter().cloned().fold(BigDecimal::zero(), |acc, x| acc + x);
				let avg = sum / BigDecimal::from_usize(prob.len()).unwrap();
				Ok(Some(avg))
			}
		} else {
			Ok(None)
		}
	}

	pub fn probability_sum(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, error_rate: &(AvgErrorRate, SumErrorRate)) -> Result<Option<BigDecimal>> {
		let signals = self.get_by_event(event_id, signal_type);
		if !signals.is_empty() {
			let prob = signals.iter().map(|s| s.probability(date, error_rate.1.clone())).collect::<Result<Vec<_>>>()?;
			if prob.is_empty() {
				Ok(None)
			} else {
				let sum: BigDecimal = prob.iter().cloned().fold(BigDecimal::zero(), |acc, x| acc + x);
				Ok(Some(sum))
			}
		} else {
			Ok(None)
		}
	}

	pub fn get_by_event(&self, event_id: &EventID, signal_type: &SignalType) -> Vec<&Signal> {
		self.0.values().filter(|signal| &signal.event_id == event_id && &signal.signal_type == signal_type).collect()
	}

	pub fn get_by_correlation(&self, correlation_id: &CorrelationID, signal_type: &SignalType) -> Vec<&Signal> {
		self.0.values().filter(|signal| &signal.correlation_id == correlation_id && &signal.signal_type == signal_type).collect()
	}

	/// Iterator over all signals (sequential)
	pub fn iter(&self) -> impl Iterator<Item = (&(CorrelationID, ManifestationId, SignalType), &Signal)> {
		self.0.iter()
	}

	/// Parallel iterator over all signals (using Rayon)
	pub fn par_iter(&self) -> impl rayon::iter::ParallelIterator<Item = (&(CorrelationID, ManifestationId, SignalType), &Signal)> {
		use rayon::prelude::*;
		self.0.par_iter()
	}

	/// Find signals by correlation ID
	pub fn find_by_correlation(&self, correlation_id: &CorrelationID) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.correlation_id == *correlation_id).collect()
	}

	/// Find signals by manifestation ID
	pub fn find_by_manifestation(&self, manifestation_id: &ManifestationId) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.manifestation_id == *manifestation_id).collect()
	}

	/// Find signals by signal type
	pub fn find_by_signal_type(&self, signal_type: &SignalType) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.signal_type == *signal_type).collect()
	}

	/// Find any signal that exists (useful for testing)
	pub fn find_any(&self) -> Option<&Signal> {
		self.0.values().next()
	}

	/// Find a random signal (useful for testing)
	pub fn find_random(&self) -> Option<&Signal> {
		use rand::{seq::IteratorRandom, thread_rng};
		self.0.values().choose(&mut thread_rng())
	}

	/// Get all unique correlation IDs
	pub fn get_correlation_ids(&self) -> Vec<CorrelationID> {
		use std::collections::HashSet;
		let mut ids: HashSet<CorrelationID> = HashSet::new();
		for signal in self.0.values() {
			ids.insert(signal.correlation_id.clone());
		}
		ids.into_iter().collect()
	}

	/// Get all unique manifestation IDs
	pub fn get_manifestation_ids(&self) -> Vec<ManifestationId> {
		use std::collections::HashSet;
		let mut ids: HashSet<ManifestationId> = HashSet::new();
		for signal in self.0.values() {
			ids.insert(signal.manifestation_id.clone());
		}
		ids.into_iter().collect()
	}

	/// Get all unique signal types
	pub fn get_signal_types(&self) -> Vec<SignalType> {
		use std::collections::HashSet;
		let mut types: HashSet<SignalType> = HashSet::new();
		for signal in self.0.values() {
			types.insert(signal.signal_type.clone());
		}
		types.into_iter().collect()
	}

	/// Find signals by event ID (requires correlations lookup)
	/// Note: This method requires external correlation data to map signals to events
	pub fn find_by_event_via_correlations(&self, event_id: &crate::types::event::EventID, correlations: &crate::types::correlation::Correlations) -> Vec<&Signal> {
		// Find correlations for this event
		let event_correlations = correlations.get_for_event(event_id);
		let correlation_ids: std::collections::HashSet<_> = event_correlations.iter().map(|(_, corr)| &corr.id).collect();

		// Find signals that belong to these correlations
		self.0.values().filter(|signal| correlation_ids.contains(&signal.correlation_id)).collect()
	}
}
