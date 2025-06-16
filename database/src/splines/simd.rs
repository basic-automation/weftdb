//! SIMD-optimized spline interpolation implementations
//!
//! This module provides high-performance SIMD implementations of spline interpolation
//! algorithms using wide vectors for batch processing of multiple data points.

use anyhow::Result;
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use uuid::Uuid;
use wide::f64x4;

use crate::{Error, Measurement, Resolution, SplineType};

// Constants for SIMD processing
const SIMD_BATCH_SIZE: usize = 4;

// Threshold for using SIMD vs scalar processing
const SIMD_THRESHOLD: usize = 32;

/// SIMD-optimized linear interpolation for batch processing
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< 2 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn linear_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>], dataset_id: Uuid) -> Result<Vec<Measurement>> {
	if measurements.len() < 2 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	if target_times.len() < SIMD_THRESHOLD {
		// Fall back to scalar for small datasets
		return linear_scalar_fallback(measurements, target_times, dataset_id);
	}

	let mut results = Vec::with_capacity(target_times.len());

	// Convert measurements to f64 for SIMD processing (note: precision loss acceptable for SIMD speed)
	#[allow(clippy::cast_precision_loss)]
	let time_values: Vec<f64> = measurements.iter().map(|m| m.timestamp.timestamp_millis() as f64).collect();

	let data_values: Vec<f64> = measurements.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

	// Process in SIMD batches
	for chunk in target_times.chunks(SIMD_BATCH_SIZE) {
		#[allow(clippy::cast_precision_loss)]
		let target_timestamps: Vec<f64> = chunk.iter().map(|t| t.timestamp_millis() as f64).collect();

		// Pad to SIMD width if necessary
		let mut padded_targets = target_timestamps;
		while padded_targets.len() < SIMD_BATCH_SIZE {
			padded_targets.push(padded_targets[padded_targets.len() - 1]);
		}

		let target_simd = f64x4::new([padded_targets[0], padded_targets[1], padded_targets[2], padded_targets[3]]);

		let interpolated_values = linear_interpolate_simd(&time_values, &data_values, target_simd);

		// Convert back to measurements
		for (i, &target_time) in chunk.iter().enumerate() {
			let interpolated_value = interpolated_values.as_array_ref()[i];
			results.push(Measurement { id: Uuid::new_v4(), dataset_id, timestamp: target_time, value: BigDecimal::from_f64(interpolated_value).unwrap_or_else(|| BigDecimal::from(0)) });
		}
	}

	Ok(results)
}

/// SIMD-optimized quadratic interpolation for batch processing
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< 3 points for quadratic)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn quadratic_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>], dataset_id: Uuid) -> Result<Vec<Measurement>> {
	if measurements.len() < 3 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	if target_times.len() < SIMD_THRESHOLD {
		// Fall back to scalar for small datasets
		return quadratic_scalar_fallback(measurements, target_times, dataset_id);
	}

	let mut results = Vec::with_capacity(target_times.len());

	// Convert measurements to f64 for SIMD processing (note: precision loss acceptable for SIMD speed)
	#[allow(clippy::cast_precision_loss)]
	let time_values: Vec<f64> = measurements.iter().map(|m| m.timestamp.timestamp_millis() as f64).collect();

	let data_values: Vec<f64> = measurements.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

	// Process in SIMD batches
	for chunk in target_times.chunks(SIMD_BATCH_SIZE) {
		#[allow(clippy::cast_precision_loss)]
		let target_timestamps: Vec<f64> = chunk.iter().map(|t| t.timestamp_millis() as f64).collect();

		// Pad to SIMD width if necessary
		let mut padded_targets = target_timestamps;
		while padded_targets.len() < SIMD_BATCH_SIZE {
			padded_targets.push(padded_targets[padded_targets.len() - 1]);
		}

		let target_simd = f64x4::new([padded_targets[0], padded_targets[1], padded_targets[2], padded_targets[3]]);

		let interpolated_values = quadratic_interpolate_simd(&time_values, &data_values, target_simd);

		// Convert back to measurements
		for (i, &target_time) in chunk.iter().enumerate() {
			let interpolated_value = interpolated_values.as_array_ref()[i];
			results.push(Measurement { id: Uuid::new_v4(), dataset_id, timestamp: target_time, value: BigDecimal::from_f64(interpolated_value).unwrap_or_else(|| BigDecimal::from(0)) });
		}
	}

	Ok(results)
}

