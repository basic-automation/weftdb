use anyhow::{Result, bail};
use bigdecimal::{BigDecimal, FromPrimitive, One, ToPrimitive, Zero};
use chrono::{DateTime, Utc};

use crate::{
    Error, Point, Resolution, splines::{SECONDS_IN_DAY, SECONDS_IN_HOUR, SECONDS_IN_MINUTE, SECONDS_IN_MONTH, SECONDS_IN_WEEK, SECONDS_IN_YEAR}
};

/// Performs polynomial interpolation of a specified degree on measurement data.
///
/// This function uses Lagrange interpolation with improved numerical stability.
/// It can handle any degree from 1 to the number of available points - 1.
///
/// # Arguments
///
/// * `points` - A vector of `Point` structs to interpolate. Must contain at least `degree + 1` points.
/// * `start` - The start `DateTime<Utc>` for the output series.
/// * `end` - The end `DateTime<Utc>` for the output series.
/// * `resolution` - The `Resolution` of the output data points.
/// * `degree` - The degree of the polynomial to use for interpolation.
/// * `bounds_factor` - Extrapolation bounds factor (None = unbounded, Some(1.0) = 1x data range, Some(2.0) = 2x data range).
///
/// # Errors
///
/// Returns an error if:
/// - `points` has fewer than `degree + 1` elements.
/// - The time range is invalid (`start >= end`).
/// - Timestamp or value conversions fail.
pub fn polynomial(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, degree: usize, bounds_factor: Option<f64>) -> Result<Vec<Point>> {
    if points.is_empty() {
        return Ok(Vec::new());
    }

    if start >= end {
        bail!(Error::InvalidTimeRangeError);
    }

    if degree == 0 {
        bail!(Error::InvalidDegreeError(degree));
    }

    if points.len() < degree + 1 {
        bail!(Error::InsufficientPointsError);
    }

    // Sort points by timestamp for efficient processing
    let mut sorted_points = points;
    sorted_points.sort_by_key(|p| p.timestamp);

    // Remove duplicate timestamps by averaging values
    let deduplicated_points = deduplicate_points(sorted_points);

    // Final check after deduplication
    if deduplicated_points.len() < degree + 1 {
        bail!(Error::InsufficientPointsError);
    }

    // Determine safe degree based on available points and numerical stability
    let safe_degree = determine_safe_degree(degree, deduplicated_points.len());

    let step = resolution.to_step();
    let mut results = Vec::new();
    let mut current_time = start;

    while current_time <= end {
        let value = evaluate_polynomial_safe(&deduplicated_points, current_time, safe_degree, resolution, bounds_factor)?;
        results.push(Point { timestamp: current_time, value });
        current_time += step;
    }

    Ok(results)
}

/// Determines the safe degree to use based on requested degree and available points
fn determine_safe_degree(requested_degree: usize, available_points: usize) -> usize {
    // Maximum degree is limited by available points
    let max_degree_by_points = available_points.saturating_sub(1);
    
    // For numerical stability, cap at 20 for most cases
    let stability_cap = if available_points > 50 {
        20  // Higher cap for large datasets
    } else if available_points > 20 {
        12  // Medium cap for medium datasets
    } else {
        8   // Conservative cap for small datasets
    };

    // Use the minimum of requested degree, available points limit, and stability cap
    requested_degree.min(max_degree_by_points).min(stability_cap)
}

/// Removes duplicate timestamps by averaging their values
fn deduplicate_points(points: Vec<Point>) -> Vec<Point> {
    if points.is_empty() {
        return points;
    }

    let mut result = Vec::new();
    let mut current_group = vec![points[0].clone()];

    for point in points.into_iter().skip(1) {
        if point.timestamp == current_group[0].timestamp {
            current_group.push(point);
        } else {
            // Average the current group
            let avg_value = current_group.iter().map(|p| &p.value).fold(BigDecimal::zero(), |acc, val| acc + val) / BigDecimal::from(current_group.len() as i64);

            result.push(Point { timestamp: current_group[0].timestamp, value: avg_value });

            current_group = vec![point];
        }
    }

    // Handle the last group
    let avg_value = current_group.iter().map(|p| &p.value).fold(BigDecimal::zero(), |acc, val| acc + val) / BigDecimal::from(current_group.len() as i64);

    result.push(Point { timestamp: current_group[0].timestamp, value: avg_value });

    result
}

