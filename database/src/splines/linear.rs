use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

/// Performs linear interpolation on measurement data.
///
/// Takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`,
/// and returns a vector of interpolated or extrapolated measurements using linear interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - Measurements are empty or have fewer than 2 points
/// - Measurements have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Timestamp conversion or `BigDecimal` operations fail
pub fn linear(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Measurement>> {
	if measurements.is_empty() {
		return Ok(vec![]);
	}

	let dataset_id = measurements[0].dataset_id;
	if measurements.iter().any(|m| m.dataset_id != dataset_id) {
		return Err(Error::InconsistentDatasetIdsError.into());
	}
	if start >= end {
		return Err(Error::InvalidTimeRangeError.into());
	}
	if measurements.len() < 2 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	// Fast path for very small datasets
	if measurements.len() == 2 {
		return linear_two_point_fast(&measurements, start, end, resolution, dataset_id);
	}

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Check for uniform spacing - enables fast path
	if is_uniformly_spaced(&sorted_measurements) {
		return linear_uniform_fast(&sorted_measurements, start, end, resolution, dataset_id);
	}

	let step = resolution.to_step();

	// Build optimized linear spline
	let spline = LinearSpline::new(&sorted_measurements)?;

	let mut result = Vec::new();

	// Pre-compute rounding to avoid repeated calculations
	let start_millis = start.timestamp_millis();
	let step_millis = step.num_milliseconds();
	let start_offset = start_millis % step_millis;
	let rounded_start = if start_offset == 0 { start } else { start + chrono::TimeDelta::milliseconds(step_millis - start_offset) };

	let end_millis = end.timestamp_millis();
	let end_offset = end_millis % step_millis;
	let rounded_end = if end_offset == 0 { end } else { end - chrono::TimeDelta::milliseconds(end_offset) };

	// Pre-allocate result vector for better performance - safe casting
	let time_diff_ms = (rounded_end - rounded_start).num_milliseconds();
	let estimated_points = if time_diff_ms > 0 {
		usize::try_from(time_diff_ms / step_millis)
			.unwrap_or(1000) // Fallback to reasonable default
			.saturating_add(1)
	} else {
		1
	};
	result.reserve(estimated_points);

	let mut current_time = rounded_start;

	while current_time <= rounded_end {
		let value = spline.evaluate(current_time)?;
		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Fast path for uniformly spaced data
fn linear_uniform_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval - check for zero interval
	let uniform_interval_ms = (measurements[1].timestamp - measurements[0].timestamp).num_milliseconds();

	// Handle case where measurements have identical or nearly identical timestamps
	if uniform_interval_ms == 0 {
		// All measurements have the same timestamp - use constant value interpolation
		let constant_value = &measurements[0].value;
		let mut current_time = start;

		while current_time <= end {
			result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	let uniform_interval = BigDecimal::from_i64(uniform_interval_ms).context("Failed to convert uniform interval to BigDecimal")?;

	let mut current_time = start;
	let data_start = measurements[0].timestamp;
	let data_end = measurements[measurements.len() - 1].timestamp;

	while current_time <= end {
		let value = if current_time < data_start || current_time > data_end {
			// Extrapolation - use boundary segments
			if current_time < data_start {
				// Extrapolate backward using first segment
				let dt = BigDecimal::from_i64((current_time - data_start).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
				let slope = (&measurements[1].value - &measurements[0].value) / &uniform_interval;
				&measurements[0].value + slope * dt
			} else {
				// Extrapolate forward using last segment
				let n = measurements.len();
				let dt = BigDecimal::from_i64((current_time - measurements[n - 2].timestamp).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
				let slope = (&measurements[n - 1].value - &measurements[n - 2].value) / &uniform_interval;
				&measurements[n - 2].value + slope * dt
			}
		} else {
			// Interpolation - use uniform spacing optimization with safe casting
			let time_from_start = (current_time - data_start).num_milliseconds();
			let segment_index = if time_from_start >= 0 && uniform_interval_ms > 0 { usize::try_from(time_from_start / uniform_interval_ms).unwrap_or(0).min(measurements.len().saturating_sub(2)) } else { 0 };

			let segment_start_time = measurements[segment_index].timestamp;
			let dt = BigDecimal::from_i64((current_time - segment_start_time).num_milliseconds()).context("Failed to convert segment time difference to BigDecimal")?;
			let slope = (&measurements[segment_index + 1].value - &measurements[segment_index].value) / &uniform_interval;
			&measurements[segment_index].value + slope * dt
		};

		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Optimized two-point linear interpolation
fn linear_two_point_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Check for identical timestamps first
	let dt_millis = (measurements[1].timestamp - measurements[0].timestamp).num_milliseconds();

	if dt_millis == 0 {
		// Handle identical timestamps - use constant value interpolation
		let constant_value = &measurements[0].value;
		let mut current_time = start;

		while current_time <= end {
			result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value: constant_value.clone() });
			current_time += step;
		}

		return Ok(result);
	}

	// Pre-compute slope once (safe now that we know dt_millis != 0)
	let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert timestamp difference to BigDecimal")?;
	let dy = &measurements[1].value - &measurements[0].value;
	let slope = dy / dt;
	let base_value = &measurements[0].value;
	let base_time = measurements[0].timestamp;

	let mut current_time = start;

	while current_time <= end {
		let time_diff = BigDecimal::from_i64((current_time - base_time).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
		let value = base_value + &slope * time_diff;

		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Check if measurements are uniformly spaced
fn is_uniformly_spaced(measurements: &[Measurement]) -> bool {
	if measurements.len() < 3 {
		return false;
	}

	let first_interval = measurements[1].timestamp - measurements[0].timestamp;

	// If the first interval is zero, this is not uniformly spaced (it's constant time)
	if first_interval.num_milliseconds() == 0 {
		return false;
	}

	let tolerance = chrono::Duration::milliseconds(50); // Tight tolerance for uniform detection

	measurements.windows(2).all(|pair| {
		let interval = pair[1].timestamp - pair[0].timestamp;
		(interval - first_interval).abs() < tolerance
	})
}

/// Linear spline implementation for efficient interpolation
struct LinearSpline {
	segments: Vec<LinearSegment>,
	time_bounds: Vec<DateTime<Utc>>,
}

impl LinearSpline {
	fn new(measurements: &[Measurement]) -> Result<Self> {
		let mut segments = Vec::with_capacity(measurements.len().saturating_sub(1));
		let mut time_bounds = Vec::with_capacity(measurements.len());

		for i in 0..measurements.len() - 1 {
			let segment = LinearSegment::fit_linear(&measurements[i], &measurements[i + 1])?;
			segments.push(segment);
			time_bounds.push(measurements[i].timestamp);
		}
		time_bounds.push(measurements[measurements.len() - 1].timestamp);

		Ok(Self { segments, time_bounds })
	}

	fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		// Handle boundary cases
		if target_time <= self.time_bounds[0] {
			return Ok(self.segments[0].intercept.clone());
		}
		if target_time >= self.time_bounds[self.time_bounds.len() - 1] {
			return Ok(self.segments[self.segments.len() - 1].evaluate_at_end());
		}

		// Find the appropriate segment
		for (i, &bound_time) in self.time_bounds.iter().enumerate().skip(1) {
			if target_time <= bound_time {
				let segment_idx = i - 1;
				let dt_millis = (target_time - self.time_bounds[segment_idx]).num_milliseconds();
				let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert time to BigDecimal")?;
				return Ok(self.segments[segment_idx].evaluate(dt));
			}
		}

		// Fallback to last segment
		let last_idx = self.segments.len() - 1;
		let dt_millis = (target_time - self.time_bounds[last_idx]).num_milliseconds();
		let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert time to BigDecimal")?;
		Ok(self.segments[last_idx].evaluate(dt))
	}
}

/// Linear segment with slope and intercept
#[derive(Debug, Clone)]
struct LinearSegment {
	slope: BigDecimal,
	intercept: BigDecimal,
	end_value: BigDecimal, // Store end value for identical timestamps
}

impl LinearSegment {
	/// Create a linear segment between two measurements
	fn fit_linear(p1: &Measurement, p2: &Measurement) -> Result<Self> {
		let dt_millis = (p2.timestamp - p1.timestamp).num_milliseconds();

		if dt_millis == 0 {
			// Handle identical timestamps - create constant segment
			return Ok(Self {
				slope: BigDecimal::zero(),
				intercept: p1.value.clone(),
				end_value: p1.value.clone(), // Use first value for consistency
			});
		}

		let dt = BigDecimal::from_i64(dt_millis).context("Failed to convert time difference to BigDecimal")?;
		let dy = &p2.value - &p1.value;
		let slope = dy / dt;

		// y = mx + b, where b is the y-intercept at the start time
		let intercept = p1.value.clone();
		let end_value = p2.value.clone();

		Ok(Self { slope, intercept, end_value })
	}

	/// Evaluate the linear function at time offset dt (in milliseconds)
	fn evaluate(&self, dt: BigDecimal) -> BigDecimal {
		&self.intercept + &self.slope * dt
	}

	/// Get the end value of this segment
	fn evaluate_at_end(&self) -> BigDecimal {
		self.end_value.clone()
	}
}

#[cfg(test)]
mod tests {
	use bigdecimal::BigDecimal;
	use chrono::{DateTime, TimeZone, Timelike, Utc};
	use uuid::Uuid;

	use super::*;

	fn create_measurement(dataset_id: Uuid, timestamp: DateTime<Utc>, value: f64) -> Measurement {
		Measurement { dataset_id, id: Uuid::new_v4(), timestamp, value: BigDecimal::from_f64(value).expect("Failed to create BigDecimal in test") }
	}

	#[test]
	fn test_linear_interpolation() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should have linear interpolation between points
		let mid_measurement = result.iter().find(|m| m.timestamp.second() == 5).unwrap();
		let expected_value = BigDecimal::from_f64(15.0).unwrap();
		assert_eq!(mid_measurement.value, expected_value);
	}

	#[test]
	fn test_linear_extrapolation_backward() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 200.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate linearly backwards
		// With slope of 10.0 per second, at t=0 (10 seconds before first point), value should be 0.0
		let first_measurement = result.iter().find(|m| m.timestamp.second() == 0).unwrap();
		let expected_value = BigDecimal::from_f64(0.0).unwrap();
		assert_eq!(first_measurement.value, expected_value);
	}

	#[test]
	fn test_linear_extrapolation_forward() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 15).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate linearly forwards
		// With slope of 10.0 per second, at t=20 (20 seconds from start), value should be 200.0
		let measurement_at_20 = result.iter().find(|m| m.timestamp.second() == 20).unwrap();
		let expected_value = BigDecimal::from_f64(200.0).unwrap();
		assert_eq!(measurement_at_20.value, expected_value);
	}

	#[test]
	fn test_linear_multiple_segments() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 30.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 15).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should have different slopes in different segments
		let measurement_at_5 = result.iter().find(|m| m.timestamp.second() == 5).unwrap();
		let expected_value_5 = BigDecimal::from_f64(5.0).unwrap(); // First segment slope = 1
		assert_eq!(measurement_at_5.value, expected_value_5);

		let measurement_at_15 = result.iter().find(|m| m.timestamp.second() == 15).unwrap();
		let expected_value_15 = BigDecimal::from_f64(20.0).unwrap(); // Second segment slope = 2, so 10 + 2*5 = 20
		assert_eq!(measurement_at_15.value, expected_value_15);
	}

	#[test]
	fn test_identical_timestamps() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds);

		// Should handle identical timestamps gracefully
		assert!(result.is_ok());
		let interpolated = result.unwrap();
		assert!(!interpolated.is_empty());

		// Should use the first measurement's value as constant
		for measurement in &interpolated {
			assert_eq!(measurement.value, BigDecimal::from_f64(10.0).unwrap());
		}
	}

	#[test]
	fn test_different_dataset_ids_error() {
		let measurements = vec![create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InconsistentDatasetIdsError)));
	}

	#[test]
	fn test_invalid_time_range_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InvalidTimeRangeError)));
	}

	#[test]
	fn test_insufficient_measurements_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InsufficientMeasurementsError)));
	}

	#[test]
	fn test_empty_measurements() {
		let measurements = vec![];
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();
		assert!(result.is_empty());
	}

	#[test]
	fn test_result_dataset_consistency() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();

		// All results should have the same dataset_id
		for measurement in &result {
			assert_eq!(measurement.dataset_id, dataset_id);
		}

		// All results should have unique IDs
		let mut ids: Vec<Uuid> = result.iter().map(|m| m.id).collect();
		ids.sort();
		ids.dedup();
		assert_eq!(ids.len(), result.len());
	}

	#[test]
	fn test_different_resolutions() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap(), 60.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 1, 0, 0).unwrap();

		// Test different resolutions
		for resolution in [Resolution::Microseconds, Resolution::Milliseconds, Resolution::Minutes, Resolution::Seconds] {
			let result = linear(measurements.clone(), start, end, resolution).unwrap();
			assert!(!result.is_empty(), "Resolution {resolution:?} should produce results");

			// Verify all results have the correct dataset_id
			for measurement in &result {
				assert_eq!(measurement.dataset_id, dataset_id);
			}
		}
	}
}