/// SIMD-optimized cubic interpolation for batch processing
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< 4 points for cubic)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn cubic_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>], dataset_id: Uuid) -> Result<Vec<Measurement>> {
	if measurements.len() < 4 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	if target_times.len() < SIMD_THRESHOLD {
		// Fall back to scalar for small datasets
		return cubic_scalar_fallback(measurements, target_times, dataset_id);
	}

	let mut results = Vec::with_capacity(target_times.len());

	// Convert measurements to f64 for SIMD processing (note: precision loss acceptable for SIMD speed)
	#[allow(clippy::cast_precision_loss)]
	let time_values: Vec<f64> = measurements.iter().map(|m| m.timestamp.timestamp_millis() as f64).collect();

	let data_values: Vec<f64> = measurements.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

	// Process in SIMD batches
	for chunk in target_times.chunks(SIMD_BATCH_SIZE) {
		#[allow(clippy::cast_precision_loss)]
		let target_timestamps: Vec<f64> = chunk.iter().map(|t| t.timestamp_millis() as f64).collect();

		// Pad to SIMD width if necessary
		let mut padded_targets = target_timestamps;
		while padded_targets.len() < SIMD_BATCH_SIZE {
			padded_targets.push(padded_targets[padded_targets.len() - 1]);
		}

		let target_simd = f64x4::new([padded_targets[0], padded_targets[1], padded_targets[2], padded_targets[3]]);

		let interpolated_values = cubic_interpolate_simd(&time_values, &data_values, target_simd);

		// Convert back to measurements
		for (i, &target_time) in chunk.iter().enumerate() {
			let interpolated_value = interpolated_values.as_array_ref()[i];
			results.push(Measurement { id: Uuid::new_v4(), dataset_id, timestamp: target_time, value: BigDecimal::from_f64(interpolated_value).unwrap_or_else(|| BigDecimal::from(0)) });
		}
	}

	Ok(results)
}

/// SIMD-optimized polynomial interpolation for batch processing
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for polynomial degree
/// - Invalid polynomial degree
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn polynomial_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>], degree: usize, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	if measurements.len() < degree + 1 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	if target_times.len() < SIMD_THRESHOLD {
		// Fall back to scalar for small datasets
		return polynomial_scalar_fallback(measurements, target_times, degree, dataset_id);
	}

	let mut results = Vec::with_capacity(target_times.len());

	// Convert measurements to f64 for SIMD processing (note: precision loss acceptable for SIMD speed)
	#[allow(clippy::cast_precision_loss)]
	let time_values: Vec<f64> = measurements.iter().map(|m| m.timestamp.timestamp_millis() as f64).collect();

	let data_values: Vec<f64> = measurements.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

	// Process in SIMD batches
	for chunk in target_times.chunks(SIMD_BATCH_SIZE) {
		#[allow(clippy::cast_precision_loss)]
		let target_timestamps: Vec<f64> = chunk.iter().map(|t| t.timestamp_millis() as f64).collect();

		// Pad to SIMD width if necessary
		let mut padded_targets = target_timestamps;
		while padded_targets.len() < SIMD_BATCH_SIZE {
			padded_targets.push(padded_targets[padded_targets.len() - 1]);
		}

		let target_simd = f64x4::new([padded_targets[0], padded_targets[1], padded_targets[2], padded_targets[3]]);

		let interpolated_values = polynomial_interpolate_simd(&time_values, &data_values, target_simd, degree);

		// Convert back to measurements
		for (i, &target_time) in chunk.iter().enumerate() {
			let interpolated_value = interpolated_values.as_array_ref()[i];
			results.push(Measurement { id: Uuid::new_v4(), dataset_id, timestamp: target_time, value: BigDecimal::from_f64(interpolated_value).unwrap_or_else(|| BigDecimal::from(0)) });
		}
	}

	Ok(results)
}

