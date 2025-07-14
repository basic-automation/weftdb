use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive, One, Zero};
use chrono::{DateTime, Utc};
use wide::f64x4;
use wide::CmpLt;

use crate::{
	Error, Point, Resolution, Spline, splines::{SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR, SIMD_BATCH_SIZE}
};

/// Performs polynomial interpolation of a specified degree on measurement data.
///
/// This function uses Lagrange interpolation. For each point in the output series,
/// it selects the `degree + 1` nearest input points to construct and evaluate
/// the interpolating polynomial. This method is suitable for any polynomial degree
/// but can be computationally intensive for high degrees or large output series.
///
/// # Arguments
///
/// * `points` - A vector of `Point` structs to interpolate. Must contain at least `degree + 1` points.
/// * `start` - The start `DateTime<Utc>` for the output series.
/// * `end` - The end `DateTime<Utc>` for the output series.
/// * `resolution` - The `Resolution` of the output data points.
/// * `degree` - The degree of the polynomial to use for interpolation.
///
/// # Errors
///
/// Returns an error if:
/// - `points` has fewer than `degree + 1` elements.
/// - The time range is invalid (`start >= end`).
/// - Timestamp or value conversions fail.
pub fn polynomial(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, degree: usize) -> Result<Vec<Point>> {
	if points.is_empty() {
		return Ok(Vec::new());
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	if points.len() <= Spline::Polynomial(degree).number_of_points_required() {
		bail!(Error::InsufficientPointsError);
	}

	// Sort points by timestamp for efficient processing
	let mut sorted_points = points;
	sorted_points.sort_by_key(|p| p.timestamp);

	let step = resolution.to_step();
	let mut results = Vec::new();
	let mut current_time = start;

	while current_time <= end {
		let value = evaluate_polynomial(&sorted_points, current_time, degree, resolution)?;
		results.push(Point { timestamp: current_time, value });
		current_time += step;
	}

	Ok(results)
}

/// Evaluates the interpolating polynomial at a specific time.
fn evaluate_polynomial(points: &[Point], target_time: DateTime<Utc>, degree: usize, resolution: Resolution) -> Result<BigDecimal> {
	// Select `degree + 1` points nearest to the target_time for local interpolation.
	let selected_points = select_nearest_points(points, target_time, degree + 1);

	// Convert timestamps to a numerical format for calculation.
	let get_time_val = |t: DateTime<Utc>| -> Result<BigDecimal> {
		let val = match resolution {
			Resolution::Nanoseconds => {
				match t.timestamp_nanos_opt() {
					Some(nanos) => nanos as i64,
					None => {
						// Handle overflow or invalid timestamp
						bail!(Error::InvalidTimestampError(format!("Timestamp {} is out of range for nanoseconds", t)))
					}
				}
			}
			Resolution::Microseconds => t.timestamp_micros(),
			Resolution::Milliseconds => t.timestamp_millis(),
			Resolution::Seconds => t.timestamp(),
			Resolution::Minutes => t.timestamp() / SECONDS_IN_MINUTE,
			Resolution::Hours => t.timestamp() / SECONDS_IN_HOUR,
			Resolution::Days => t.timestamp() / SECONDS_IN_DAY,
			Resolution::Weeks => t.timestamp() / SECONDS_IN_WEEK,
			Resolution::Months => t.timestamp() / SECONDS_IN_MONTH,
			Resolution::Years => t.timestamp() / SECONDS_IN_YEAR,
		};
		match BigDecimal::from_i64(val) {
			Some(decimal) => Ok(decimal),
			None => bail!(Error::DecimalConversionError),
		}
	};

	let target_x = get_time_val(target_time)?;
	let mut total = BigDecimal::zero();

	// Lagrange Interpolation Formula:
	// P(x) = Σ [y_j * L_j(x)]
	// L_j(x) = Π [(x - x_m) / (x_j - x_m)] for m != j
	for j in 0..selected_points.len() {
		let p_j = &selected_points[j];
		let y_j = &p_j.value;
		let x_j = get_time_val(p_j.timestamp)?;

		let mut lagrange_basis = BigDecimal::one();
		for m in 0..selected_points.len() {
			if m == j {
				continue;
			}
			let p_m = &selected_points[m];
			let x_m = get_time_val(p_m.timestamp)?;

			let denominator = &x_j - &x_m;
			if denominator.is_zero() {
				// This case implies duplicate timestamps in the input data.
				// If target_x matches, the value is y_j; otherwise, this term is unstable.
				// A robust implementation might average values or handle this as an error.
				// For simplicity, we treat it as if the points are distinct.
				// If target_x is one of the node, the result should be that node's value.
				if target_x == x_j {
					return Ok(y_j.clone());
				}
				// If denominator is zero but x_j != target_x, we skip this term to avoid division by zero,
				// though it indicates a problematic input set.
				continue;
			}

			let numerator = &target_x - &x_m;
			lagrange_basis *= numerator / denominator;
		}
		total += y_j * lagrange_basis;
	}

	Ok(total)
}

/// Selects a slice of `n` points from a sorted list that are nearest to a target time.
fn select_nearest_points(points: &[Point], target_time: DateTime<Utc>, n: usize) -> &[Point] {
	if points.len() <= n {
		return points;
	}

	// Find the index of the last point with a timestamp <= target_time.
	let search_result = points.binary_search_by_key(&target_time, |p| p.timestamp);
	let center_idx = match search_result {
		Ok(i) => i,
		Err(i) => i.saturating_sub(1),
	};

	// Determine the start index of the slice, centering it around `center_idx`.
	let mut start_idx = center_idx.saturating_sub(n / 2);

	// Adjust slice if it goes out of bounds.
	if start_idx + n > points.len() {
		start_idx = points.len() - n;
	}

	&points[start_idx..start_idx + n]
}


/// SIMD-optimized polynomial interpolation for a given degree.
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points for the given degree (< degree + 1)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn polynomial_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution, degree: usize) -> Result<Vec<Point>> {
    if points.len() < degree + 1 {
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
        let result_simd = simd_polynomial_interpolate(&input_times, &input_values, target_simd, degree);

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

/// SIMD polynomial interpolation core function for a given degree.
fn simd_polynomial_interpolate(input_times: &[f64], input_values: &[f64], target_times: f64x4, degree: usize) -> f64x4 {
    // Assumption: all target times in the vector fall into the same segment.
    // Use the first lane to determine the segment.
    let target_time_scalar = target_times.to_array()[0];
    let n = input_times.len();
    let num_points = degree + 1;

    // Find the segment for the target time.
    let i = match input_times.binary_search_by(|t| t.partial_cmp(&target_time_scalar).unwrap()) {
        Ok(i) => i,
        Err(i) => i,
    };

    // Determine the start index for the 'num_points' window.
    // Try to center the window around the target time's interval.
    let mut start_index = i.saturating_sub(num_points / 2);

    // Adjust window to be within bounds.
    if start_index + num_points > n {
        start_index = n - num_points;
    }

    let segment_indices: Vec<usize> = (start_index..start_index + num_points).collect();

    // Load segment points into SIMD vectors
    let segment_times: Vec<f64x4> = segment_indices.iter().map(|&idx| f64x4::splat(input_times[idx])).collect();
    let segment_values: Vec<f64x4> = segment_indices.iter().map(|&idx| f64x4::splat(input_values[idx])).collect();

    let mut total = f64x4::splat(0.0);
    let mut fallback_mask = f64x4::splat(0.0).cmp_lt(f64x4::splat(0.0)); // All false mask

    // Perform Lagrange interpolation using SIMD operations
    for i in 0..num_points {
        let mut numerator = f64x4::splat(1.0);
        let mut denominator = f64x4::splat(1.0);

        for j in 0..num_points {
            if i == j {
                continue;
            }
            numerator *= target_times - segment_times[j];
            denominator *= segment_times[i] - segment_times[j];
        }

        fallback_mask |= denominator.abs().cmp_lt(f64x4::splat(1e-10));
        let basis_polynomial = numerator / denominator;
        total += segment_values[i] * basis_polynomial;
    }

    // Use a simple average of the two center points of the window as a fallback
    let center_idx1 = num_points / 2;
    let center_idx2 = (num_points - 1) / 2;
    let fallback_value = (segment_values[center_idx1] + segment_values[center_idx2]) * f64x4::splat(0.5);

    fallback_mask.blend(fallback_value, total)
}
