use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{Error, Measurement, Resolution};

/// quadratic takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`, and returns a vector of interpolated or extrapolated (or both) `Measurement` using quadratic spline interpolation.
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
pub fn quadratic(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Measurement>> {
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

	// Build quadratic spline coefficients
	let spline = QuadraticSpline::new(&sorted_measurements)?;

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

struct QuadraticSpline {
	measurements: Vec<Measurement>,
	coefficients: Vec<QuadraticSegment>,
}

struct QuadraticSegment {
	a: BigDecimal, // quadratic coefficient
	b: BigDecimal, // linear coefficient
	c: BigDecimal, // constant coefficient
}

impl QuadraticSpline {
	fn new(measurements: &[Measurement]) -> Result<Self> {
		let n = measurements.len();
		if n < 2 {
			return Err(Error::InsufficientMeasurementsError.into());
		}

		let mut coefficients = Vec::with_capacity(n - 1);

		if n == 2 {
			// Linear interpolation for 2 points
			let dt = BigDecimal::from_i64((measurements[1].timestamp - measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal")?;
			let dy = measurements[1].value.clone() - measurements[0].value.clone();
			let slope = dy / dt;

			coefficients.push(QuadraticSegment { a: BigDecimal::zero(), b: slope, c: measurements[0].value.clone() });
		} else {
			// Quadratic spline for 3+ points
			for i in 0..n - 1 {
				if i + 2 < n {
					// Use three consecutive points for quadratic fit
					let segment = Self::fit_parabola(&measurements[i], &measurements[i + 1], &measurements[i + 2])?;
					coefficients.push(segment);
				} else {
					// For the last segment, use linear interpolation
					let dt = BigDecimal::from_i64((measurements[i + 1].timestamp - measurements[i].timestamp).num_milliseconds()).context(format!("Failed to convert timestamp difference to BigDecimal for final segment {i}"))?;
					let dy = measurements[i + 1].value.clone() - measurements[i].value.clone();
					let slope = dy / dt;

					coefficients.push(QuadraticSegment { a: BigDecimal::zero(), b: slope, c: measurements[i].value.clone() });
				}
			}
		}

		Ok(Self { measurements: measurements.to_vec(), coefficients })
	}

	fn fit_parabola(p1: &Measurement, p2: &Measurement, p3: &Measurement) -> Result<QuadraticSegment> {
		// Convert timestamps to relative time from p1
		let _t1 = BigDecimal::zero(); // t1 is always 0, so we don't need it in calculations
		let t2 = BigDecimal::from_i64((p2.timestamp - p1.timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for p2")?;
		let t3 = BigDecimal::from_i64((p3.timestamp - p1.timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for p3")?;

		let y1 = p1.value.clone();
		let y2 = p2.value.clone();
		let y3 = p3.value.clone();

		// Solve system of equations for quadratic: y = at² + bt + c
		// y1 = a*t1² + b*t1 + c  => y1 = c (since t1 = 0)
		// y2 = a*t2² + b*t2 + c
		// y3 = a*t3² + b*t3 + c

		let c = y1;

		// Rearrange to solve for a and b:
		// y2 - c = a*t2² + b*t2
		// y3 - c = a*t3² + b*t3

		let det = t2.clone() * t3.clone() * (t3.clone() - t2.clone());
		if det.is_zero() {
			// Points are collinear or timestamps are identical, fall back to linear
			let slope = if t2.is_zero() { BigDecimal::zero() } else { (y2 - c.clone()) / t2 };
			return Ok(QuadraticSegment { a: BigDecimal::zero(), b: slope, c });
		}

		// Using Cramer's rule to solve the 2x2 system
		// System: [t2² t2] [a] = [y2-c]
		//         [t3² t3] [b]   [y3-c]

		let t2_sq = t2.clone() * t2.clone();
		let t3_sq = t3.clone() * t3.clone();

		let rhs1 = y2 - c.clone();
		let rhs2 = y3 - c.clone();

		// Determinant of coefficient matrix: t2²*t3 - t3²*t2 = t2*t3*(t2-t3)
		let matrix_det = t2.clone() * t3.clone() * (t2.clone() - t3.clone());

		if matrix_det.is_zero() {
			// Fall back to linear if determinant is zero
			let slope = if t2.is_zero() { BigDecimal::zero() } else { rhs1 / t2 };
			return Ok(QuadraticSegment { a: BigDecimal::zero(), b: slope, c });
		}

		// Solve using Cramer's rule:
		// a = (rhs1*t3 - rhs2*t2) / matrix_det
		// b = (t2²*rhs2 - t3²*rhs1) / matrix_det

		let a = (rhs1.clone() * t3 - rhs2.clone() * t2) / matrix_det.clone();
		let b = (t2_sq * rhs2 - t3_sq * rhs1) / matrix_det;

		Ok(QuadraticSegment { a, b, c })
	}

	fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		let n = self.measurements.len();

		// Handle extrapolation
		if target_time <= self.measurements[0].timestamp {
			// Extrapolate using first segment
			let dt = BigDecimal::from_i64((target_time - self.measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for backward extrapolation")?;
			return Ok(self.evaluate_segment(0, dt));
		}

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
		let dt2 = dt.clone() * dt.clone();

		seg.a.clone() * dt2 + seg.b.clone() * dt + seg.c.clone()
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
	fn test_quadratic_interpolation() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 15).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should have smooth quadratic interpolation between points
	}

	#[test]
	fn test_quadratic_with_two_points() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should behave like linear interpolation with 2 points
		let mid_measurement = result.iter().find(|m| m.timestamp.second() == 5).unwrap();
		let expected_value = BigDecimal::from_f64(15.0).unwrap();
		assert_eq!(mid_measurement.value, expected_value);
	}

	#[test]
	fn test_quadratic_extrapolation_backward() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 900.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate smoothly backwards
	}

	#[test]
	fn test_quadratic_extrapolation_forward() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 35).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate smoothly forwards
	}

	#[test]
	fn test_different_dataset_ids_error() {
		let measurements = vec![create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(Uuid::new_v4(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InconsistentDatasetIdsError)));
	}

	#[test]
	fn test_invalid_time_range_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InvalidTimeRangeError)));
	}

	#[test]
	fn test_insufficient_measurements_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err().downcast_ref::<Error>(), Some(Error::InsufficientMeasurementsError)));
	}

	#[test]
	fn test_empty_measurements() {
		let measurements = vec![];
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();
		assert!(result.is_empty());
	}

	#[test]
	fn test_result_dataset_consistency() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 20.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 2).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 8).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

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
}