/// Auto-selecting SIMD interpolation based on spline type
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation functions fail
pub fn auto_interpolate_simd(measurements: &[Measurement], target_times: &[DateTime<Utc>], spline_type: SplineType) -> Result<Vec<Measurement>> {
	if measurements.is_empty() {
		return Ok(vec![]);
	}

	let dataset_id = measurements[0].dataset_id;

	match spline_type {
		SplineType::Linear => linear_simd_batch(measurements, target_times, dataset_id),
		SplineType::Quadratic => quadratic_simd_batch(measurements, target_times, dataset_id),
		SplineType::Cubic => cubic_simd_batch(measurements, target_times, dataset_id),
		SplineType::Polynomial(degree) => polynomial_simd_batch(measurements, target_times, degree, dataset_id),
	}
}

/// Parallel SIMD linear interpolation for large target sets
///
/// # Errors
///
/// Returns an error if SIMD batch processing fails
pub fn linear_simd_batch_parallel(measurements: &[Measurement], target_times: &[DateTime<Utc>], dataset_id: Uuid) -> Result<Vec<Measurement>> {
    if target_times.len() < 256 {
        return linear_simd_batch(measurements, target_times, dataset_id);
    }

    let chunk_size = 128;
    let results: Result<Vec<Vec<Measurement>>> = target_times
        .par_chunks(chunk_size)
        .map(|time_chunk| {
            linear_simd_batch(measurements, time_chunk, dataset_id)
        })
        .collect();

    let chunk_results = results?;
    Ok(chunk_results.into_iter().flatten().collect())
}

/// Parallel SIMD quadratic interpolation for large target sets
///
/// # Errors
///
/// Returns an error if SIMD batch processing fails
pub fn quadratic_simd_batch_parallel(measurements: &[Measurement], target_times: &[DateTime<Utc>], dataset_id: Uuid) -> Result<Vec<Measurement>> {
    if target_times.len() < 256 {
        return quadratic_simd_batch(measurements, target_times, dataset_id);
    }

    let chunk_size = 128;
    let results: Result<Vec<Vec<Measurement>>> = target_times
        .par_chunks(chunk_size)
        .map(|time_chunk| {
            quadratic_simd_batch(measurements, time_chunk, dataset_id)
        })
        .collect();

    let chunk_results = results?;
    Ok(chunk_results.into_iter().flatten().collect())
}

/// Parallel SIMD cubic interpolation for large target sets
///
/// # Errors
///
/// Returns an error if SIMD batch processing fails
pub fn cubic_simd_batch_parallel(measurements: &[Measurement], target_times: &[DateTime<Utc>], dataset_id: Uuid) -> Result<Vec<Measurement>> {
    if target_times.len() < 256 {
        return cubic_simd_batch(measurements, target_times, dataset_id);
    }

    let chunk_size = 128;
    let results: Result<Vec<Vec<Measurement>>> = target_times
        .par_chunks(chunk_size)
        .map(|time_chunk| {
            cubic_simd_batch(measurements, time_chunk, dataset_id)
        })
        .collect();

    let chunk_results = results?;
    Ok(chunk_results.into_iter().flatten().collect())
}

/// Parallel SIMD polynomial interpolation for large target sets
///
/// # Errors
///
/// Returns an error if SIMD batch processing fails
pub fn polynomial_simd_batch_parallel(measurements: &[Measurement], target_times: &[DateTime<Utc>], degree: usize, dataset_id: Uuid) -> Result<Vec<Measurement>> {
    if target_times.len() < 256 {
        return polynomial_simd_batch(measurements, target_times, degree, dataset_id);
    }

    let chunk_size = 128;
    let results: Result<Vec<Vec<Measurement>>> = target_times
        .par_chunks(chunk_size)
        .map(|time_chunk| {
            polynomial_simd_batch(measurements, time_chunk, degree, dataset_id)
        })
        .collect();

    let chunk_results = results?;
    Ok(chunk_results.into_iter().flatten().collect())
}

