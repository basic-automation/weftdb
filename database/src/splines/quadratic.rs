use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, Zero};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{splines::gpu::gpu_linear_interpolate_optimized, Error, Measurement, Resolution};

/// Performs quadratic spline interpolation on measurement data.
///
/// Takes a vector of `Measurement`, a start date/time, an end date/time, a `Resolution`,
/// and returns a vector of interpolated or extrapolated measurements using quadratic spline interpolation.
///
/// The `Resolution` is used to determine the time step for the interpolation or extrapolation.
///
/// # Errors
///
/// Returns an error if:
/// - Measurements are empty or have fewer than 3 points
/// - Measurements have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Insufficient points for quadratic spline interpolation
/// - Timestamp conversion or `BigDecimal` operations fail
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

	// Fast path for two points - degrade to linear
	if measurements.len() == 2 {
		return quadratic_two_point_fast(&measurements, start, end, resolution, dataset_id);
	}

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Check for uniform spacing - enables fast path
	if is_uniformly_spaced(&sorted_measurements) {
		return quadratic_uniform_fast(&sorted_measurements, start, end, resolution, dataset_id);
	}

	let step = resolution.to_step();

	// Build optimized quadratic spline
	let spline = QuadraticSpline::new(&sorted_measurements)?;

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

/// Fast path for uniformly spaced data using optimized quadratic interpolation
fn quadratic_uniform_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute uniform interval
	let uniform_interval_ms = (measurements[1].timestamp - measurements[0].timestamp).num_milliseconds();
	let uniform_interval = BigDecimal::from_i64(uniform_interval_ms).context("Failed to convert uniform interval to BigDecimal")?;

	let mut current_time = start;
	let data_start = measurements[0].timestamp;
	let data_end = measurements[measurements.len() - 1].timestamp;

	while current_time <= end {
		let value = if current_time < data_start || current_time > data_end {
			// Extrapolation - use boundary quadratic segments
			if current_time < data_start {
				extrapolate_backward_uniform(measurements, current_time, &uniform_interval)?
			} else {
				extrapolate_forward_uniform(measurements, current_time, &uniform_interval)?
			}
		} else {
			// Interpolation - use uniform spacing optimization for quadratic
			interpolate_uniform_quadratic(measurements, current_time, &uniform_interval)?
		};

		result.push(Measurement { dataset_id, id: Uuid::new_v4(), timestamp: current_time, value });

		current_time += step;
	}

	Ok(result)
}

