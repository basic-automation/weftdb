use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

/// linear takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`, and returns a vector of interpolated or extrapolated (or both) `Measurement` using linear interpolation.
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
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

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	let step = resolution.to_step();

	// Build linear spline coefficients
	let spline = LinearSpline::new(&sorted_measurements)?;

	let mut result = Vec::new();

	// round start and end to the nearest step, but ensure we don't go before the requested start time
	let start_millis = start.timestamp_millis();
	let step_millis = step.num_milliseconds();
	let start_offset = start_millis % step_millis;
	let rounded_start = if start_offset == 0 { start } else { start + chrono::TimeDelta::milliseconds(step_millis - start_offset) };

	let end_millis = end.timestamp_millis();
	let end_offset = end_millis % step_millis;
	let rounded_end = if end_offset == 0 { end } else { end - chrono::TimeDelta::milliseconds(end_offset) };

	let mut current_time = rounded_start;

	while current_time <= rounded_end {
		let value = spline.evaluate(current_time)?;
		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

struct LinearSpline {
	measurements: Vec<Measurement>,
	coefficients: Vec<LinearSegment>,
}

struct LinearSegment {
	slope: BigDecimal,     // linear coefficient
	intercept: BigDecimal, // constant coefficient
}

impl LinearSpline {
	fn new(measurements: &[Measurement]) -> Result<Self> {
		let n = measurements.len();
		if n < 2 {
			return Err(Error::InsufficientMeasurementsError.into());
		}

		let mut coefficients = Vec::with_capacity(n - 1);

		// Create linear segments between consecutive points
		for i in 0..n - 1 {
			let segment = Self::fit_linear(&measurements[i], &measurements[i + 1])?;
			coefficients.push(segment);
		}

		Ok(Self { measurements: measurements.to_vec(), coefficients })
	}

	fn fit_linear(p1: &Measurement, p2: &Measurement) -> Result<LinearSegment> {
		let dt = BigDecimal::from_i64((p2.timestamp - p1.timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal")?;

		if dt.is_zero() {
			// Handle case where timestamps are identical
			return Ok(LinearSegment { slope: BigDecimal::zero(), intercept: p1.value.clone() });
		}

		let dy = p2.value.clone() - p1.value.clone();
		let slope = dy / dt;

		// y = mx + b, where b = y1 - m*x1
		// Since we're using relative time from p1, x1 = 0, so b = y1
		let intercept = p1.value.clone();

		Ok(LinearSegment { slope, intercept })
	}

	fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		let n = self.measurements.len();

		// Handle extrapolation backward
		if target_time <= self.measurements[0].timestamp {
			// Extrapolate using first segment
			let dt = BigDecimal::from_i64((target_time - self.measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for backward extrapolation")?;
			return Ok(self.evaluate_segment(0, dt));
		}

		// Handle extrapolation forward
		if target_time >= self.measurements[n - 1].timestamp {
			// Extrapolate using last segment
			let dt = BigDecimal::from_i64((target_time - self.measurements[n - 2].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for forward extrapolation")?;
			return Ok(self.evaluate_segment(n - 2, dt));
		}

		// Find the appropriate segment for interpolation
		for i in 0..n - 1 {
			if target_time >= self.measurements[i].timestamp && target_time <= self.measurements[i + 1].timestamp {
				let dt = BigDecimal::from_i64((target_time - self.measurements[i].timestamp).num_milliseconds()).context(format!("Failed to convert timestamp difference to BigDecimal for interpolation segment {i}"))?;
				return Ok(self.evaluate_segment(i, dt));
			}
		}

		// Fallback
		Ok(self.measurements[0].value.clone())
	}

	fn evaluate_segment(&self, segment_idx: usize, dt: BigDecimal) -> BigDecimal {
		let seg = &self.coefficients[segment_idx];
		seg.slope.clone() * dt + seg.intercept.clone()
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

		let result = linear(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should handle identical timestamps gracefully
		for measurement in &result {
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
		for resolution in [Resolution::Minutes, Resolution::Seconds] {
			let result = linear(measurements.clone(), start, end, resolution).unwrap();
			assert!(!result.is_empty());
		}
	}
}