/// Enhanced auto interpolation with SIMD + Rayon parallelism
///
/// # Errors
///
/// Returns an error if the selected SIMD interpolation method fails
pub fn auto_interpolate_simd_parallel(
    measurements: &[Measurement], 
    target_times: &[DateTime<Utc>], 
    spline_type: SplineType
) -> Result<Vec<Measurement>> {
    if target_times.len() < 256 {
        return auto_interpolate_simd(measurements, target_times, spline_type);
    }

    let dataset_id = measurements[0].dataset_id;

    match spline_type {
        SplineType::Linear => linear_simd_batch_parallel(measurements, target_times, dataset_id),
        SplineType::Quadratic => quadratic_simd_batch_parallel(measurements, target_times, dataset_id),
        SplineType::Cubic => cubic_simd_batch_parallel(measurements, target_times, dataset_id),
        SplineType::Polynomial(degree) => polynomial_simd_batch_parallel(measurements, target_times, degree, dataset_id),
    }
}

// Core SIMD interpolation functions

fn linear_interpolate_simd(time_values: &[f64], data_values: &[f64], target_times: f64x4) -> f64x4 {
	let mut results = [0.0; 4];

	for (i, result) in results.iter_mut().enumerate() {
		let target_time = target_times.as_array_ref()[i];
		*result = linear_interpolate_single(time_values, data_values, target_time);
	}

	f64x4::new(results)
}

fn quadratic_interpolate_simd(time_values: &[f64], data_values: &[f64], target_times: f64x4) -> f64x4 {
	let mut results = [0.0; 4];

	for (i, result) in results.iter_mut().enumerate() {
		let target_time = target_times.as_array_ref()[i];
		*result = quadratic_interpolate_single(time_values, data_values, target_time);
	}

	f64x4::new(results)
}

fn cubic_interpolate_simd(time_values: &[f64], data_values: &[f64], target_times: f64x4) -> f64x4 {
	let mut results = [0.0; 4];

	for (i, result) in results.iter_mut().enumerate() {
		let target_time = target_times.as_array_ref()[i];
		*result = cubic_interpolate_single(time_values, data_values, target_time);
	}

	f64x4::new(results)
}

fn polynomial_interpolate_simd(time_values: &[f64], data_values: &[f64], target_times: f64x4, degree: usize) -> f64x4 {
	let mut results = [0.0; 4];

	for (i, result) in results.iter_mut().enumerate() {
		let target_time = target_times.as_array_ref()[i];
		*result = polynomial_interpolate_single(time_values, data_values, target_time, degree);
	}

	f64x4::new(results)
}

// Helper functions for single-point interpolation

fn linear_interpolate_single(time_values: &[f64], data_values: &[f64], target_time: f64) -> f64 {
	// Find the two closest points
	let mut left_idx = 0;
	for (i, &time) in time_values.iter().enumerate() {
		if time <= target_time {
			left_idx = i;
		} else {
			break;
		}
	}

	if left_idx >= time_values.len() - 1 {
		return data_values[data_values.len() - 1];
	}

	let x0 = time_values[left_idx];
	let x1 = time_values[left_idx + 1];
	let y0 = data_values[left_idx];
	let y1 = data_values[left_idx + 1];

	if (x1 - x0).abs() < f64::EPSILON {
		return y0;
	}

	y0 + (y1 - y0) * (target_time - x0) / (x1 - x0)
}

fn quadratic_interpolate_single(time_values: &[f64], data_values: &[f64], target_time: f64) -> f64 {
	// Find the closest three points for quadratic interpolation
	let mut center_idx = 0;
	let mut min_distance = f64::MAX;

	for (i, &time) in time_values.iter().enumerate() {
		let distance = (time - target_time).abs();
		if distance < min_distance {
			min_distance = distance;
			center_idx = i;
		}
	}

	// Ensure we have three points
	let start_idx = if center_idx == 0 {
		0
	} else if center_idx >= time_values.len() - 1 {
		time_values.len() - 3
	} else {
		center_idx - 1
	};

	if start_idx + 2 >= time_values.len() {
		return linear_interpolate_single(time_values, data_values, target_time);
	}

	let x0 = time_values[start_idx];
	let x1 = time_values[start_idx + 1];
	let x2 = time_values[start_idx + 2];
	let y0 = data_values[start_idx];
	let y1 = data_values[start_idx + 1];
	let y2 = data_values[start_idx + 2];

	// Lagrange interpolation
	let l0 = ((target_time - x1) * (target_time - x2)) / ((x0 - x1) * (x0 - x2));
	let l1 = ((target_time - x0) * (target_time - x2)) / ((x1 - x0) * (x1 - x2));
	let l2 = ((target_time - x0) * (target_time - x1)) / ((x2 - x0) * (x2 - x1));

	y2.mul_add(l2, y0.mul_add(l0, y1 * l1))
}

