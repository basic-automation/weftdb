use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use wide::f64x4;

use crate::{Error, Point, Resolution, Spline};

/// Main cubic spline interpolation function using Lagrange interpolation
pub fn cubic(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	if points.is_empty() {
		return Ok(Vec::new());
	}

	if points.len() < Spline::Cubic.number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	// Validate time range
	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	// Sort points by timestamp
	let mut sorted_points = points;
	sorted_points.sort_by_key(|m| m.timestamp);

	let mut current = start;
	let step = resolution.to_step();
	let mut results = Vec::new();

	while current <= end {
		let value = evaluate_lagrange_cubic(&sorted_points, current, resolution)?;
		results.push(Point { timestamp: current, value });
		current += step;
	}

	Ok(results)
}

/// Evaluate cubic Lagrange interpolation at target time
fn evaluate_lagrange_cubic(points: &[Point], target_time: DateTime<Utc>, resolution: Resolution) -> Result<BigDecimal> {
	let n = points.len();

	// Handle edge cases - use boundary values for extrapolation
	if target_time <= points[0].timestamp {
		return Ok(points[0].value.clone());
	}

	if target_time >= points[n - 1].timestamp {
		return Ok(points[n - 1].value.clone());
	}

	// Find segment
	let segment_idx = find_segment(points, target_time);

	// Select 4 points for cubic interpolation (match GPU exactly)
	let i1 = (1_usize).max((segment_idx).min(n - 2));
	let i0 = i1 - 1;
	let i2 = i1 + 1;
	let i3 = i1 + 2;

	// Boundary condition adjustments
	let (final_i0, final_i1, final_i2, final_i3) = if i3 >= n { (n - 4, n - 3, n - 2, n - 1) } else { (i0, i1, i2, i3) };

	// Convert timestamps to f64 for numerical computation
	let base_time = points[final_i0].timestamp;
	let t0 = timestamp_to_f64(points[final_i0].timestamp, base_time, resolution);
	let t1 = timestamp_to_f64(points[final_i1].timestamp, base_time, resolution);
	let t2 = timestamp_to_f64(points[final_i2].timestamp, base_time, resolution);
	let t3 = timestamp_to_f64(points[final_i3].timestamp, base_time, resolution);
	let target_t = timestamp_to_f64(target_time, base_time, resolution);

	let v0 = points[final_i0].value.to_f64().unwrap_or(0.0);
	let v1 = points[final_i1].value.to_f64().unwrap_or(0.0);
	let v2 = points[final_i2].value.to_f64().unwrap_or(0.0);
	let v3 = points[final_i3].value.to_f64().unwrap_or(0.0);

	let result = cubic_interpolate_lagrange(target_t, t0, t1, t2, t3, v0, v1, v2, v3);

	Ok(BigDecimal::from_f64(result).unwrap_or_default())
}

/// Lagrange cubic interpolation (matches GPU implementation)
fn cubic_interpolate_lagrange(t: f64, t0: f64, t1: f64, t2: f64, t3: f64, v0: f64, v1: f64, v2: f64, v3: f64) -> f64 {
	// Calculate Lagrange denominators
	let denom0 = (t0 - t1) * (t0 - t2) * (t0 - t3);
	let denom1 = (t1 - t0) * (t1 - t2) * (t1 - t3);
	let denom2 = (t2 - t0) * (t2 - t1) * (t2 - t3);
	let denom3 = (t3 - t0) * (t3 - t1) * (t3 - t2);

	// Handle degenerate cases
	if denom0.abs() < 1e-10 || denom1.abs() < 1e-10 || denom2.abs() < 1e-10 || denom3.abs() < 1e-10 {
		return 0.0;
	}

	// Lagrange basis functions
	let l0 = ((t - t1) * (t - t2) * (t - t3)) / denom0;
	let l1 = ((t - t0) * (t - t2) * (t - t3)) / denom1;
	let l2 = ((t - t0) * (t - t1) * (t - t3)) / denom2;
	let l3 = ((t - t0) * (t - t1) * (t - t2)) / denom3;

	l0 * v0 + l1 * v1 + l2 * v2 + l3 * v3
}

/// Find segment index for target time
fn find_segment(points: &[Point], target_time: DateTime<Utc>) -> usize {
	for (i, point) in points.iter().enumerate() {
		if target_time <= point.timestamp {
			return i.saturating_sub(1);
		}
	}
	points.len().saturating_sub(2)
}

/// Convert timestamp to f64 relative to base time
fn timestamp_to_f64(timestamp: DateTime<Utc>, base_time: DateTime<Utc>, resolution: Resolution) -> f64 {
	let duration = timestamp - base_time;
	match resolution {
		Resolution::Nanoseconds => duration.num_nanoseconds().map_or(0.0, |v| v as f64),
		Resolution::Microseconds => duration.num_microseconds().map_or(0.0, |v| v as f64),
		Resolution::Milliseconds => duration.num_milliseconds() as f64,
		Resolution::Seconds => duration.num_seconds() as f64,
		Resolution::Minutes => duration.num_minutes() as f64,
		Resolution::Hours => duration.num_hours() as f64,
		Resolution::Days => duration.num_days() as f64,
		Resolution::Weeks => duration.num_weeks() as f64,
		Resolution::Months => duration.num_days() as f64 / super::types::DAYS_IN_MONTH as f64,
		Resolution::Years => duration.num_days() as f64 / super::types::DAYS_IN_YEAR as f64,
	}
}