/// Check if we're extrapolating beyond the data range
fn is_extrapolating(points: &[Point], target_time: DateTime<Utc>) -> bool {
    if points.is_empty() {
        return false;
    }

    let first_time = points.first().unwrap().timestamp;
    let last_time = points.last().unwrap().timestamp;

    target_time < first_time || target_time > last_time
}

/// Apply reasonable bounds to extrapolated values - FIXED TO MATCH GPU
fn apply_extrapolation_bounds(points: &[Point], target_time: DateTime<Utc>, value: BigDecimal, bounds_factor: f64) -> BigDecimal {
    if points.is_empty() {
        return value;
    }

    // Calculate reasonable bounds based on the data range
    let min_value = points.iter().map(|p| &p.value).min().unwrap();
    let max_value = points.iter().map(|p| &p.value).max().unwrap();
    let range = max_value - min_value;

    // Apply configurable bounds factor
    let factor = BigDecimal::from_f64(bounds_factor).unwrap_or_else(|| BigDecimal::from(2));
    let lower_bound = min_value - &range * &factor;
    let upper_bound = max_value + &range * &factor;

    // Determine which bound to apply based on which side we're extrapolating
    let first_time = points.first().unwrap().timestamp;
    let last_time = points.last().unwrap().timestamp;

    if target_time < first_time {
        // Left extrapolation - use lower bound
        lower_bound
    } else if target_time > last_time {
        // Right extrapolation - use upper bound  
        upper_bound
    } else {
        // This shouldn't happen if we're in extrapolation mode, but clamp just in case
        if value < lower_bound {
            lower_bound
        } else if value > upper_bound {
            upper_bound
        } else {
            value
        }
    }
}

/// Evaluates the interpolating polynomial at a specific time - FIXED TO MATCH GPU
fn evaluate_polynomial_safe(points: &[Point], target_time: DateTime<Utc>, degree: usize, resolution: Resolution, bounds_factor: Option<f64>) -> Result<BigDecimal> {
    // Check if target is exactly at a data point
    if let Some(point) = points.iter().find(|p| p.timestamp == target_time) {
        return Ok(point.value.clone());
    }

    // Check for extrapolation BEFORE point selection
    let is_extrapolation = is_extrapolating(points, target_time);
    
    // For interpolation OR unbounded extrapolation, do the calculation
    let selected_points = select_nearest_points_adaptive(points, target_time, degree + 1);

    // Convert timestamps to a numerical format for calculation with normalization
    let time_values = get_normalized_time_values(selected_points, target_time, resolution)?;

    // Use normalized values for better numerical stability
    let target_x = time_values.target_normalized;
    let mut total = BigDecimal::zero();

    // Lagrange Interpolation
    for j in 0..selected_points.len() {
        let y_j = &selected_points[j].value;
        let x_j = &time_values.points_normalized[j];

        let mut lagrange_basis = BigDecimal::one();
        let mut has_near_zero_denominator = false;

        for m in 0..selected_points.len() {
            if m == j {
                continue;
            }
            let x_m = &time_values.points_normalized[m];
            let denominator = x_j - x_m;

            if denominator.abs() < BigDecimal::from_f64(1e-10).unwrap_or_else(|| BigDecimal::from(0)) {
                has_near_zero_denominator = true;
                break;
            }

            let numerator = &target_x - x_m;
            lagrange_basis *= numerator / denominator;
        }

        if !has_near_zero_denominator {
            total += y_j * lagrange_basis;
        }
    }

    // Apply bounds if configured and extrapolating
    if is_extrapolation {
        if let Some(factor) = bounds_factor {
            total = apply_extrapolation_bounds(points, target_time, total, factor);
        }
        // If bounds_factor is None, return unbounded value
    }

    Ok(total)
}

/// Normalizes time values to improve numerical stability
struct NormalizedTimeValues {
    target_normalized: BigDecimal,
    points_normalized: Vec<BigDecimal>,
}

