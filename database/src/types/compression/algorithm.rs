//! Core compression algorithm for simplifying measurements.
//!
//! This module provides the `simplify_with_aggressiveness` function that reduces
//! measurement count by interpolating to a coarser resolution and removing
//! redundant points.

use anyhow::Result;
use bigdecimal::BigDecimal;
use splimes::{Point, Resolution, Spline};

use crate::Measurement;

/// Applies simplification with aggressiveness parameter.
///
/// # Arguments
/// * `measurements` - Input measurements (must be sorted by timestamp)
/// * `aggressiveness` - Value from 0.0 to 1.0
///   - 0.0 = no compression (returns original)
///   - 1.0 = maximum compression (returns only first and last points)
/// * `base_resolution` - Coarsest resolution at max aggressiveness (e.g., Days)
/// * `original_resolution` - The original resolution of the data
/// * `interpolation_method` - Spline method for interpolation
///
/// # Algorithm
/// 1. If aggressiveness is 0, return original
/// 2. If aggressiveness is 1, return only endpoints
/// 3. Otherwise:
///    - Interpolate to a coarser resolution based on aggressiveness
///    - Apply slope-change simplification to remove redundant points
///    - Return simplified points
///
/// # Errors
///
/// Returns an error if interpolation fails.
///
/// # Panics
///
/// Panics if measurements is non-empty but has no first/last elements (impossible case).
pub async fn simplify_with_aggressiveness(measurements: &[Measurement], aggressiveness: f64, base_resolution: Resolution, original_resolution: Resolution, interpolation_method: Spline) -> Result<Vec<Point>> {
	// Validate inputs
	if measurements.is_empty() {
		return Ok(Vec::new());
	}

	if measurements.len() <= 2 {
		return Ok(measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect());
	}

	let aggressiveness = aggressiveness.clamp(0.0, 1.0);

	// Case: no compression
	if aggressiveness == 0.0 {
		return Ok(measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect());
	}

	// Case: maximum compression - return only endpoints
	if aggressiveness >= 0.99 {
		let first = measurements.first().unwrap();
		let last = measurements.last().unwrap();
		return Ok(vec![Point { timestamp: first.timestamp(), value: first.value().clone() }, Point { timestamp: last.timestamp(), value: last.value().clone() }]);
	}

	// Calculate target resolution based on aggressiveness
	// aggressiveness 0 = original resolution, aggressiveness 1 = base (coarsest) resolution
	let target_resolution = interpolate_resolution(original_resolution, base_resolution, aggressiveness);

	// Convert to points
	let mut points: Vec<Point> = measurements.iter().map(|m| Point { timestamp: m.timestamp(), value: m.value().clone() }).collect();

	let start = measurements.first().unwrap().timestamp();
	let end = measurements.last().unwrap().timestamp();

	// If start == end (all measurements within same time unit), return as-is
	if start >= end {
		return Ok(points);
	}

	// Interpolate to the target (coarser) resolution
	let interpolated = splimes::auto_interpolate(&mut points, start, end, target_resolution, interpolation_method).await?;

	// Apply slope-change simplification to remove redundant points
	let simplified = simplify_by_slope_change(&interpolated);

	Ok(simplified)
}

/// Interpolate between two resolutions based on aggressiveness.
///
/// Returns a resolution that's between original (at aggressiveness=0)
/// and base (at aggressiveness=1).
#[allow(clippy::cast_possible_truncation)]
fn interpolate_resolution(original: Resolution, base: Resolution, aggressiveness: f64) -> Resolution {
	let original_rank = resolution_rank(original);
	let base_rank = resolution_rank(base);

	// Interpolate between ranks (truncation is intentional - we want discrete resolution steps)
	let target_rank = original_rank + (f64::from(base_rank - original_rank) * aggressiveness) as i32;

	rank_to_resolution(target_rank)
}

/// Get a numeric rank for a resolution (higher = coarser).
const fn resolution_rank(resolution: Resolution) -> i32 {
	match resolution {
		Resolution::Nanoseconds => 0,
		Resolution::Microseconds => 1,
		Resolution::Milliseconds => 2,
		Resolution::Seconds => 3,
		Resolution::Minutes => 4,
		Resolution::Hours => 5,
		Resolution::Days => 6,
		Resolution::Weeks => 7,
		Resolution::Months => 8,
		Resolution::Years => 9,
	}
}

/// Convert a rank back to a resolution.
const fn rank_to_resolution(rank: i32) -> Resolution {
	match rank {
		0 => Resolution::Nanoseconds,
		1 => Resolution::Microseconds,
		2 => Resolution::Milliseconds,
		3 => Resolution::Seconds,
		4 => Resolution::Minutes,
		5 => Resolution::Hours,
		6 => Resolution::Days,
		7 => Resolution::Weeks,
		8 => Resolution::Months,
		_ => Resolution::Years,
	}
}