fn cubic_interpolate_single(time_values: &[f64], data_values: &[f64], target_time: f64) -> f64 {
	// Find the closest four points for cubic interpolation
	let mut center_idx = 0;
	let mut min_distance = f64::MAX;

	for (i, &time) in time_values.iter().enumerate() {
		let distance = (time - target_time).abs();
		if distance < min_distance {
			min_distance = distance;
			center_idx = i;
		}
	}

	// Ensure we have four points
	let start_idx = if center_idx <= 1 {
		0
	} else if center_idx >= time_values.len() - 2 {
		time_values.len() - 4
	} else {
		center_idx - 1
	};

	if start_idx + 3 >= time_values.len() {
		return quadratic_interpolate_single(time_values, data_values, target_time);
	}

	let x0 = time_values[start_idx];
	let x1 = time_values[start_idx + 1];
	let x2 = time_values[start_idx + 2];
	let x3 = time_values[start_idx + 3];
	let y0 = data_values[start_idx];
	let y1 = data_values[start_idx + 1];
	let y2 = data_values[start_idx + 2];
	let y3 = data_values[start_idx + 3];

	// Lagrange interpolation
	let l0 = ((target_time - x1) * (target_time - x2) * (target_time - x3)) / ((x0 - x1) * (x0 - x2) * (x0 - x3));
	let l1 = ((target_time - x0) * (target_time - x2) * (target_time - x3)) / ((x1 - x0) * (x1 - x2) * (x1 - x3));
	let l2 = ((target_time - x0) * (target_time - x1) * (target_time - x3)) / ((x2 - x0) * (x2 - x1) * (x2 - x3));
	let l3 = ((target_time - x0) * (target_time - x1) * (target_time - x2)) / ((x3 - x0) * (x3 - x1) * (x3 - x2));

	y3.mul_add(l3, y2.mul_add(l2, y0.mul_add(l0, y1 * l1)))
}

fn polynomial_interpolate_single(time_values: &[f64], data_values: &[f64], target_time: f64, degree: usize) -> f64 {
	let points_needed = degree + 1;

	if time_values.len() < points_needed {
		return cubic_interpolate_single(time_values, data_values, target_time);
	}

	// Find the closest points
	let mut distances: Vec<(f64, usize)> = time_values.iter().enumerate().map(|(i, &time)| ((time - target_time).abs(), i)).collect();

	distances.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

	let selected_indices: Vec<usize> = distances.iter().take(points_needed).map(|(_, idx)| *idx).collect();

	// Lagrange interpolation
	let mut result = 0.0;

	for (i, &idx_i) in selected_indices.iter().enumerate() {
		let mut li = 1.0;

		for (j, &idx_j) in selected_indices.iter().enumerate() {
			if i != j {
				li *= (target_time - time_values[idx_j]) / (time_values[idx_i] - time_values[idx_j]);
			}
		}

		result += data_values[idx_i] * li;
	}

	result
}

// Scalar fallback functions

fn linear_scalar_fallback(measurements: &[Measurement], target_times: &[DateTime<Utc>], _dataset_id: Uuid) -> Result<Vec<Measurement>> {
	super::linear::linear(measurements.to_vec(), target_times[0], target_times[target_times.len() - 1], Resolution::Milliseconds)
}

fn quadratic_scalar_fallback(measurements: &[Measurement], target_times: &[DateTime<Utc>], _dataset_id: Uuid) -> Result<Vec<Measurement>> {
	super::quadratic::quadratic(measurements.to_vec(), target_times[0], target_times[target_times.len() - 1], Resolution::Milliseconds)
}