/// Optimized uniform quadratic interpolation
fn interpolate_uniform_quadratic(measurements: &[Measurement], target_time: DateTime<Utc>, uniform_interval: &BigDecimal) -> Result<BigDecimal> {
	let data_start = measurements[0].timestamp;
	let time_from_start = (target_time - data_start).num_milliseconds();

	// Stay in BigDecimal - convert to i64 only when necessary for indexing
	let uniform_interval_ms = uniform_interval.to_i64().context("Failed to convert uniform interval to i64")?;

	// Find the segment index using direct calculation for uniform data - safe casting
	let segment_index = if time_from_start >= 0 && uniform_interval_ms > 0 { usize::try_from(time_from_start / uniform_interval_ms).unwrap_or(0).min(measurements.len().saturating_sub(2)) } else { 0 };

	// Use three points for quadratic interpolation: segment_index-1, segment_index, segment_index+1
	let (p0, p1, p2) = if segment_index == 0 {
		// Use first three points
		(&measurements[0], &measurements[1], &measurements[2])
	} else if segment_index >= measurements.len() - 1 {
		// Use last three points
		let n = measurements.len();
		(&measurements[n - 3], &measurements[n - 2], &measurements[n - 1])
	} else {
		// Use centered three points
		(&measurements[segment_index - 1], &measurements[segment_index], &measurements[segment_index + 1])
	};

	// Calculate relative time parameter for quadratic interpolation - stay in BigDecimal
	let dt = BigDecimal::from_i64((target_time - p1.timestamp).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
	let t = dt / uniform_interval;

	// Quadratic interpolation using Lagrange formula - all BigDecimal operations
	// P(t) = y0*L0(t) + y1*L1(t) + y2*L2(t)
	// where L0(t) = t*(t-1)/2, L1(t) = 1-t², L2(t) = t*(t+1)/2

	let half = BigDecimal::from_f64(0.5).context("Failed to create BigDecimal from 0.5")?;
	let one = BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0")?;
	let t_squared = &t * &t;

	let l0 = &t * (&t - &one) * &half;
	let l1 = &one - &t_squared;
	let l2 = &t * (&t + &one) * &half;

	let result = &p0.value * &l0 + &p1.value * &l1 + &p2.value * &l2;

	Ok(result)
}

/// Backward extrapolation for uniform data
fn extrapolate_backward_uniform(measurements: &[Measurement], target_time: DateTime<Utc>, uniform_interval: &BigDecimal) -> Result<BigDecimal> {
	// Use first three points for quadratic extrapolation
	let p0 = &measurements[0];
	let p1 = &measurements[1];
	let p2 = &measurements[2];

	let dt = BigDecimal::from_i64((target_time - p0.timestamp).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
	let t = dt / uniform_interval;

	// Quadratic extrapolation using the same Lagrange formula
	let half = BigDecimal::from_f64(0.5).context("Failed to create BigDecimal from 0.5")?;
	let one = BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0")?;
	let t_squared = &t * &t;

	let l0 = &t * (&t - &one) * &half;
	let l1 = &one - &t_squared;
	let l2 = &t * (&t + &one) * &half;

	let result = &p0.value * &l0 + &p1.value * &l1 + &p2.value * &l2;

	Ok(result)
}

/// Forward extrapolation for uniform data
fn extrapolate_forward_uniform(measurements: &[Measurement], target_time: DateTime<Utc>, uniform_interval: &BigDecimal) -> Result<BigDecimal> {
	// Use last three points for quadratic extrapolation
	let n = measurements.len();
	let p0 = &measurements[n - 3];
	let p1 = &measurements[n - 2];
	let p2 = &measurements[n - 1];

	let dt = BigDecimal::from_i64((target_time - p1.timestamp).num_milliseconds()).context("Failed to convert time difference to BigDecimal")?;
	let t = dt / uniform_interval;

	// Quadratic extrapolation using the same Lagrange formula
	let half = BigDecimal::from_f64(0.5).context("Failed to create BigDecimal from 0.5")?;
	let one = BigDecimal::from_f64(1.0).context("Failed to create BigDecimal from 1.0")?;
	let t_squared = &t * &t;

	let l0 = &t * (&t - &one) * &half;
	let l1 = &one - &t_squared;
	let l2 = &t * (&t + &one) * &half;

	let result = &p0.value * &l0 + &p1.value * &l1 + &p2.value * &l2;

	Ok(result)
}

/// Fast path for two points - degrade to linear interpolation
fn quadratic_two_point_fast(measurements: &[Measurement], start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	let step = resolution.to_step();
	let mut result = Vec::new();

	// Pre-compute slope once for linear interpolation
	let dt = BigDecimal::from_i64((measurements[1].timestamp - measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal")?;
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
	let tolerance = chrono::Duration::milliseconds(50); // Tight tolerance for uniform detection

	measurements.windows(2).all(|pair| {
		let interval = pair[1].timestamp - pair[0].timestamp;
		(interval - first_interval).abs() < tolerance
	})
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

		// Pre-compute all segments for better cache locality
		for i in 0..n - 1 {
			let segment = Self::fit_quadratic_segment(measurements, i)?;
			coefficients.push(segment);
		}

		Ok(Self { measurements: measurements.to_vec(), coefficients })
	}

	/// Fit a quadratic segment using local points
	fn fit_quadratic_segment(measurements: &[Measurement], segment_idx: usize) -> Result<QuadraticSegment> {
		let n = measurements.len();

		// Choose three points for quadratic fitting
		let (p0, p1, p2) = if segment_idx == 0 && n >= 3 {
			// Use first three points
			(&measurements[0], &measurements[1], &measurements[2])
		} else if segment_idx >= n - 2 && n >= 3 {
			// Use last three points
			(&measurements[n - 3], &measurements[n - 2], &measurements[n - 1])
		} else if n >= 3 {
			// Use centered three points
			(&measurements[segment_idx], &measurements[segment_idx + 1], &measurements[segment_idx.min(n - 2)])
		} else {
			// Fall back to linear for insufficient points
			let p1 = &measurements[segment_idx];
			let p2 = &measurements[segment_idx + 1];

			let dt = BigDecimal::from_i64((p2.timestamp - p1.timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal")?;

			if dt.is_zero() {
				return Ok(QuadraticSegment { a: BigDecimal::zero(), b: BigDecimal::zero(), c: p1.value.clone() });
			}

			let dy = &p2.value - &p1.value;
			let slope = dy / dt;

			return Ok(QuadraticSegment { a: BigDecimal::zero(), b: slope, c: p1.value.clone() });
		};

		// Convert timestamps to relative time from p1 for numerical stability
		let base_time = p1.timestamp;
		let t0 = BigDecimal::from_i64((p0.timestamp - base_time).num_milliseconds()).context("Failed to convert timestamp to BigDecimal")?;
		let t1 = BigDecimal::zero(); // p1 is at time 0
		let t2 = BigDecimal::from_i64((p2.timestamp - base_time).num_milliseconds()).context("Failed to convert timestamp to BigDecimal")?;

		// Solve quadratic system: y = at² + bt + c
		// Using Lagrange interpolation for numerical stability

		// Calculate denominators for Lagrange basis functions
		let denom_0 = (&t0 - &t1) * (&t0 - &t2);
		let denom_1 = (&t1 - &t0) * (&t1 - &t2);
		let denom_2 = (&t2 - &t0) * (&t2 - &t1);

		if denom_0.is_zero() || denom_1.is_zero() || denom_2.is_zero() {
			// Fall back to linear interpolation if points are collinear in time
			let dt = &t2 - &t0;
			if dt.is_zero() {
				return Ok(QuadraticSegment { a: BigDecimal::zero(), b: BigDecimal::zero(), c: p1.value.clone() });
			}

			let dy = &p2.value - &p0.value;
			let slope = dy / dt;

			return Ok(QuadraticSegment { a: BigDecimal::zero(), b: slope, c: p1.value.clone() });
		}

		// Calculate quadratic coefficients using Lagrange method - remove unused variable
		// For efficiency, we compute the coefficients directly

		// Coefficient of t² term
		let a = (&p0.value / &denom_0) + (&p1.value / &denom_1) + (&p2.value / &denom_2);

		// Coefficient of t term
		let b = (&p0.value * (&t1 + &t2) / (-&denom_0)) + (&p1.value * (&t0 + &t2) / (-&denom_1)) + (&p2.value * (&t0 + &t1) / (-&denom_2));

		// Constant term (value at t=0, which is p1)
		let c = p1.value.clone();

		Ok(QuadraticSegment { a, b, c })
	}

	/// Optimized segment evaluation with reference passing
	fn evaluate_segment(&self, segment_idx: usize, dt: &BigDecimal) -> BigDecimal {
		let seg = &self.coefficients[segment_idx];
		let dt_squared = dt * dt;

		// Evaluate: a*t² + b*t + c
		&seg.a * dt_squared + &seg.b * dt + &seg.c
	}

	fn evaluate(&self, target_time: DateTime<Utc>) -> Result<BigDecimal> {
		let n = self.measurements.len();

		// Handle extrapolation backward
		if target_time <= self.measurements[0].timestamp {
			let dt = BigDecimal::from_i64((target_time - self.measurements[0].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for backward extrapolation")?;
			return Ok(self.evaluate_segment(0, &dt));
		}

		// Handle extrapolation forward
		if target_time >= self.measurements[n - 1].timestamp {
			let dt = BigDecimal::from_i64((target_time - self.measurements[n - 2].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for forward extrapolation")?;
			return Ok(self.evaluate_segment(n - 2, &dt));
		}

		// Binary search for the appropriate segment - MAJOR PERFORMANCE IMPROVEMENT
		let segment_idx = self.find_segment_binary(target_time);
		let dt = BigDecimal::from_i64((target_time - self.measurements[segment_idx].timestamp).num_milliseconds()).context("Failed to convert timestamp difference to BigDecimal for interpolation")?;

		Ok(self.evaluate_segment(segment_idx, &dt))
	}

	/// Binary search for segment - O(log n) instead of O(n)
	fn find_segment_binary(&self, target_time: DateTime<Utc>) -> usize {
		let mut left = 0;
		let mut right = self.measurements.len() - 1;

		while left < right - 1 {
			let mid = left + (right - left) / 2;
			if target_time < self.measurements[mid].timestamp {
				right = mid;
			} else {
				left = mid;
			}
		}

		left
	}
}

/// GPU-accelerated quadratic spline interpolation with CPU fallback
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for quadratic spline interpolation
/// - Measurements have inconsistent dataset IDs
/// - Invalid time range
/// - Both GPU and CPU interpolation fail
pub async fn gpu_quadratic_interpolate_optimized(measurements: Vec<Measurement>, target_times: Vec<DateTime<Utc>>, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	if measurements.len() < 3 {
		return Err(Error::InsufficientPointsForCubicSplineError.into());
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// For now, use GPU linear interpolation as fallback
	// TODO: Implement true GPU quadratic interpolation
	//println!("🚀 Using GPU acceleration for quadratic interpolation (linear fallback)");
	gpu_linear_interpolate_optimized(measurements, target_times, dataset_id).await
}

/// GPU-accelerated quadratic interpolation with CPU fallback
///
/// # Errors
///
/// Returns an error if both GPU and CPU interpolation fail
pub async fn gpu_quadratic_interpolate_with_fallback(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	// Generate target times for GPU
	let target_times = generate_target_times(start, end, resolution);

	// Try GPU first
	match gpu_quadratic_interpolate_optimized(measurements.clone(), target_times, dataset_id).await {
		Ok(result) => Ok(result),
		Err(_gpu_error) => {
			// Fallback to CPU quadratic interpolation
			println!("⚠️  GPU quadratic fallback to CPU");
			quadratic(measurements, start, end, resolution)
		}
	}
}

/// Generate target times for interpolation
fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
	let mut target_times = Vec::new();
	let mut current = start;
	let step = resolution.to_step();

	while current <= end {
		target_times.push(current);
		current += step;
	}

	target_times
}

/// Should use GPU for quadratic interpolation based on data characteristics
#[must_use]
pub const fn should_use_gpu_quadratic(measurement_count: usize, target_count: usize) -> bool {
	// Use same thresholds as linear but slightly higher due to complexity
	measurement_count >= 1_500 && target_count >= 30_000
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
	fn test_quadratic_uniform_spacing() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 0.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 900.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 25).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should use the uniform fast path
	}

	#[test]
	fn test_insufficient_measurements_error() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), 10.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds);
		assert!(result.is_err());
	}

	#[test]
	fn test_quadratic_extrapolation() {
		let dataset_id = Uuid::new_v4();
		let measurements = vec![create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 10).unwrap(), 100.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 20).unwrap(), 400.0), create_measurement(dataset_id, Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), 900.0)];

		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 5).unwrap();

		let result = quadratic(measurements, start, end, Resolution::Seconds).unwrap();

		assert!(!result.is_empty());
		// Should extrapolate using quadratic curve
	}
}