/// SIMD-optimized cubic interpolation using Lagrange method
pub fn cubic_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution) -> Result<Vec<Point>> {
	if points.len() < Spline::Cubic.number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Sort points by timestamp to match main cubic function
	let mut sorted_points = points.to_vec();
	sorted_points.sort_by_key(|m| m.timestamp);

	// Convert to f64 arrays for SIMD processing
	let base_time = sorted_points[0].timestamp;
	let point_times: Vec<f64> = sorted_points.iter().map(|p| timestamp_to_f64(p.timestamp, base_time, resolution)).collect();
	let point_values: Vec<f64> = sorted_points.iter().map(|p| p.value.to_f64().unwrap_or(0.0)).collect();

	let mut results = Vec::with_capacity(target_times.len());
	let mut target_f64s = Vec::with_capacity(target_times.len());

	// Pre-convert all target times to f64
	for target_time in target_times {
		target_f64s.push(timestamp_to_f64(*target_time, base_time, resolution));
	}

	// Process in SIMD batches of 4
	const SIMD_WIDTH: usize = 4;
	let chunks = target_f64s.chunks_exact(SIMD_WIDTH);
	let remainder = chunks.remainder();

	// Process SIMD chunks
	for chunk in chunks {
		let target_vec = f64x4::new([chunk[0], chunk[1], chunk[2], chunk[3]]);
		let result_vec = evaluate_lagrange_cubic_simd(&point_times, &point_values, target_vec);
		let result_array = result_vec.to_array();

		for (i, &target_time) in target_times[results.len()..results.len() + SIMD_WIDTH].iter().enumerate() {
			let value = BigDecimal::from_f64(result_array[i]).unwrap_or_default();
			results.push(Point { timestamp: target_time, value });
		}
	}

	// Process remainder sequentially
	for &target_f64 in remainder.iter() {
		let value = evaluate_lagrange_cubic_f64(&point_times, &point_values, target_f64);
		let target_time = target_times[results.len()];
		results.push(Point { timestamp: target_time, value: BigDecimal::from_f64(value).unwrap_or_default() });
	}

	Ok(results)
}

/// SIMD-optimized Lagrange cubic interpolation for 4 target times at once
fn evaluate_lagrange_cubic_simd(times: &[f64], values: &[f64], target_times: f64x4) -> f64x4 {
	let n = times.len();

	// For true SIMD optimization, we need to process all 4 lanes together
	// Check if all target times are within bounds
	let target_array = target_times.to_array();
	let first_time = times[0];
	let last_time = times[n - 1];

	let mut results = [0.0f64; 4];

	// Handle each target time in the SIMD vector
	for lane in 0..4 {
		let target_time = target_array[lane];

		// Handle edge cases
		if target_time <= first_time {
			results[lane] = values[0];
			continue;
		}

		if target_time >= last_time {
			results[lane] = values[n - 1];
			continue;
		}

		// Find segment
		let segment_idx = find_segment_f64_binary(times, target_time);

		// Select 4 points for cubic interpolation
		let i1 = (1_usize).max((segment_idx).min(n - 2));
		let i0 = i1 - 1;
		let i2 = i1 + 1;
		let i3 = i1 + 2;

		// Boundary condition adjustments
		let (final_i0, final_i1, final_i2, final_i3) = if i3 >= n { (n - 4, n - 3, n - 2, n - 1) } else { (i0, i1, i2, i3) };

		results[lane] = cubic_interpolate_lagrange(target_time, times[final_i0], times[final_i1], times[final_i2], times[final_i3], values[final_i0], values[final_i1], values[final_i2], values[final_i3]);
	}

	f64x4::new(results)
}

/// Evaluate Lagrange cubic interpolation with f64 arrays (for remainder processing)
fn evaluate_lagrange_cubic_f64(times: &[f64], values: &[f64], target_time: f64) -> f64 {
	let n = times.len();

	// Handle edge cases
	if target_time <= times[0] {
		return values[0];
	}

	if target_time >= times[n - 1] {
		return values[n - 1];
	}

	// Find segment
	let segment_idx = find_segment_f64_binary(times, target_time);

	// Select 4 points for cubic interpolation
	let i1 = (1_usize).max((segment_idx).min(n - 2));
	let i0 = i1 - 1;
	let i2 = i1 + 1;
	let i3 = i1 + 2;

	// Boundary condition adjustments
	let (final_i0, final_i1, final_i2, final_i3) = if i3 >= n { (n - 4, n - 3, n - 2, n - 1) } else { (i0, i1, i2, i3) };

	cubic_interpolate_lagrange(target_time, times[final_i0], times[final_i1], times[final_i2], times[final_i3], values[final_i0], values[final_i1], values[final_i2], values[final_i3])
}

/// Binary search for segment index (more efficient than linear search)
fn find_segment_f64_binary(times: &[f64], target_time: f64) -> usize {
	if target_time <= times[0] {
		return 0;
	}

	let mut left = 0;
	let mut right = times.len() - 1;

	while left < right - 1 {
		let mid = left + (right - left) / 2;
		if target_time < times[mid] {
			right = mid;
		} else {
			left = mid;
		}
	}

	left
}
