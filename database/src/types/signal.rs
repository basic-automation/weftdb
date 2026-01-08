use std::{collections::HashMap, pin::Pin, str::FromStr};

use anyhow::{bail, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::{Deserialize, Serialize};
use splimes::Resolution;

use crate::{
	types::{
		correlation::{Correlation, CorrelationID}, database::traits::Outputs, event::{Event, EventID, ManifestationId}
	}, AspectId
};

/// Finds the midpoint of the manifestation that occurred immediately before the given
/// `manifestation_id`.
///
/// This helper is used when computing probabilities over a sequence of manifestations for
/// a signal. It uses a pre-built index for O(1) lookup instead of linear search.
///
/// # Parameters
/// - `manifestation_id`: The identifier of the manifestation whose *previous* midpoint
///   should be retrieved.
/// - `midpoint_index`: A `HashMap` mapping each `ManifestationId` to its index in the sorted sequence.
/// - `sorted_midpoints`: A slice of (`ManifestationId`, `DateTime<Utc>`) tuples. This slice
///   must be sorted in the same order in which manifestations are considered for probability
///   calculations (typically chronological order by midpoint).
///
/// # Returns
/// - `Some(DateTime<Utc>)` with the midpoint of the manifestation that appears immediately
///   before `manifestation_id` in `sorted_midpoints`.
/// - `None` if `manifestation_id` is not present in the index or if it is the first
///   entry in the slice (i.e., there is no previous manifestation).
fn find_previous_manifestation_midpoint(
	manifestation_id: &ManifestationId,
	midpoint_index: &HashMap<ManifestationId, usize>,
	sorted_midpoints: &[(ManifestationId, DateTime<Utc>)]
) -> Option<DateTime<Utc>> {
	// O(1) lookup using the pre-built index
	let idx = *midpoint_index.get(manifestation_id)?;
	
	// If it's the first manifestation, there's no previous one
	if idx == 0 {
		return None;
	}
	
	// Return the previous manifestation's midpoint
	Some(sorted_midpoints[idx - 1].1)
}

/// Builds an index mapping `ManifestationId` to its position in the sorted midpoints slice.
/// This enables O(1) lookups instead of O(n) linear searches.
fn build_midpoint_index(sorted_midpoints: &[(ManifestationId, DateTime<Utc>)]) -> HashMap<ManifestationId, usize> {
	sorted_midpoints
		.iter()
		.enumerate()
		.map(|(idx, (id, _))| (id.clone(), idx))
		.collect()
}

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

impl FromStr for SignalType {
	type Err = anyhow::Error;

	fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
		s.strip_prefix("Custom(")
			.and_then(|s| s.strip_suffix(')'))
			.map_or_else(
				|| Err(anyhow::anyhow!("Invalid SignalType: {s}")),
				|name| Ok(Self::Custom(name.to_string()))
			)
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
		use futures::StreamExt;
		let mut correlation_stream: Pin<Box<dyn Stream<Item = Result<Correlation>> + Send>> = database.get_correlations(aspect_id).await?;

		// Search through the stream for the matching correlation
		while let Some(result) = correlation_stream.next().await {
			if let Ok(correlation) = result {
				if correlation.id() == &self.correlation_id {
					let Some(signal_error_rate) = correlation.error_rate().get(self.signal_type()) else {
						return Err(anyhow::anyhow!("No error rate found for signal type {:?} in correlation {:?}", self.signal_type(), self.correlation_id()));
					};
					return Ok(signal_error_rate.clone());
				}
			}
		}

		Err(anyhow::anyhow!("No correlation found for correlation_id {:?}", self.correlation_id))
	}

	/// Probability of the signal at a given date using the correlation's `average_distance`.
	/// 
	/// Per documentation: probability = `time_elapsed` / `average_distance`
	/// The signal represents a prediction starting at `manifestation_date`.
	/// The curve passes through probability = 1 at time = `manifestation_date` + `average_distance`
	///
	/// # Arguments
	/// * `date` - The query date to calculate probability for
	/// * `average_distance` - The average distance from the correlation (the divisor)
	///
	/// # Errors
	///
	/// Returns an error if time difference calculation fails
	pub fn probability_with_average_distance(&self, date: DateTime<Utc>, average_distance: &Distance) -> Result<BigDecimal> {
		// Check for division by zero
		if average_distance.value.is_zero() {
			return Ok(BigDecimal::zero()); // Avoid division by zero
		}

		// Calculate time elapsed since the signal's manifestation date (in average_distance units)
		let Some(time_elapsed) = BigDecimal::from_i64(average_distance.units.difference(&date, &self.manifestation_date)?) else { bail!("Failed to convert time difference to BigDecimal") };

		// If time_elapsed <= 0, we're before or at the manifestation date
		if time_elapsed <= BigDecimal::zero() {
			return Ok(BigDecimal::zero());
		}

		// Probability = time_elapsed / average_distance
		// This creates a linear curve from 0 at manifestation_date to 1 at average_distance
		let probability = &time_elapsed / average_distance.value();

		// Debug output disabled - uncomment for troubleshooting
		// if count < 5 {
		// 	println!("DEBUG probability_with_avg {}: manifestation_date={}, query_date={}, average_distance={} {:?}, time_elapsed={}, probability={}", 
		// 		count, self.manifestation_date, date, average_distance.value(), average_distance.units(), time_elapsed, probability);
		// }

		Ok(probability)
	}

	/// probability of the signal at a given date (legacy method using individual distance + `error_rate`)
	/// The signal represents a prediction curve starting at `manifestation_date`.
	/// The curve passes through probability = 1 at time = `manifestation_date` + distance.value + `error_rate`
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Time difference calculation fails
	/// - Distance unit conversion fails
	#[deprecated(note = "Use probability_with_average_distance instead, which uses the correlation's average_distance as per documentation")]
	pub fn probability(&self, date: DateTime<Utc>, error_rate: &Distance) -> Result<BigDecimal> {
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

		// Debug output disabled
		// if count < 5 {
		// 	println!("DEBUG probability {}: manifestation_date={}, query_date={}, distance={}, error_rate={}, time_elapsed={}, predicted_event_time={}, probability={}", count, self.manifestation_date, date, self.distance.value, error_rate_in_signal_units, time_elapsed, predicted_event_time, probability);
		// }

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
	pub fn remove_with_error_correction(&mut self, correlation: &mut Correlation, manifestation_id: &ManifestationId, signal_type: &SignalType, _resolution_time: DateTime<Utc>) -> Result<Option<Signal>> {
		let correlation_id = correlation.id().clone();

		// Get the signal before removing it
		if self.get(&correlation_id, manifestation_id, signal_type).is_some() {
			// Note: Error rate updates are now handled by calibrate_error_rates() during training.
			// We don't update error_rate here to avoid accumulation issues with the
			// formula: New Error Rate = (Signal - 1) + Current Error Rate
			// which would cause unbounded growth.
			
			// Just remove and return the signal
			Ok(self.remove(&correlation_id, manifestation_id, signal_type))
		} else {
			Ok(None)
		}
	}

	/// Calculate the sum of probabilities for all signals of a given event and signal type at a specific date
	/// Uses the correlation's `average_distance` as per documentation: probability = `time_elapsed` / `average_distance`
	/// 
	/// Per documentation: Signal Sum = sum of all probabilities - `signal_sum_error_rate`
	/// The `error_rate` is averaged across all contributing correlations and subtracted once from the sum.
	///
	/// # Errors
	///
	/// Returns an error if signal probability calculation fails for any signal.
	#[allow(deprecated)]
	pub async fn probability_sum(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, database: &crate::Database, aspect_id: &AspectId) -> Result<Option<BigDecimal>> {
		use futures::StreamExt;
		use std::collections::HashSet;
		
		let signals = self.get_by_event(event_id, signal_type);
		if signals.is_empty() {
			return Ok(None);
		}
		
		// Load correlations once to lookup average_distance and error_rate
		let correlations: Vec<Correlation> = database.get_correlations(aspect_id).await?.filter_map(|r| async { r.ok() }).collect().await;
		
		let mut prob = Vec::new();
		let mut seen_correlations: HashSet<CorrelationID> = HashSet::new();
		let mut total_error_rate = BigDecimal::zero();
		let mut error_rate_count = 0usize;
		
		for s in &signals {
			// Skip signals whose manifestation_date is in the future (not yet relevant)
			if s.manifestation_date() > &date {
				continue;
			}
			
			// Find the correlation for this signal
			if let Some(correlation) = correlations.iter().find(|c| c.id() == s.correlation_id()) {
				// Use average_distance if available, otherwise fall back to individual distance + error_rate
				if let Some(avg_dist) = correlation.average_distance() {
					let p = s.probability_with_average_distance(date, avg_dist)?;
					// Skip signals with zero probability (before they start contributing)
					if p.is_zero() {
						continue;
					}
					// Debug output disabled
					// if debug_signal_count < 3 {
					// 	println!("DEBUG prob_sum signal: manif_date={}, query_date={}, avg_dist={}, prob={}", 
					// 		s.manifestation_date().format("%Y-%m-%d %H:%M"), date.format("%Y-%m-%d %H:%M"), avg_dist.value(), p);
					// 	debug_signal_count += 1;
					// }
					prob.push(p);
					
					// Collect error rates from unique correlations only (not per-signal)
					if !seen_correlations.contains(correlation.id()) {
						seen_correlations.insert(correlation.id().clone());
						if let Some(err_rate) = correlation.get_error_rate(signal_type) {
							total_error_rate = &total_error_rate + err_rate.value();
							error_rate_count += 1;
						}
					}
				} else {
					// Fallback to old method if no average_distance set
					let default_distance = Distance::new(BigDecimal::zero(), Resolution::Seconds);
					let error_rate = correlation.get_error_rate(signal_type).cloned().unwrap_or(default_distance);
					let p = s.probability(date, &error_rate)?;
					if !p.is_zero() {
						prob.push(p);
						
						if !seen_correlations.contains(correlation.id()) {
							seen_correlations.insert(correlation.id().clone());
							if let Some(err_rate) = correlation.get_error_rate(signal_type) {
								total_error_rate = &total_error_rate + err_rate.value();
								error_rate_count += 1;
							}
						}
					}
				}
			}
		}
		
		if prob.is_empty() {
			Ok(None)
		} else {
			let raw_sum: BigDecimal = prob.iter().cloned().fold(BigDecimal::zero(), |acc, x| acc + x);
			// Apply error correction: Signal Sum = raw_sum - avg_error_rate
			// Average the error rates from contributing correlations
			let avg_error_rate = if error_rate_count > 0 {
				&total_error_rate / &BigDecimal::from(error_rate_count as u64)
			} else {
				BigDecimal::zero()
			};
			let corrected_sum = &raw_sum - &avg_error_rate;
			// Debug output disabled
			// println!("DEBUG probability_sum: Using {} signals, raw_sum={}, avg_error_rate={}, corrected_sum={}", prob.len(), raw_sum, avg_error_rate, corrected_sum);
			Ok(Some(corrected_sum))
		}
	}

	/// Calculate the average probability for all signals of a given event and signal type at a specific date
	/// Uses the correlation's `average_distance` as per documentation: probability = `time_elapsed` / `average_distance`
	/// This provides a normalized probability value by averaging individual signal probabilities
	/// 
	/// Per documentation: Signal Average = average of all probabilities - `signal_avg_error_rate`
	/// The `error_rate` is averaged across all unique contributing correlations and subtracted once.
	/// 
	/// The probability for each signal is calculated from the LAST KNOWN MANIFESTATION's midpoint,
	/// not from the signal's `manifestation_date` (which is the pattern end time). This ensures
	/// the signal-based probability matches the event-based probability calculation.
	///
	/// # Errors
	///
	/// Returns an error if signal probability calculation fails for any signal.
	#[allow(deprecated)]
	pub async fn probability_average(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, database: &crate::Database, aspect_id: &AspectId) -> Result<Option<BigDecimal>> {
		use futures::StreamExt;
		use std::collections::HashSet;
		
		let signals = self.get_by_event(event_id, signal_type);
		if signals.is_empty() {
			return Ok(None);
		}
		
		// Load correlations once to lookup average_distance and error_rate
		let correlations: Vec<Correlation> = database.get_correlations(aspect_id).await?.filter_map(|r| async { r.ok() }).collect().await;
		
		// Load the event to get manifestations for proper probability calculation
		let events: Vec<Event> = database.get_unprocessed_events(aspect_id).await?.filter_map(|r| async { r.ok() }).collect().await;
		let event = events.iter().find(|e| e.id() == event_id);
		
		// Build a sorted list of manifestation midpoints for looking up the previous manifestation
		let manifestation_midpoints: Vec<(ManifestationId, DateTime<Utc>)> = event
			.map(|e| {
				let mut midpoints: Vec<_> = e.manifestations()
					.iter()
					.map(|(id, m)| (id.clone(), m.midpoint()))
					.collect();
				midpoints.sort_by_key(|(_, midpoint)| *midpoint);
				midpoints
			})
			.unwrap_or_default();
		
		// Build index for O(1) lookups instead of O(n) linear search per signal
		let midpoint_index = build_midpoint_index(&manifestation_midpoints);
		
		// Get the last manifestation's midpoint as fallback for forward-looking signals
		let last_manifestation_midpoint = manifestation_midpoints.last().map(|(_, midpoint)| *midpoint);
		
		let mut prob = Vec::new();
		let mut seen_correlations: HashSet<CorrelationID> = HashSet::new();
		let mut total_error_rate = BigDecimal::zero();
		let mut error_rate_count = 0usize;
		
		for s in &signals {
			// Skip signals whose manifestation_date is in the future (not yet relevant)
			if s.manifestation_date() > &date {
				continue;
			}
			
			// Find the correlation for this signal
			if let Some(correlation) = correlations.iter().find(|c| c.id() == s.correlation_id()) {
				// Use average_distance if available, otherwise fall back to individual distance + error_rate
				if let Some(avg_dist) = correlation.average_distance() {
					// Find the previous manifestation's midpoint for this signal
					// The signal's manifestation_id is the manifestation it PREDICTS
					// We need to find the manifestation that occurred BEFORE it
					let previous_midpoint = find_previous_manifestation_midpoint(
						s.manifestation_id(),
						&midpoint_index,
						&manifestation_midpoints
					);
					
					// Calculate probability from the previous manifestation's midpoint
					// For historical signals: use the manifestation before the one being predicted
					// For forward-looking signals (manifestation_id not found): use the last known manifestation
					// Fallback to signal's manifestation_date only if no manifestation data exists
					let reference_time = previous_midpoint
						.or(last_manifestation_midpoint)
						.unwrap_or_else(|| *s.manifestation_date());
					
					// Calculate time elapsed from reference point
					let time_elapsed_i64 = avg_dist.units().difference(&date, &reference_time)?;
					if time_elapsed_i64 <= 0 {
						continue; // Not yet past the reference point
					}
					
					let time_elapsed = BigDecimal::from(time_elapsed_i64);
					let p = &time_elapsed / avg_dist.value();
					
					// Skip signals with zero probability
					if p.is_zero() {
						continue;
					}
					prob.push(p);
					
					// Collect error rates from unique correlations only (not per-signal)
					if !seen_correlations.contains(correlation.id()) {
						seen_correlations.insert(correlation.id().clone());
						if let Some(err_rate) = correlation.get_error_rate(signal_type) {
							total_error_rate = &total_error_rate + err_rate.value();
							error_rate_count += 1;
						}
					}
				} else {
					// Fallback to old method if no average_distance set
					let default_distance = Distance::new(BigDecimal::zero(), Resolution::Seconds);
					let error_rate = correlation.get_error_rate(signal_type).cloned().unwrap_or(default_distance);
					let p = s.probability(date, &error_rate)?;
					if !p.is_zero() {
						prob.push(p);
						
						if !seen_correlations.contains(correlation.id()) {
							seen_correlations.insert(correlation.id().clone());
							if let Some(err_rate) = correlation.get_error_rate(signal_type) {
								total_error_rate = &total_error_rate + err_rate.value();
								error_rate_count += 1;
							}
						}
					}
				}
			}
		}
		
		if prob.is_empty() {
			Ok(None)
		} else {
			let sum: BigDecimal = prob.iter().cloned().fold(BigDecimal::zero(), |acc, x| acc + x);
			let signal_count = BigDecimal::from(u64::try_from(prob.len()).unwrap_or(0));
			let raw_average = &sum / &signal_count;
			
			// Apply error correction: Signal Average = raw_average - avg_error_rate
			// The avg_error_rate is the average of error rates from unique contributing correlations
			let avg_error_rate = if error_rate_count > 0 {
				&total_error_rate / &BigDecimal::from(error_rate_count as u64)
			} else {
				BigDecimal::zero()
			};
			let corrected_average = &raw_average - &avg_error_rate;
			
			// Debug output disabled
			// println!("DEBUG probability_average: Using {} signals, raw_average={}, avg_error_rate={}, corrected_average={}", prob.len(), raw_average, avg_error_rate, corrected_average);
			Ok(Some(corrected_average))
		}
	}

	/// Calculates the event-based probability at a given time.
	/// 
	/// This function returns a single probability value based on time elapsed since the
	/// **last manifestation** of the event, rather than averaging across individual signal
	/// `manifestation_dates`.
	///
	/// Formula: probability = (`query_date` - `last_manifestation_end`) / `average_distance`
	///
	/// This is useful when you want to know "how overdue is the next event?" rather than
	/// averaging signals from different pattern occurrences.
	///
	/// Returns None if:
	/// - No signals exist for the event
	/// - No correlation can be found with an `average_distance`
	/// - The event has no manifestations
	/// - The query date is before or at the last manifestation
	///
	/// # Errors
	///
	/// Returns an error if database operations fail.
	pub async fn event_probability(&self, event_id: &EventID, signal_type: &SignalType, date: DateTime<Utc>, database: &crate::Database, aspect_id: &AspectId) -> Result<Option<BigDecimal>> {
		use futures::StreamExt;
		
		let signals = self.get_by_event(event_id, signal_type);
		if signals.is_empty() {
			return Ok(None);
		}
		
		// Load correlations to get average_distance
		let correlations: Vec<Correlation> = database.get_correlations(aspect_id).await?.filter_map(|r| async { r.ok() }).collect().await;
		
		// Find any correlation for this event with an average_distance
		let avg_dist = correlations
			.iter()
			.find(|c| c.event_id() == event_id && c.average_distance().is_some())
			.and_then(|c| c.average_distance().cloned());
		
		let Some(average_distance) = avg_dist else {
			return Ok(None);
		};
		
		if average_distance.value().is_zero() {
			return Ok(None);
		}
		
		// Get the event to find the last manifestation
		let events: Vec<Event> = database.get_unprocessed_events(aspect_id).await?.filter_map(|r| async { r.ok() }).collect().await;
		let event = events.iter().find(|e| e.id() == event_id);
		
		let Some(event) = event else {
			return Ok(None);
		};
		
		// Find the last manifestation and extract the relevant timestamp based on signal_type
		// For "PredictStart" we measure from start-to-start (when will the next event START)
		// For "PredictEnd" we measure from end-to-end (when will the next event END)
		let last_manifestation = event.manifestations()
			.values()
			.max_by_key(|m| m.start());
		
		let Some(last_manifest) = last_manifestation else {
			return Ok(None);
		};
		
		// Choose the reference point based on signal type
		// The manifestation spans from before the peak to after the peak
		// For predictions, we typically want to measure from when the event peaked (midpoint)
		// except for PredictEnd which measures from when the event finished
		let reference_time = match signal_type {
			SignalType::Custom(ref type_name) if type_name == "PredictStart" => last_manifest.midpoint(),
			SignalType::Custom(ref type_name) if type_name == "PredictMid" => last_manifest.midpoint(),
			SignalType::Custom(ref type_name) if type_name == "PredictEnd" => *last_manifest.end(),
			SignalType::Custom(_) => last_manifest.midpoint(), // Default to midpoint
		};
		
		// Calculate time elapsed since reference point (in the average_distance units)
		let time_elapsed = average_distance.units().difference(&date, &reference_time)?;
		
		// If we're before or at the reference point, probability is 0
		if time_elapsed <= 0 {
			return Ok(Some(BigDecimal::zero()));
		}
		
		let time_elapsed_bd = BigDecimal::from(time_elapsed);
		let probability = &time_elapsed_bd / average_distance.value();
		
		// Debug output disabled
		// println!("DEBUG event_probability: reference_time={} ({}), query_date={}, time_elapsed={}, avg_dist={}, probability={}", 
		// 	reference_time.format("%Y-%m-%d %H:%M"), signal_type, date.format("%Y-%m-%d %H:%M"), time_elapsed, average_distance.value(), probability);
		
		Ok(Some(probability))
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