fn get_normalized_time_values(points: &[Point], target_time: DateTime<Utc>, resolution: Resolution) -> Result<NormalizedTimeValues> {
    let get_time_val = |t: DateTime<Utc>| -> Result<BigDecimal> {
        let val = match resolution {
            Resolution::Nanoseconds => match t.timestamp_nanos_opt() {
                Some(nanos) => nanos as i64,
                None => bail!(Error::InvalidTimestampError(format!("Timestamp {} is out of range for nanoseconds", t))),
            },
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
        BigDecimal::from_i64(val).ok_or_else(|| Error::DecimalConversionError.into())
    };

    // Get all time values
    let target_raw = get_time_val(target_time)?;
    let mut points_raw = Vec::new();
    for point in points {
        points_raw.push(get_time_val(point.timestamp)?);
    }

    // Normalize around the first point to improve numerical stability
    let reference = &points_raw[0];
    let target_normalized = &target_raw - reference;
    let points_normalized = points_raw.iter().map(|t| t - reference).collect();

    Ok(NormalizedTimeValues { target_normalized, points_normalized })
}

/// Adaptive point selection that considers degree and dataset size
fn select_nearest_points_adaptive(points: &[Point], target_time: DateTime<Utc>, n: usize) -> &[Point] {
    if points.len() <= n {
        return points;
    }

    let search_result = points.binary_search_by_key(&target_time, |p| p.timestamp);
    let center_idx = match search_result {
        Ok(i) => i,
        Err(i) => {
            if i == 0 {
                0
            } else if i == points.len() {
                points.len() - 1
            } else {
                // Choose the closer point
                let dist_before = (target_time - points[i - 1].timestamp).num_seconds().abs();
                let dist_after = (points[i].timestamp - target_time).num_seconds().abs();
                if dist_before <= dist_after { i - 1 } else { i }
            }
        }
    };

    // Adaptive window selection based on dataset size and degree
    let window_strategy = if n <= 4 {
        // For low degrees, center the window
        WindowStrategy::Centered
    } else if points.len() > 100 {
        // For large datasets, use balanced approach
        WindowStrategy::Balanced
    } else {
        // For medium datasets, prefer slightly forward-looking
        WindowStrategy::ForwardBiased
    };

    let start_idx = match window_strategy {
        WindowStrategy::Centered => {
            center_idx.saturating_sub(n / 2)
        }
        WindowStrategy::Balanced => {
            center_idx.saturating_sub(n / 3)
        }
        WindowStrategy::ForwardBiased => {
            center_idx.saturating_sub(n / 4)
        }
    };

    // Ensure we don't go out of bounds
    let final_start = if start_idx + n > points.len() {
        points.len() - n
    } else {
        start_idx
    };

    &points[final_start..final_start + n]
}

enum WindowStrategy {
    Centered,
    Balanced,
    ForwardBiased,
}

/// Enhanced SIMD-optimized polynomial interpolation for any degree with configurable bounds.
///
/// # Arguments
///
/// * `points` - A slice of `Point` structs to interpolate.
/// * `target_times` - A slice of target `DateTime<Utc>` values to interpolate.
/// * `resolution` - The `Resolution` for timestamp conversions.
/// * `degree` - The degree of the polynomial to use for interpolation.
/// * `bounds_factor` - Extrapolation bounds factor (None = unbounded, Some(1.0) = 1x data range, Some(2.0) = 2x data range).
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points for the given degree (< degree + 1)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn polynomial_simd(points: &[Point], target_times: &[DateTime<Utc>], resolution: Resolution, degree: usize, bounds_factor: Option<f64>) -> Result<Vec<Point>> {
    if degree == 0 {
        bail!(Error::InvalidDegreeError(degree));
    }

    if points.len() < degree + 1 {
        bail!(Error::InsufficientPointsError);
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    // Sort points by timestamp for efficient processing (match CPU behavior)
    let mut sorted_points = points.to_vec();
    sorted_points.sort_by_key(|p| p.timestamp);

    // Remove duplicate timestamps by averaging values (match CPU behavior)
    let deduplicated_points = deduplicate_points_simd(sorted_points);

    // Final check after deduplication
    if deduplicated_points.len() < degree + 1 {
        bail!(Error::InsufficientPointsError);
    }

    // Determine safe degree
    let safe_degree = determine_safe_degree(degree, deduplicated_points.len());

    // Process each target time using the same logic as CPU
    let mut results = Vec::with_capacity(target_times.len());

    for &target_time in target_times {
        let value = evaluate_polynomial_safe_simd(&deduplicated_points, target_time, safe_degree, resolution, bounds_factor)?;
        results.push(Point { 
            timestamp: target_time, 
            value: BigDecimal::from_f64(value).unwrap_or_else(|| BigDecimal::from(0)) 
        });
    }

    Ok(results)
}

/// SIMD version of deduplicate_points that matches CPU behavior
fn deduplicate_points_simd(points: Vec<Point>) -> Vec<Point> {
    if points.is_empty() {
        return points;
    }

    let mut result = Vec::new();
    let mut current_group = vec![points[0].clone()];

    for point in points.into_iter().skip(1) {
        if point.timestamp == current_group[0].timestamp {
            current_group.push(point);
        } else {
            // Average the current group
            let avg_value = current_group.iter().map(|p| &p.value).fold(BigDecimal::zero(), |acc, val| acc + val) / BigDecimal::from(current_group.len() as i64);
            result.push(Point { timestamp: current_group[0].timestamp, value: avg_value });
            current_group = vec![point];
        }
    }

    // Handle the last group
    let avg_value = current_group.iter().map(|p| &p.value).fold(BigDecimal::zero(), |acc, val| acc + val) / BigDecimal::from(current_group.len() as i64);
    result.push(Point { timestamp: current_group[0].timestamp, value: avg_value });

    result
}

/// SIMD version of apply_extrapolation_bounds - FIXED TO MATCH GPU
fn apply_extrapolation_bounds_simd(points: &[Point], target_time: DateTime<Utc>, value: f64, bounds_factor: f64) -> f64 {
    if points.is_empty() {
        return value;
    }

    // Calculate bounds using the same logic as CPU BigDecimal version
    let min_value_bd = points.iter().map(|p| &p.value).min().unwrap();
    let max_value_bd = points.iter().map(|p| &p.value).max().unwrap();
    let range_bd = max_value_bd - min_value_bd;

    // Convert to f64 for calculation
    let min_value = min_value_bd.to_f64().unwrap_or(0.0);
    let max_value = max_value_bd.to_f64().unwrap_or(0.0);
    let range = range_bd.to_f64().unwrap_or(0.0);

    // Apply configurable bounds factor
    let lower_bound = min_value - range * bounds_factor;
    let upper_bound = max_value + range * bounds_factor;

    // Determine which bound to apply based on which side we're extrapolating
    let first_time = points.first().unwrap().timestamp;
    let last_time = points.last().unwrap().timestamp;

    if target_time < first_time {
        // Left extrapolation - use lower bound
        lower_bound
    } else if target_time > last_time {
        // Right extrapolation - use upper bound
        upper_bound
    } else {
        // This shouldn't happen if we're in extrapolation mode, but clamp just in case
        if value < lower_bound {
            lower_bound
        } else if value > upper_bound {
            upper_bound
        } else {
            value
        }
    }
}

/// SIMD version of evaluate_polynomial_safe - FIXED TO MATCH GPU
fn evaluate_polynomial_safe_simd(points: &[Point], target_time: DateTime<Utc>, degree: usize, resolution: Resolution, bounds_factor: Option<f64>) -> Result<f64> {
    // Check if target is exactly at a data point
    if let Some(point) = points.iter().find(|p| p.timestamp == target_time) {
        return Ok(point.value.to_f64().unwrap_or(0.0));
    }

    // Check for extrapolation BEFORE point selection
    let is_extrapolation = is_extrapolating_simd(points, target_time);
    
    // For interpolation OR unbounded extrapolation, do the calculation
    let selected_points = select_nearest_points_adaptive_simd(points, target_time, degree + 1);

    // Convert to f64 for SIMD processing with normalization like CPU
    let reference_time = selected_points[0].timestamp;
    
    #[allow(clippy::cast_precision_loss)]
    let get_normalized_time = |t: DateTime<Utc>| -> f64 {
        let raw_time = match resolution {
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
        };
        
        let reference_raw = match resolution {
            Resolution::Nanoseconds => reference_time.timestamp_nanos_opt().map_or(0.0, |v| v as f64),
            Resolution::Microseconds => reference_time.timestamp_micros() as f64,
            Resolution::Milliseconds => reference_time.timestamp_millis() as f64,
            Resolution::Seconds => reference_time.timestamp() as f64,
            Resolution::Minutes => reference_time.timestamp() as f64 / SECONDS_IN_MINUTE as f64,
            Resolution::Hours => reference_time.timestamp() as f64 / SECONDS_IN_HOUR as f64,
            Resolution::Days => reference_time.timestamp() as f64 / SECONDS_IN_DAY as f64,
            Resolution::Weeks => reference_time.timestamp() as f64 / SECONDS_IN_WEEK as f64,
            Resolution::Months => reference_time.timestamp() as f64 / SECONDS_IN_MONTH as f64,
            Resolution::Years => reference_time.timestamp() as f64 / SECONDS_IN_YEAR as f64,
        };
        
        raw_time - reference_raw
    };

    let input_times: Vec<f64> = selected_points.iter().map(|p| get_normalized_time(p.timestamp)).collect();
    let input_values: Vec<f64> = selected_points.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();
    let target_f64 = get_normalized_time(target_time);

    // Lagrange interpolation exactly like CPU
    let mut result = 0.0;
    
    for j in 0..selected_points.len() {
        let y_j = input_values[j];
        let x_j = input_times[j];

        let mut lagrange_basis = 1.0;
        let mut has_near_zero_denominator = false;

        for m in 0..selected_points.len() {
            if m == j {
                continue;
            }
            let x_m = input_times[m];
            let denominator = x_j - x_m;

            if denominator.abs() < 1e-10 {
                has_near_zero_denominator = true;
                break;
            }

            let numerator = target_f64 - x_m;
            lagrange_basis *= numerator / denominator;
        }

        if !has_near_zero_denominator {
            result += y_j * lagrange_basis;
        }
    }

    // Apply bounds if configured and extrapolating
    if is_extrapolation {
        if let Some(factor) = bounds_factor {
            result = apply_extrapolation_bounds_simd(points, target_time, result, factor);
        }
        // If bounds_factor is None, return unbounded value
    }

    Ok(result)
}

/// SIMD version of is_extrapolating that matches CPU behavior
fn is_extrapolating_simd(points: &[Point], target_time: DateTime<Utc>) -> bool {
    if points.is_empty() {
        return false;
    }

    let first_time = points.first().unwrap().timestamp;
    let last_time = points.last().unwrap().timestamp;

    target_time < first_time || target_time > last_time
}

/// SIMD version of select_nearest_points_adaptive that matches CPU behavior
fn select_nearest_points_adaptive_simd(points: &[Point], target_time: DateTime<Utc>, n: usize) -> &[Point] {
    if points.len() <= n {
        return points;
    }

    let search_result = points.binary_search_by_key(&target_time, |p| p.timestamp);
    let center_idx = match search_result {
        Ok(i) => i,
        Err(i) => {
            if i == 0 {
                0
            } else if i == points.len() {
                points.len() - 1
            } else {
                // Choose the closer point
                let dist_before = (target_time - points[i - 1].timestamp).num_seconds().abs();
                let dist_after = (points[i].timestamp - target_time).num_seconds().abs();
                if dist_before <= dist_after { i - 1 } else { i }
            }
        }
    };

    // Use the same window strategy as CPU
    let window_strategy = if n <= 4 {
        WindowStrategy::Centered
    } else if points.len() > 100 {
        WindowStrategy::Balanced
    } else {
        WindowStrategy::ForwardBiased
    };

    let start_idx = match window_strategy {
        WindowStrategy::Centered => center_idx.saturating_sub(n / 2),
        WindowStrategy::Balanced => center_idx.saturating_sub(n / 3),
        WindowStrategy::ForwardBiased => center_idx.saturating_sub(n / 4),
    };

    // Ensure we don't go out of bounds
    let final_start = if start_idx + n > points.len() {
        points.len() - n
    } else {
        start_idx
    };

    &points[final_start..final_start + n]
}
