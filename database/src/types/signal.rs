use std::collections::HashMap;

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::{
	types::{
		correlation::{Correlation, CorrelationID}, event::{EventID, ManifestationId}
	}, AspectId
};

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum SignalType {
	Custom(String),
}

impl std::fmt::Display for SignalType {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Custom(name) => write!(f, "Custom({name})"),
		}
	}
}

// Custom serialization for SignalType to ensure it works as HashMap keys
impl serde::Serialize for SignalType {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(&self.to_string())
	}
}

// Custom deserialization for SignalType
impl<'de> serde::Deserialize<'de> for SignalType {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = String::deserialize(deserializer)?;
		s.strip_prefix("Custom(").and_then(|s| s.strip_suffix(')')).map_or_else(|| Err(serde::de::Error::custom(format!("Invalid SignalType: {s}"))), |name| Ok(Self::Custom(name.to_string())))
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distance {
	value: BigDecimal,
	units: Resolution,
}

impl Distance {
	/// Create a new Distance
	#[must_use]
	pub const fn new(value: BigDecimal, units: Resolution) -> Self {
		Self { value, units }
	}

	/// Get the distance value
	#[must_use]
	pub const fn value(&self) -> &BigDecimal {
		&self.value
	}

	/// Set the distance value
	pub fn set_value(&mut self, value: BigDecimal) {
		self.value = value;
	}

	/// Get the distance units
	#[must_use]
	pub const fn units(&self) -> &Resolution {
		&self.units
	}

	/// Set the distance units
	pub const fn set_units(&mut self, units: Resolution) {
		self.units = units;
	}
}

impl std::fmt::Display for Distance {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} {:?}", self.value, self.units)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
	correlation_id: CorrelationID,
	manifestation_id: ManifestationId,
	event_id: EventID,
	manifestation_date: DateTime<Utc>,
	type_: SignalType,
	distance: Distance,
}

impl Signal {
	#[must_use]
	pub const fn new(correlation_id: CorrelationID, manifestation_id: ManifestationId, event_id: EventID, manifestation_date: DateTime<Utc>, signal_type: SignalType, distance: Distance) -> Self {
		Self { correlation_id, manifestation_id, event_id, manifestation_date, type_: signal_type, distance }
	}

	/// Get the correlation ID
	#[must_use]
	pub const fn correlation_id(&self) -> &CorrelationID {
		&self.correlation_id
	}

	/// Set the correlation ID
	pub const fn set_correlation_id(&mut self, correlation_id: CorrelationID) {
		self.correlation_id = correlation_id;
	}

	/// Get the manifestation ID
	#[must_use]
	pub const fn manifestation_id(&self) -> &ManifestationId {
		&self.manifestation_id
	}

	/// Set the manifestation ID
	pub const fn set_manifestation_id(&mut self, manifestation_id: ManifestationId) {
		self.manifestation_id = manifestation_id;
	}

	/// Get the event ID
	#[must_use]
	pub const fn event_id(&self) -> &EventID {
		&self.event_id
	}

	/// Set the event ID
	pub const fn set_event_id(&mut self, event_id: EventID) {
		self.event_id = event_id;
	}

	/// Get the manifestation date
	#[must_use]
	pub const fn manifestation_date(&self) -> &DateTime<Utc> {
		&self.manifestation_date
	}

	/// Set the manifestation date
	pub const fn set_manifestation_date(&mut self, manifestation_date: DateTime<Utc>) {
		self.manifestation_date = manifestation_date;
	}

	/// Get the signal type
	#[must_use]
	pub const fn signal_type(&self) -> &SignalType {
		&self.type_
	}

	/// Set the signal type
	pub fn set_signal_type(&mut self, signal_type: SignalType) {
		self.type_ = signal_type;
	}

	#[must_use]
	pub const fn distance(&self) -> &Distance {
		&self.distance
	}

	/// Set the distance
	pub fn set_distance(&mut self, distance: Distance) {
		self.distance = distance;
	}

