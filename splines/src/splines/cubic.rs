use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use wide::f64x4;
use wide::CmpLt;

use super::types::CubicSpline;
use crate::{Error, Point, Resolution, Spline};
use crate::splines::{SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR, SIMD_BATCH_SIZE};

/// Main cubic spline interpolation function
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points for cubic spline interpolation (< 2 points)
/// - points have inconsistent dataset IDs
/// - Invalid time range (start >= end)
/// - Timestamp conversion or `BigDecimal` operations fail
/// - Spline evaluation encounters numerical errors
pub fn cubic(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Result<Vec<Point>> {
	if points.is_empty() {
		return Ok(Vec::new()); // Return empty vector for empty input
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

	let spline = CubicSpline::new(&sorted_points, resolution)?;

	let mut current = start;
	let step = resolution.to_step();
	let mut results = Vec::new();

	while current <= end {
		let value = spline.evaluate(current)?;
		results.push(Point { timestamp: current, value });
		current += step;
	}

	Ok(results)
}


/// SIMD-optimized cubic interpolation.
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points (< 4 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn cubic_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution) -> Result<Vec<Point>> {
    if points.len() < Spline::Cubic.number_of_points_required() {
        bail!(Error::InsufficientPointsError);
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    // Convert to f64 arrays for SIMD processing
    #[allow(clippy::cast_precision_loss)]
    let input_times: Vec<f64> = points
        .iter()
        .map(|m| match resolution {
            Resolution::Nanoseconds => m.timestamp.timestamp_nanos_opt().map_or(0.0, |v| v as f64),
            Resolution::Microseconds => m.timestamp.timestamp_micros() as f64,
            Resolution::Milliseconds => m.timestamp.timestamp_millis() as f64,
            Resolution::Seconds => m.timestamp.timestamp() as f64,
            Resolution::Minutes => m.timestamp.timestamp() as f64 / SECONDS_IN_MINUTE as f64,
            Resolution::Hours => m.timestamp.timestamp() as f64 / SECONDS_IN_HOUR as f64,
            Resolution::Days => m.timestamp.timestamp() as f64 / SECONDS_IN_DAY as f64,
            Resolution::Weeks => m.timestamp.timestamp() as f64 / SECONDS_IN_WEEK as f64,
            Resolution::Months => m.timestamp.timestamp() as f64 / SECONDS_IN_MONTH as f64,
            Resolution::Years => m.timestamp.timestamp() as f64 / SECONDS_IN_YEAR as f64,
        })
        .collect();
    let input_values: Vec<f64> = points.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

    // Process target times in SIMD batches
    let mut results = Vec::with_capacity(target_times.len());

    for target_chunk in target_times.chunks(SIMD_BATCH_SIZE) {
        #[allow(clippy::cast_precision_loss)]
        let target_f64s: Vec<f64> = target_chunk
            .iter()
            .map(|t| match resolution {
                Resolution::Nanoseconds => t.timestamp_nanos_opt().map_or(0.0, |v| v as f64),
                Resolution::Microseconds => t.timestamp_micros() as f64,
                Resolution::Milliseconds => t.timestamp_millis() as f64,
                Resolution::Seconds => t.timestamp() as f64,
                Resolution::Minutes => t.timestamp() as f64 / SECONDS_IN_MINUTE as f64,
                Resolution::Hours => t.timestamp() as f64 / SECONDS_IN_HOUR as f64,
                Resolution::Days => t.timestamp() as f64 / SECONDS_IN_DAY as f64,
                Resolution::Weeks => t.timestamp() as f64 / SECONDS_IN_WEEK as f64,
                Resolution::Months => t.timestamp() as f64 / SECONDS_IN_MONTH as f64,
                Resolution::Years => t.timestamp() as f64 / SECONDS_IN_YEAR as f64,
            })
            .collect();

        // Pad to SIMD width
        let mut padded_targets = [0.0; SIMD_BATCH_SIZE];
        let chunk_size = target_f64s.len();
        padded_targets[..chunk_size].copy_from_slice(&target_f64s);

        if chunk_size < SIMD_BATCH_SIZE && !target_f64s.is_empty() {
            let last_value = target_f64s[chunk_size - 1];
            for target in &mut padded_targets[chunk_size..] {
                *target = last_value;
            }
        }

        let target_simd = f64x4::new(padded_targets);
        let result_simd = simd_cubic_interpolate(&input_times, &input_values, target_simd);

        let result_array = result_simd.to_array();
        results.extend_from_slice(&result_array[..chunk_size]);
    }

    // Convert back to Points
    let mut interpolated = Vec::with_capacity(target_times.len());
    for (i, &value) in results.iter().enumerate() {
        interpolated.push(Point { timestamp: target_times[i], value: BigDecimal::from_f64(value).unwrap_or_else(|| BigDecimal::from(0)) });
    }

    Ok(interpolated)
}

/// SIMD cubic interpolation core function
fn simd_cubic_interpolate(input_times: &[f64], input_values: &[f64], target_times: f64x4) -> f64x4 {
    // Assumption: all target times in the vector fall into the same segment.
    // Use the first lane to determine the segment.
    let target_time_scalar = target_times.to_array()[0];
    let n = input_times.len();

    // Find the segment for the target time. We need 4 points.
    let i1 = match input_times.binary_search_by(|t| t.partial_cmp(&target_time_scalar).unwrap()) {
        Ok(i) => i,
        Err(i) => i,
    }
    .max(1)
    .min(n - 2); // Clamp to ensure i0, i1, i2 are valid

    let i0 = i1 - 1;
    let i2 = i1 + 1;
    let i3 = i1 + 2;

    // Boundary condition adjustments to ensure we have 4 valid points
    let (i0, i1, i2, i3) = if i3 >= n { (n - 4, n - 3, n - 2, n - 1) } else { (i0, i1, i2, i3) };

    // Load segment points into SIMD vectors
    let t0 = f64x4::splat(input_times[i0]);
    let t1 = f64x4::splat(input_times[i1]);
    let t2 = f64x4::splat(input_times[i2]);
    let t3 = f64x4::splat(input_times[i3]);

    let v0 = f64x4::splat(input_values[i0]);
    let v1 = f64x4::splat(input_values[i1]);
    let v2 = f64x4::splat(input_values[i2]);
    let v3 = f64x4::splat(input_values[i3]);

    // Perform Lagrange cubic interpolation using SIMD operations
    let denom0 = (t0 - t1) * (t0 - t2) * (t0 - t3);
    let denom1 = (t1 - t0) * (t1 - t2) * (t1 - t3);
    let denom2 = (t2 - t0) * (t2 - t1) * (t2 - t3);
    let denom3 = (t3 - t0) * (t3 - t1) * (t3 - t2);

    // Create masks for fallback conditions (e.g., division by zero)
    let fallback_mask = denom0.abs().cmp_lt(f64x4::splat(1e-10))
        | denom1.abs().cmp_lt(f64x4::splat(1e-10))
        | denom2.abs().cmp_lt(f64x4::splat(1e-10))
        | denom3.abs().cmp_lt(f64x4::splat(1e-10));

    // Calculate Lagrange basis polynomials
    let l0 = ((target_times - t1) * (target_times - t2) * (target_times - t3)) / denom0;
    let l1 = ((target_times - t0) * (target_times - t2) * (target_times - t3)) / denom1;
    let l2 = ((target_times - t0) * (target_times - t1) * (target_times - t3)) / denom2;
    let l3 = ((target_times - t0) * (target_times - t1) * (target_times - t2)) / denom3;

    // Calculate final value using fused multiply-add for precision and performance
    let val0 = v0 * l0;
    let val1 = v1 * l1;
    let val2 = v2 * l2;
    let val3 = v3 * l3;
    let result = val0 + val1 + val2 + val3;

    // Use a simple average of the two center points as a fallback
    let fallback_value = (v1 + v2) * f64x4::splat(0.5);
    fallback_mask.blend(fallback_value, result)
}