/// Simplify points by keeping only those where the slope direction changes.
///
/// This is similar to the existing `simplify_transformation` in batches,
/// but works directly on Point vectors.
fn simplify_by_slope_change(points: &[Point]) -> Vec<Point> {
	if points.len() <= 2 {
		return points.to_vec();
	}

	let mut kept_points = Vec::with_capacity(points.len());

	// Always keep the first point
	kept_points.push(points[0].clone());

	// Process middle points
	for i in 1..points.len() - 1 {
		let prev = &points[i - 1];
		let curr = &points[i];
		let next = &points[i + 1];

		// Calculate slopes
		let prev_to_curr_slope = calculate_slope(prev, curr);
		let curr_to_next_slope = calculate_slope(curr, next);

		// Keep point if slope direction changes
		if slope_sign(&prev_to_curr_slope) != slope_sign(&curr_to_next_slope) {
			kept_points.push(curr.clone());
		}
	}

	// Always keep the last point
	if points.len() > 1 {
		kept_points.push(points.last().unwrap().clone());
	}

	kept_points
}

/// Calculate slope between two points.
fn calculate_slope(p1: &Point, p2: &Point) -> BigDecimal {
	use bigdecimal::Zero;

	let dt = (p2.timestamp - p1.timestamp).num_milliseconds();
	if dt == 0 {
		return BigDecimal::zero();
	}

	(&p2.value - &p1.value) / BigDecimal::from(dt)
}

/// Get the sign of a slope (-1, 0, or 1).
fn slope_sign(slope: &BigDecimal) -> i8 {
	use std::cmp::Ordering;

	use bigdecimal::Zero;

	match slope.cmp(&BigDecimal::zero()) {
		Ordering::Less => -1,
		Ordering::Equal => 0,
		Ordering::Greater => 1,
	}
}

#[cfg(test)]
mod tests {
	use bigdecimal::FromPrimitive;
	use chrono::Utc;

	use super::*;

	#[test]
	fn test_resolution_interpolation() {
		// At aggressiveness 0, should return original
		let res = interpolate_resolution(Resolution::Minutes, Resolution::Days, 0.0);
		assert_eq!(res, Resolution::Minutes);

		// At aggressiveness 1, should return base
		let res = interpolate_resolution(Resolution::Minutes, Resolution::Days, 1.0);
		assert_eq!(res, Resolution::Days);

		// At aggressiveness 0.5, should be halfway (Hours is rank 5, between Minutes=4 and Days=6)
		let res = interpolate_resolution(Resolution::Minutes, Resolution::Days, 0.5);
		assert_eq!(res, Resolution::Hours);
	}

	#[test]
	fn test_simplify_by_slope_change() {
		let now = Utc::now();
		let points = vec![
			Point { timestamp: now, value: BigDecimal::from_f64(0.0).unwrap() },                               // hour 0
			Point { timestamp: now + chrono::Duration::hours(1), value: BigDecimal::from_f64(10.0).unwrap() }, // hour 1: slope +10, next +10 -> same, skip
			Point { timestamp: now + chrono::Duration::hours(2), value: BigDecimal::from_f64(20.0).unwrap() }, // hour 2: slope +10, next -5 -> change, KEEP
			Point { timestamp: now + chrono::Duration::hours(3), value: BigDecimal::from_f64(15.0).unwrap() }, // hour 3: slope -5, next -5 -> same, skip
			Point { timestamp: now + chrono::Duration::hours(4), value: BigDecimal::from_f64(10.0).unwrap() }, // hour 4
		];

		let simplified = simplify_by_slope_change(&points);

		// Should keep: first (hour 0, value 0), peak (hour 2, value 20), last (hour 4, value 10)
		// Hour 2 is kept because slope changes from +10 to -5 (positive to negative)
		assert_eq!(simplified.len(), 3);
		assert_eq!(simplified[0].value, BigDecimal::from_f64(0.0).unwrap());
		assert_eq!(simplified[1].value, BigDecimal::from_f64(20.0).unwrap());
		assert_eq!(simplified[2].value, BigDecimal::from_f64(10.0).unwrap());
	}

	#[test]
	fn test_slope_sign() {
		assert_eq!(slope_sign(&BigDecimal::from_f64(1.5).unwrap()), 1);
		assert_eq!(slope_sign(&BigDecimal::from_f64(-1.5).unwrap()), -1);
		assert_eq!(slope_sign(&BigDecimal::from_f64(0.0).unwrap()), 0);
	}
}