	/// Convert a Distance from one resolution to the signal's distance units
	/// This ensures all distance values are in compatible units for calculations
	///
	/// # Errors
	///
	/// Returns an error if the distance conversion fails due to unsupported resolution units.
	fn convert_distance_to_signal_units(&self, distance: &Distance) -> BigDecimal {
		// Convert source distance to seconds first, then to target units
		let seconds = Self::distance_to_seconds(distance);
		self.seconds_to_distance_units(&seconds)
	}

	/// Convert a distance value to seconds based on its resolution
	fn distance_to_seconds(distance: &Distance) -> BigDecimal {
		let conversion_factor = match distance.units {
			Resolution::Nanoseconds => BigDecimal::from(1_000_000_000_i64),
			Resolution::Microseconds => BigDecimal::from(1_000_000_i64),
			Resolution::Milliseconds => BigDecimal::from(1_000_i64),
			Resolution::Seconds => BigDecimal::from(1_i64),
			Resolution::Minutes => BigDecimal::from(60_i64),
			Resolution::Hours => BigDecimal::from(3600_i64),
			Resolution::Days => BigDecimal::from(86400_i64),
			Resolution::Weeks => BigDecimal::from(604_800_i64),
			Resolution::Months => BigDecimal::from(2_592_000_i64), // 30 days
			Resolution::Years => BigDecimal::from(31_536_000_i64), // 365 days
		};

		// For sub-second units, divide by factor; for super-second units, multiply by factor
		match distance.units {
			Resolution::Nanoseconds | Resolution::Microseconds | Resolution::Milliseconds => &distance.value / &conversion_factor,
			_ => &distance.value * &conversion_factor,
		}
	}

	/// Convert seconds to the signal's distance units
	fn seconds_to_distance_units(&self, seconds: &BigDecimal) -> BigDecimal {
		let conversion_factor = match self.distance.units {
			Resolution::Nanoseconds => BigDecimal::from(1_000_000_000_i64),
			Resolution::Microseconds => BigDecimal::from(1_000_000_i64),
			Resolution::Milliseconds => BigDecimal::from(1_000_i64),
			Resolution::Seconds => BigDecimal::from(1_i64),
			Resolution::Minutes => BigDecimal::from(60_i64),
			Resolution::Hours => BigDecimal::from(3600_i64),
			Resolution::Days => BigDecimal::from(86400_i64),
			Resolution::Weeks => BigDecimal::from(604_800_i64),
			Resolution::Months => BigDecimal::from(2_592_000_i64), // 30 days
			Resolution::Years => BigDecimal::from(31_536_000_i64), // 365 days
		};

		// For sub-second units, multiply by factor; for super-second units, divide by factor
		match self.distance.units {
			Resolution::Nanoseconds | Resolution::Microseconds | Resolution::Milliseconds => seconds * &conversion_factor,
			_ => seconds / &conversion_factor,
		}
	}

	/// Get the error rate for this signal from its correlation
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - No correlation is found for the signal's correlation ID
	/// - No error rate is found for the signal's type in the correlation
	pub async fn get_error_rate(&self, database: &crate::Database, aspect_id: &AspectId) -> Result<Distance> {
		let correlations = database.get_correlations(aspect_id).await?;

		let correlation = correlations.iter().find(|c| c.id() == &self.correlation_id).ok_or_else(|| anyhow::anyhow!("No correlation found for correlation_id {:?}", self.correlation_id))?;

		let Some(signal_error_rate) = correlation.error_rate().get(self.signal_type()) else {
			return Err(anyhow::anyhow!("No error rate found for signal type {:?} in correlation {:?}", self.signal_type(), self.correlation_id()));
		};

		Ok(signal_error_rate.clone())
	}