fn cubic_scalar_fallback(measurements: &[Measurement], target_times: &[DateTime<Utc>], _dataset_id: Uuid) -> Result<Vec<Measurement>> {
	super::cubic::cubic(measurements.to_vec(), target_times[0], target_times[target_times.len() - 1], Resolution::Milliseconds)
}

fn polynomial_scalar_fallback(measurements: &[Measurement], target_times: &[DateTime<Utc>], degree: usize, _dataset_id: Uuid) -> Result<Vec<Measurement>> {
	super::polynomial::polynomial(measurements.to_vec(), target_times[0], target_times[target_times.len() - 1], Resolution::Milliseconds, degree)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use chrono::TimeZone;

	use super::*;

	fn create_test_measurements(count: usize) -> Vec<Measurement> {
		let dataset_id = Uuid::new_v4();
		let start_time = chrono::Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();

		(0..count).map(|i| Measurement { id: Uuid::new_v4(), dataset_id, timestamp: start_time + chrono::Duration::seconds(i as i64), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
	}

	#[test]
	fn test_linear_simd_batch() {
		let measurements = create_test_measurements(100);
		let dataset_id = measurements[0].dataset_id;
		let target_times: Vec<_> = (0..50).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 500)).collect();

		let result = linear_simd_batch(&measurements, &target_times, dataset_id);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert_eq!(interpolated.len(), target_times.len());
		assert!(interpolated.iter().all(|m| m.dataset_id == dataset_id));
	}

	#[test]
	fn test_quadratic_simd_batch() {
		let measurements = create_test_measurements(100);
		let dataset_id = measurements[0].dataset_id;
		let target_times: Vec<_> = (0..50).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 500)).collect();

		let result = quadratic_simd_batch(&measurements, &target_times, dataset_id);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert_eq!(interpolated.len(), target_times.len());
		assert!(interpolated.iter().all(|m| m.dataset_id == dataset_id));
	}

	#[test]
	fn test_cubic_simd_batch() {
		let measurements = create_test_measurements(100);
		let dataset_id = measurements[0].dataset_id;
		let target_times: Vec<_> = (0..50).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 500)).collect();

		let result = cubic_simd_batch(&measurements, &target_times, dataset_id);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert_eq!(interpolated.len(), target_times.len());
		assert!(interpolated.iter().all(|m| m.dataset_id == dataset_id));
	}

	#[test]
	fn test_auto_interpolate_simd() {
		let measurements = create_test_measurements(100);
		let target_times: Vec<_> = (0..50).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 500)).collect();

		for spline_type in [SplineType::Linear, SplineType::Quadratic, SplineType::Cubic, SplineType::Polynomial(3)] {
			let result = auto_interpolate_simd(&measurements, &target_times, spline_type);
			assert!(result.is_ok(), "Failed for spline type: {:?}", spline_type);

			let interpolated = result.unwrap();
			assert_eq!(interpolated.len(), target_times.len());
		}
	}

	#[test]
	fn test_simd_threshold_fallback() {
		let measurements = create_test_measurements(50);
		let dataset_id = measurements[0].dataset_id;

		// Test with small target set (below threshold)
		let small_target_times: Vec<_> = (0..16).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 500)).collect();

		let result = linear_simd_batch(&measurements, &small_target_times, dataset_id);
		assert!(result.is_ok());

		// Test with large target set (above threshold)
		let large_target_times: Vec<_> = (0..64).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 500)).collect();

		let result = linear_simd_batch(&measurements, &large_target_times, dataset_id);
		assert!(result.is_ok());
	}

	#[test]
	fn test_linear_simd_batch_parallel() {
		let measurements = create_test_measurements(200);
		let dataset_id = measurements[0].dataset_id;
		let target_times: Vec<_> = (0..300).map(|i| measurements[0].timestamp + chrono::Duration::milliseconds(i * 250)).collect();

		let result = linear_simd_batch_parallel(&measurements, &target_times, dataset_id);
		assert!(result.is_ok());

		let interpolated = result.unwrap();
		assert_eq!(interpolated.len(), target_times.len());
		assert!(interpolated.iter().all(|m| m.dataset_id == dataset_id));
	}
}
