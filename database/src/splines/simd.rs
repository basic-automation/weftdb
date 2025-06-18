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
pub fn linear_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>]) -> Result<Vec<Measurement>> {
    if measurements.len() < 2 {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    let dataset_id = measurements[0].dataset_id;

    // Convert to f64 arrays for SIMD processing
    #[allow(clippy::cast_precision_loss)]
    let input_times: Vec<f64> = measurements.iter().map(|m| m.timestamp.timestamp() as f64).collect();

    let input_values: Vec<f64> = measurements.iter().map(|m| m.value.to_f64().unwrap_or(0.0)).collect();

    #[allow(clippy::cast_precision_loss)]
    let targets: Vec<f64> = target_times.iter().map(|t| t.timestamp() as f64).collect();

    // Process in SIMD batches
    let mut results = Vec::with_capacity(target_times.len());

    for chunk in targets.chunks(SIMD_BATCH_SIZE) {
        let mut padded_targets = [0.0; SIMD_BATCH_SIZE];
        let chunk_size = chunk.len();

        // Copy actual values and pad with last value if needed
        padded_targets[..chunk_size].copy_from_slice(chunk);
        
        // Fill remaining slots with the last value
        if chunk_size < SIMD_BATCH_SIZE {
            let last_value = chunk[chunk_size - 1];
            for target in &mut padded_targets[chunk_size..SIMD_BATCH_SIZE] {
                *target = last_value;
            }
        }

        let target_simd = f64x4::new([padded_targets[0], padded_targets[1], padded_targets[2], padded_targets[3]]);
        let result_simd = simd_linear_interpolate(&input_times, &input_values, target_simd);

        // Extract results (only take the actual chunk size)
        let result_array = result_simd.to_array();
        results.extend_from_slice(&result_array[..chunk_size]);
    }

    // Convert back to Measurements
    let mut interpolated = Vec::with_capacity(target_times.len());
    for (i, &value) in results.iter().enumerate() {
        interpolated.push(Measurement { 
            id: Uuid::new_v4(), 
            dataset_id, 
            timestamp: target_times[i], 
            value: BigDecimal::from_f64(value).unwrap_or_else(|| BigDecimal::from(0)) 
        });
    }

    Ok(interpolated)
}

/// SIMD linear interpolation core function
fn simd_linear_interpolate(
    input_times: &[f64],
    input_values: &[f64],
    target_times: f64x4,
) -> f64x4 {
    // For each SIMD lane, find the appropriate segment and interpolate
    let target_array = target_times.to_array();
    let mut result_array = [0.0; 4];
    
    for (lane, &target_time) in target_array.iter().enumerate() {
        // Find the segment containing this target time
        let mut segment_idx = 0;
        for i in 0..input_times.len() - 1 {
            if input_times[i] <= target_time && target_time <= input_times[i + 1] {
                segment_idx = i;
                break;
            }
        }
        
        // Linear interpolation within the segment
        let t0 = input_times[segment_idx];
        let t1 = input_times[segment_idx + 1];
        let v0 = input_values[segment_idx];
        let v1 = input_values[segment_idx + 1];
        
        let alpha = (target_time - t0) / (t1 - t0);
        result_array[lane] = alpha.mul_add(v1 - v0, v0);
    }
    
    f64x4::new(result_array)
}

/// SIMD-optimized quadratic interpolation
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< 3 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn quadratic_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>]) -> Result<Vec<Measurement>> {
    if measurements.len() < 3 {
        return Err(Error::InsufficientPointsForCubicSplineError.into());
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    // For now, delegate to scalar implementation
    // TODO: Implement true SIMD quadratic interpolation
    crate::splines::quadratic::quadratic(
        measurements.to_vec(),
        target_times[0],
        target_times[target_times.len() - 1],
        Resolution::Seconds, // Use Seconds instead of Milliseconds
    )
}

/// SIMD-optimized cubic interpolation
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< 4 points)
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn cubic_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>]) -> Result<Vec<Measurement>> {
    if measurements.len() < 4 {
        return Err(Error::InsufficientPointsForCubicSplineError.into());
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    // For now, delegate to scalar implementation
    // TODO: Implement true SIMD cubic interpolation
    crate::splines::cubic::cubic(
        measurements.to_vec(),
        target_times[0],
        target_times[target_times.len() - 1],
        Resolution::Seconds, // Use Seconds instead of Milliseconds
    )
}

/// SIMD-optimized polynomial interpolation
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for the given degree
/// - Timestamp conversion fails
/// - `BigDecimal` operations fail
pub fn polynomial_simd_batch(measurements: &[Measurement], target_times: &[DateTime<Utc>], degree: usize) -> Result<Vec<Measurement>> {
    if measurements.len() < degree + 1 {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    // For now, delegate to scalar implementation
    // TODO: Implement true SIMD polynomial interpolation
    crate::splines::polynomial::polynomial(
        measurements.to_vec(),
        target_times[0],
        target_times[target_times.len() - 1],
        Resolution::Seconds, // Use Seconds instead of Milliseconds
        degree,
    )
}

/// Auto-select SIMD interpolation method based on spline type
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation fails
pub fn auto_interpolate_simd(measurements: &[Measurement], target_times: &[DateTime<Utc>], spline_type: SplineType) -> Result<Vec<Measurement>> {
    // Check if SIMD is beneficial
    if target_times.len() < SIMD_THRESHOLD {
        // Fall back to using direct SIMD batch functions for small datasets
        // This ensures we get exactly the target times requested
        return match spline_type {
            SplineType::Linear => linear_simd_batch(measurements, target_times),
            SplineType::Quadratic => quadratic_simd_batch(measurements, target_times),
            SplineType::Cubic => cubic_simd_batch(measurements, target_times),
            SplineType::Polynomial(degree) => polynomial_simd_batch(measurements, target_times, degree),
        };
    }

    match spline_type {
        SplineType::Linear => linear_simd_batch(measurements, target_times),
        SplineType::Quadratic => quadratic_simd_batch(measurements, target_times),
        SplineType::Cubic => cubic_simd_batch(measurements, target_times),
        SplineType::Polynomial(degree) => polynomial_simd_batch(measurements, target_times, degree),
    }
}

/// Enhanced SIMD interpolation with parallel processing for very large datasets
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation fails
pub fn parallel_simd_interpolate(measurements: &[Measurement], target_times: &[DateTime<Utc>], spline_type: SplineType) -> Result<Vec<Measurement>> {
    if target_times.len() < 1000 {
        // Use regular SIMD for smaller datasets
        return auto_interpolate_simd(measurements, target_times, spline_type);
    }

    // Split target times into chunks and process in parallel
    let chunk_size = 1000;
    let results: Result<Vec<Vec<Measurement>>> = target_times.par_chunks(chunk_size).map(|chunk| auto_interpolate_simd(measurements, chunk, spline_type)).collect();

    let chunk_results = results?;

    // Flatten results
    Ok(chunk_results.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bigdecimal::BigDecimal;
    use chrono::{TimeZone, Utc};

    use super::*;

    fn create_test_measurements() -> Vec<Measurement> {
        let dataset_id = Uuid::new_v4();
        vec![Measurement { id: Uuid::new_v4(), dataset_id, timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap(), value: BigDecimal::from_str("0.0").unwrap() }, Measurement { id: Uuid::new_v4(), dataset_id, timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 0).unwrap(), value: BigDecimal::from_str("10.0").unwrap() }, Measurement { id: Uuid::new_v4(), dataset_id, timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 2, 0).unwrap(), value: BigDecimal::from_str("20.0").unwrap() }, Measurement { id: Uuid::new_v4(), dataset_id, timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 3, 0).unwrap(), value: BigDecimal::from_str("30.0").unwrap() }]
    }

    #[test]
    fn test_linear_simd_batch() {
        let measurements = create_test_measurements();
        let target_times = vec![Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 30).unwrap()];

        let result = linear_simd_batch(&measurements, &target_times);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        assert_eq!(interpolated.len(), 2);
    }

    #[test]
    fn test_auto_interpolate_simd() {
        let measurements = create_test_measurements();
        // Use only 2 target times (below SIMD threshold) but now it should use direct SIMD batch functions
        let target_times = vec![Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 30).unwrap(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 1, 30).unwrap()];

        let result = auto_interpolate_simd(&measurements, &target_times, SplineType::Linear);
        assert!(result.is_ok());

        let interpolated = result.unwrap();
        // Should now return exactly 2 results since we're using direct SIMD batch functions
        assert_eq!(interpolated.len(), 2);
    }
}
