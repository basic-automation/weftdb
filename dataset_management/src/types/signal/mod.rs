use std::collections::HashMap;

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::{
	types::{
		correlation::{Correlation, CorrelationID, ErrorRate}, event::ManifestationId
	}, EventID, CORRELATIONS_QUEUE
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

impl std::fmt::Display for Distance {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} {:?}", self.value, self.units)
	}
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

	/// Convert a Distance from one resolution to the signal's distance units
	/// This ensures all distance values are in compatible units for calculations
	fn convert_distance_to_signal_units(&self, distance: &Distance) -> Result<BigDecimal> {
		// Convert source distance to seconds first, then to target units
		let seconds = self.distance_to_seconds(distance)?;
		let target_value = self.seconds_to_distance_units(seconds)?;
		Ok(target_value)
	}

	/// Convert a distance value to seconds based on its resolution
	fn distance_to_seconds(&self, distance: &Distance) -> Result<BigDecimal> {
		let conversion_factor = match distance.units {
			Resolution::Nanoseconds => BigDecimal::from(1_000_000_000i64),
			Resolution::Microseconds => BigDecimal::from(1_000_000i64),
			Resolution::Milliseconds => BigDecimal::from(1_000i64),
			Resolution::Seconds => BigDecimal::from(1i64),
			Resolution::Minutes => BigDecimal::from(60i64),
			Resolution::Hours => BigDecimal::from(3600i64),
			Resolution::Days => BigDecimal::from(86400i64),
			Resolution::Weeks => BigDecimal::from(604800i64),
			Resolution::Months => BigDecimal::from(2592000i64), // 30 days
			Resolution::Years => BigDecimal::from(31536000i64), // 365 days
		};

		// For sub-second units, divide by factor; for super-second units, multiply by factor
		let seconds = match distance.units {
			Resolution::Nanoseconds | Resolution::Microseconds | Resolution::Milliseconds => &distance.value / &conversion_factor,
			_ => &distance.value * &conversion_factor,
		};

		Ok(seconds)
	}

	/// Convert seconds to the signal's distance units
	fn seconds_to_distance_units(&self, seconds: BigDecimal) -> Result<BigDecimal> {
		let conversion_factor = match self.distance.units {
			Resolution::Nanoseconds => BigDecimal::from(1_000_000_000i64),
			Resolution::Microseconds => BigDecimal::from(1_000_000i64),
			Resolution::Milliseconds => BigDecimal::from(1_000i64),
			Resolution::Seconds => BigDecimal::from(1i64),
			Resolution::Minutes => BigDecimal::from(60i64),
			Resolution::Hours => BigDecimal::from(3600i64),
			Resolution::Days => BigDecimal::from(86400i64),
			Resolution::Weeks => BigDecimal::from(604800i64),
			Resolution::Months => BigDecimal::from(2592000i64), // 30 days
			Resolution::Years => BigDecimal::from(31536000i64), // 365 days
		};

		// For sub-second units, multiply by factor; for super-second units, divide by factor
		let target_value = match self.distance.units {
			Resolution::Nanoseconds | Resolution::Microseconds | Resolution::Milliseconds => &seconds * &conversion_factor,
			_ => &seconds / &conversion_factor,
		};

		Ok(target_value)
	}

	pub async fn get_error_rate(&self) -> Result<ErrorRate> {
		let error_rate = CORRELATIONS_QUEUE.lock().await.get_by_id(&self.correlation_id.clone()).ok_or_else(|| anyhow::anyhow!("No correlation found for signal"))?.get_error_rate(&self.signal_type).ok_or_else(|| anyhow::anyhow!("No error rate found for signal type in correlation"))?.clone();
		Ok(error_rate)
	}

	/// probability of the signal at a given date
	/// The signal represents a prediction curve starting at manifestation_date.
	/// The curve passes through probability = 1 at time = manifestation_date + distance.value + error_rate
	pub fn probability(&self, date: DateTime<Utc>, error_rate: &Distance) -> Result<BigDecimal> {
		// Check for division by zero
		if self.distance.value.is_zero() && error_rate.value.is_zero() {
			return Ok(BigDecimal::zero()); // Avoid division by zero
		}

		// Calculate time elapsed since the signal's manifestation date
		let time_elapsed = match BigDecimal::from_i64(self.distance.units.difference(&date, &self.manifestation_date)?) {
			Some(diff) => diff,
			None => bail!("Failed to convert time difference to BigDecimal"),
		};

		// Convert error rate to the signal's distance units if they differ
		let error_rate_in_signal_units = if error_rate.units == self.distance.units {
			// Same units, use error rate value directly
			error_rate.value.clone()
		} else {
			// Different units, convert error rate to signal's units using Resolution methods
			self.convert_distance_to_signal_units(error_rate)?
		};

		// The predicted event time is: manifestation_date + distance.value + error_rate (in signal units)
		let predicted_event_time = &self.distance.value + &error_rate_in_signal_units;

		// If we haven't reached the predicted time yet, probability should be proportional to time elapsed
		// If time_elapsed <= 0, we're before or at the manifestation date
		if time_elapsed <= BigDecimal::zero() {
			return Ok(BigDecimal::zero());
		}

		// If predicted_event_time <= 0, handle edge case
		if predicted_event_time <= BigDecimal::zero() {
			return Ok(BigDecimal::from(1));
		}

		// Probability = time_elapsed / predicted_event_time
		// This creates a linear curve from 0 at manifestation_date to 1 at predicted_event_time
		let probability = &time_elapsed / predicted_event_time;

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
	/// - `signal_error_rates`: The current error rates for this signal type (to avoid deadlock)
	///
	/// # Returns
	/// - The removed signal if it existed, None otherwise
	///
	/// # Error Correction Formula
	/// New Error Rate = (Signal Probability - default_correction_rate) + Current Error Rate
	///
	/// Where Signal Probability is calculated at the resolution time.
	pub async fn remove_with_error_correction(&mut self, correlation: &mut Correlation, manifestation_id: &ManifestationId, signal_type: &SignalType, resolution_time: DateTime<Utc>) -> Result<Option<Signal>> {
		let correlation_id = &correlation.id;
		let Some(signal_error_rates) = correlation.get_error_rate(signal_type) else {
			// No error rates for this signal type, cannot perform error correction
			bail!("No error rates found for signal type in correlation");
		};

		// Get the signal before removing it
		if let Some(signal) = self.get(correlation_id, manifestation_id, signal_type) {
			// Calculate the signal probability at the resolution time using provided error rate
			let signal_probability = signal.probability(resolution_time, signal_error_rates)?;

			// Apply error correction formula: (Signal - 1) + Current Error Rate = New Error Rate
			let new_error_rate = Distance { value: (&signal_probability - 1) + &signal_error_rates.value, units: signal_error_rates.units };

			// Update the correlation's error rate
			correlation.error_rate.insert(signal_type.clone(), new_error_rate);

			// Remove and return the signal
			Ok(self.remove(correlation_id, manifestation_id, signal_type))
		} else {
			Ok(None)
		}
	}

	pub async fn probability_sum(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>) -> Result<Option<BigDecimal>> {
		let signals = self.get_by_event(event_id, signal_type);
		if !signals.is_empty() {
			let mut prob = Vec::new();
			let mut total_error_rate = BigDecimal::zero();
			let mut error_rate_count = 0;

			for s in signals.iter() {
				let default_distance = Distance { value: BigDecimal::zero(), units: Resolution::Seconds };
				let error_rate = s.get_error_rate().await.unwrap_or(default_distance.clone());
				total_error_rate = &total_error_rate + &error_rate.value;
				error_rate_count += 1;
				prob.push(s.probability(date, &error_rate)?);
			}
			if prob.is_empty() {
				Ok(None)
			} else {
				let avg_error_rate = &total_error_rate / BigDecimal::from(error_rate_count);
				println!("DEBUG probability_sum: Using {} signals with average error rate: {}", signals.len(), avg_error_rate);
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