	/// probability of the signal at a given date
	/// The signal represents a prediction curve starting at `manifestation_date`.
	/// The curve passes through probability = 1 at time = `manifestation_date` + distance.value + `error_rate`
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Time difference calculation fails
	/// - Distance unit conversion fails
	pub fn probability(&self, date: DateTime<Utc>, error_rate: &Distance) -> Result<BigDecimal> {
		// Debug: Track probability calculations (first 5)
		static PROBABILITY_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
		let count = PROBABILITY_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

		// Check for division by zero
		if self.distance.value.is_zero() && error_rate.value.is_zero() {
			return Ok(BigDecimal::zero()); // Avoid division by zero
		}

		// Calculate time elapsed since the signal's manifestation date
		let Some(time_elapsed) = BigDecimal::from_i64(self.distance.units.difference(&date, &self.manifestation_date)?) else { bail!("Failed to convert time difference to BigDecimal") };

		// Convert error rate to the signal's distance units if they differ
		let error_rate_in_signal_units = if error_rate.units == self.distance.units {
			// Same units, use error rate value directly
			error_rate.value.clone()
		} else {
			// Different units, convert error rate to signal's units using Resolution methods
			self.convert_distance_to_signal_units(error_rate)
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
		let probability = &time_elapsed / &predicted_event_time;

		if count < 5 {
			println!("DEBUG probability {}: manifestation_date={}, query_date={}, distance={}, error_rate={}, time_elapsed={}, predicted_event_time={}, probability={}", count, self.manifestation_date, date, self.distance.value, error_rate_in_signal_units, time_elapsed, predicted_event_time, probability);
		}

		Ok(probability)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrVal {
	value: BigDecimal,
	error: BigDecimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signals(HashMap<(CorrelationID, ManifestationId, SignalType), Signal>); // Keyed by (CorrelationID, ManifestationId, SignalType) tuple

impl Default for Signals {
	fn default() -> Self {
		Self::new()
	}
}

impl Signals {
	#[must_use]
	pub fn new() -> Self {
		Self(HashMap::new())
	}

	pub fn insert(&mut self, signal: Signal) {
		let key = (signal.correlation_id().clone(), signal.manifestation_id().clone(), signal.signal_type().clone());
		self.0.insert(key, signal);
	}

	#[must_use]
	pub fn get(&self, correlation_id: &CorrelationID, manifestation_id: &ManifestationId, signal_type: &SignalType) -> Option<&Signal> {
		self.0.get(&(correlation_id.clone(), manifestation_id.clone(), signal_type.clone()))
	}

	#[must_use]
	pub fn len(&self) -> usize {
		self.0.len()
	}

	#[must_use]
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
	/// This method implements error correction using exponential moving average (EMA).
	/// The error rate tracks the typical offset between when patterns occur and when events manifest.
	///
	/// # Parameters
	/// - `correlation_id`: The correlation ID to remove signal for
	/// - `manifestation_id`: The manifestation ID to remove signal for  
	/// - `signal_type`: The signal type to remove
	/// - `correlation`: Mutable reference to the correlation to update with new error rate
	/// - `resolution_time`: The time when the event actually occurred
	///
	/// # Returns
	/// - The removed signal if it existed, None otherwise
	///
	/// # Error Correction Formula
	/// The signal predicts an event at: `manifestation_date + distance + old_error_rate`
	/// The event actually occurred at: `resolution_time`
	///
	/// Actual time from manifestation = `resolution_time` - `manifestation_date`
	/// Predicted time from manifestation = distance + `old_error_rate`
	/// Observed error = `actual_time` - `predicted_time` (can be positive or negative)
	///
	/// New Error Rate = alpha * `observed_error` + (1 - alpha) * `old_error_rate`
	/// Where alpha = 0.2 (gives 20% weight to new observation, 80% to historical average)
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - No error rates are found for the signal type in the correlation
	/// - Time difference calculation fails
	///
	/// # Panics
	///
	/// Panics if the alpha value (0.2) cannot be converted to `BigDecimal`, which should
	/// not happen under normal circumstances.
	pub fn remove_with_error_correction(&mut self, correlation: &mut Correlation, manifestation_id: &ManifestationId, signal_type: &SignalType, resolution_time: DateTime<Utc>) -> Result<Option<Signal>> {
		let correlation_id = correlation.id().clone();
		let Some(signal_error_rates) = correlation.get_error_rate(signal_type) else {
			// No error rates for this signal type, cannot perform error correction
			bail!("No error rates found for signal type in correlation");
		};

		// Get the signal before removing it
		if let Some(signal) = self.get(&correlation_id, manifestation_id, signal_type) {
			// Calculate how much time actually elapsed from manifestation_date to resolution_time
			let Some(actual_time_elapsed) = BigDecimal::from_i64(signal.distance.units.difference(&resolution_time, &signal.manifestation_date)?) else {
				bail!("Failed to convert time difference to BigDecimal");
			};

			// Convert the old error rate to the signal's distance units if needed
			let old_error_in_signal_units = if signal_error_rates.units == signal.distance.units { signal_error_rates.value.clone() } else { signal.convert_distance_to_signal_units(signal_error_rates) };

			// The signal predicted the event would occur at: distance + old_error_rate
			let predicted_time = &signal.distance.value + &old_error_in_signal_units;

			// The observed error is how much we were off (positive = we predicted too early, negative = too late)
			let observed_error = &actual_time_elapsed - &predicted_time;

			// Debug: Track error corrections (first 10)
			let count = {
				static ERROR_CORRECTION_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
				ERROR_CORRECTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
			};
			if count < 10 {
				println!("DEBUG Error correction {}: manifestation_date={}, resolution_time={}, signal.distance={}, old_error={}, actual_time={}, predicted_time={}, observed_error={}", count, signal.manifestation_date, resolution_time, signal.distance.value, old_error_in_signal_units, actual_time_elapsed, predicted_time, observed_error);
			}

			// Apply exponential moving average (EMA) to update error rate
			// alpha = 0.2 means 20% weight to new observation, 80% to historical average
			// This balances between adapting to recent data and maintaining stability
			let alpha = BigDecimal::from_f64(0.2).unwrap();
			let one_minus_alpha = BigDecimal::from_f64(0.8).unwrap();

			let new_error_value = (&alpha * &observed_error) + (&one_minus_alpha * &old_error_in_signal_units);

			if count < 10 {
				println!("  -> new_error_rate={new_error_value}");
			}

			let new_error_rate = Distance::new(new_error_value, *signal.distance().units());

			// Update the correlation's error rate
			correlation.set_error_rate(signal_type.clone(), new_error_rate);

			// Remove and return the signal
			Ok(self.remove(&correlation_id, manifestation_id, signal_type))
		} else {
			Ok(None)
		}
	}

	/// Calculate the sum of probabilities for all signals of a given event and signal type at a specific date
	///
	/// # Errors
	///
	/// Returns an error if signal probability calculation fails for any signal.
	pub async fn probability_sum(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, database: &crate::Database, aspect_id: &AspectId) -> Result<Option<BigDecimal>> {
		let signals = self.get_by_event(event_id, signal_type);
		if signals.is_empty() {
			Ok(None)
		} else {
			let mut prob = Vec::new();
			let mut total_error_rate = BigDecimal::zero();
			let mut error_rate_count = 0;

			for s in &signals {
				let default_distance = Distance::new(BigDecimal::zero(), Resolution::Seconds);
				let error_rate = s.get_error_rate(database, aspect_id).await.unwrap_or_else(|_| default_distance.clone());
				total_error_rate = &total_error_rate + error_rate.value();
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
		}
	}

	/// Calculate the average probability for all signals of a given event and signal type at a specific date
	/// This provides a normalized probability value between 0 and 1 by averaging individual signal probabilities
	///
	/// # Errors
	///
	/// Returns an error if signal probability calculation fails for any signal.
	pub async fn probability_average(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, database: &crate::Database, aspect_id: &AspectId) -> Result<Option<BigDecimal>> {
		let signals = self.get_by_event(event_id, signal_type);
		if signals.is_empty() {
			Ok(None)
		} else {
			let mut prob = Vec::new();
			let mut total_error_rate = BigDecimal::zero();
			let mut error_rate_count = 0;

			for s in &signals {
				let default_distance = Distance::new(BigDecimal::zero(), Resolution::Seconds);
				let error_rate = s.get_error_rate(database, aspect_id).await.unwrap_or_else(|_| default_distance.clone());
				total_error_rate = &total_error_rate + error_rate.value();
				error_rate_count += 1;
				prob.push(s.probability(date, &error_rate)?);
			}
			if prob.is_empty() {
				Ok(None)
			} else {
				let avg_error_rate = &total_error_rate / BigDecimal::from(error_rate_count);
				let sum: BigDecimal = prob.iter().cloned().fold(BigDecimal::zero(), |acc, x| acc + x);
				let signal_count = BigDecimal::from(u64::try_from(signals.len()).unwrap_or(0));
				let average = &sum / &signal_count;
				println!("DEBUG probability_average: Using {} signals with average error rate: {} minutes, average probability: {}", signals.len(), avg_error_rate, average);
				Ok(Some(average))
			}
		}
	}

	#[must_use]
	pub fn get_by_event(&self, event_id: &EventID, signal_type: &SignalType) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.event_id() == event_id && signal.signal_type() == signal_type).collect()
	}

	#[must_use]
	pub fn get_by_correlation(&self, correlation_id: &CorrelationID, signal_type: &SignalType) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.correlation_id() == correlation_id && signal.signal_type() == signal_type).collect()
	}

	/// Iterator over all signals (sequential)
	pub fn iter(&self) -> impl Iterator<Item = (&(CorrelationID, ManifestationId, SignalType), &Signal)> {
		self.0.iter()
	}

	/// Parallel iterator over all signals (using Rayon)
	#[must_use]
	pub fn par_iter(&self) -> impl rayon::iter::ParallelIterator<Item = (&(CorrelationID, ManifestationId, SignalType), &Signal)> {
		use rayon::prelude::*;
		self.0.par_iter()
	}

	/// Find signals by correlation ID
	#[must_use]
	pub fn find_by_correlation(&self, correlation_id: &CorrelationID) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.correlation_id == *correlation_id).collect()
	}

	/// Find signals by manifestation ID
	#[must_use]
	pub fn find_by_manifestation(&self, manifestation_id: &ManifestationId) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.manifestation_id == *manifestation_id).collect()
	}

	/// Find signals by signal type
	#[must_use]
	pub fn find_by_signal_type(&self, signal_type: &SignalType) -> Vec<&Signal> {
		self.0.values().filter(|signal| signal.signal_type() == signal_type).collect()
	}

	/// Find any signal that exists (useful for testing)
	#[must_use]
	pub fn find_any(&self) -> Option<&Signal> {
		self.0.values().next()
	}

	/// Find a random signal (useful for testing)
	#[must_use]
	pub fn find_random(&self) -> Option<&Signal> {
		use rand::{seq::IteratorRandom, thread_rng};
		self.0.values().choose(&mut thread_rng())
	}

	/// Get all unique correlation IDs
	#[must_use]
	pub fn get_correlation_ids(&self) -> Vec<CorrelationID> {
		use std::collections::HashSet;
		let mut ids: HashSet<CorrelationID> = HashSet::new();
		for signal in self.0.values() {
			ids.insert(signal.correlation_id.clone());
		}
		ids.into_iter().collect()
	}

	/// Get all unique manifestation IDs
	#[must_use]
	pub fn get_manifestation_ids(&self) -> Vec<ManifestationId> {
		use std::collections::HashSet;
		let mut ids: HashSet<ManifestationId> = HashSet::new();
		for signal in self.0.values() {
			ids.insert(signal.manifestation_id.clone());
		}
		ids.into_iter().collect()
	}

	/// Get all unique signal types
	#[must_use]
	pub fn get_signal_types(&self) -> Vec<SignalType> {
		use std::collections::HashSet;
		let mut types: HashSet<SignalType> = HashSet::new();
		for signal in self.0.values() {
			types.insert(signal.signal_type().clone());
		}
		types.into_iter().collect()
	}

	/// Find signals by event ID (requires correlations lookup)
	/// Note: This method requires external correlation data to map signals to events
	#[must_use]
	pub fn find_by_event_via_correlations(&self, event_id: &EventID, correlations: &crate::types::correlation::Correlations) -> Vec<&Signal> {
		// Find correlations for this event
		let event_correlations = correlations.get_for_event(event_id);
		let correlation_ids: std::collections::HashSet<_> = event_correlations.iter().map(|(_, corr)| corr.id().clone()).collect();

		// Find signals that belong to these correlations
		self.0.values().filter(|signal| correlation_ids.contains(&signal.correlation_id)).collect()
	}
}
