//! Parallel and optimized interpolation processing for large datasets
//!
//! This module provides high-performance interpolation with automatic algorithm
//! selection based on dataset characteristics and benchmark-driven optimizations.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rayon::prelude::*;

use super::{Resolution, SplineType};
use crate::Measurement;

/// Threshold for switching to parallel processing
const PARALLEL_THRESHOLD: usize = 1000;
const SIMD_PARALLEL_THRESHOLD: usize = 5000;

/// Parallel interpolation with automatic algorithm selection
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub fn parallel_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	if measurements.len() < PARALLEL_THRESHOLD {
		// Use single-threaded for small datasets
		return match spline_type {
			SplineType::Linear => super::linear::linear(measurements, start, end, resolution),
			SplineType::Quadratic => super::quadratic::quadratic(measurements, start, end, resolution),
			SplineType::Cubic => super::cubic::cubic(measurements, start, end, resolution),
			SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start, end, resolution, degree),
		};
	}

	// Calculate target times
	let target_times = generate_target_times(start, end, resolution);

	if target_times.len() >= SIMD_PARALLEL_THRESHOLD {
		// Use SIMD + parallel for very large output
		return super::simd::parallel_simd_interpolate(&measurements, &target_times, spline_type);
	}

	// Use parallel processing for medium-large datasets
	let chunk_size = std::cmp::max(target_times.len() / rayon::current_num_threads(), 100);

	let measurements_arc = Arc::new(measurements);
	let results: Result<Vec<Vec<Measurement>>> = target_times
		.par_chunks(chunk_size)
		.map(|chunk| {
			let measurements_ref = measurements_arc.clone();
			interpolate_chunk(&measurements_ref, chunk, spline_type)
		})
		.collect();

	let chunk_results = results?;
	Ok(chunk_results.into_iter().flatten().collect())
}

/// Generate target times for interpolation
fn generate_target_times(start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution) -> Vec<DateTime<Utc>> {
	let step = resolution.to_step();
	let mut times = Vec::new();
	let mut current = start;

	while current <= end {
		times.push(current);
		current += step;
	}

	times
}

/// Interpolate a chunk of target times
fn interpolate_chunk(measurements: &[Measurement], target_times: &[DateTime<Utc>], spline_type: SplineType) -> Result<Vec<Measurement>> {
	// Use SIMD for the chunk if beneficial
	if target_times.len() >= 32 {
		return super::simd::auto_interpolate_simd(measurements, target_times, spline_type);
	}

	// Fall back to scalar implementation
	let start = target_times[0];
	let end = target_times[target_times.len() - 1];

	match spline_type {
		SplineType::Linear => super::linear::linear(measurements.to_vec(), start, end, Resolution::Seconds),
		SplineType::Quadratic => super::quadratic::quadratic(measurements.to_vec(), start, end, Resolution::Seconds),
		SplineType::Cubic => super::cubic::cubic(measurements.to_vec(), start, end, Resolution::Seconds),
		SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements.to_vec(), start, end, Resolution::Seconds, degree),
	}
}

/// Optimized interpolation with intelligent algorithm selection
///
/// This function provides the highest level of optimization by automatically
/// selecting the best interpolation strategy based on data characteristics.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub fn optimized_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	let measurement_count = measurements.len();
	let target_times = generate_target_times(start, end, resolution);
	let output_points = target_times.len();

	// Strategy selection based on benchmarked thresholds
	match (measurement_count, output_points) {
		// Small datasets - use scalar
		(0..=200, _) => match spline_type {
			SplineType::Linear => super::linear::linear(measurements, start, end, resolution),
			SplineType::Quadratic => super::quadratic::quadratic(measurements, start, end, resolution),
			SplineType::Cubic => super::cubic::cubic(measurements, start, end, resolution),
			SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start, end, resolution, degree),
		},

		// Medium datasets with dense output - use SIMD
		(201..=1000, 512..) => super::simd::auto_interpolate_simd(&measurements, &target_times, spline_type),

		// Large datasets and all other cases - use parallel
		_ => parallel_interpolate(measurements, start, end, resolution, spline_type),
	}
}

/// Fast path interpolation with specialized optimizations for common cases
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub fn fast_path_interpolate(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	// Fast path for linear interpolation with small datasets
	if matches!(spline_type, SplineType::Linear) && measurements.len() <= 100 {
		return super::linear::linear(measurements, start, end, resolution);
	}

	// Fast path for very dense output with linear interpolation
	let target_times = generate_target_times(start, end, resolution);
	if matches!(spline_type, SplineType::Linear) && target_times.len() >= 1000 {
		return super::simd::auto_interpolate_simd(&measurements, &target_times, spline_type);
	}

	// Default to optimized interpolation
	optimized_interpolate(measurements, start, end, resolution, spline_type)
}

/// Optimized interpolation with intelligent algorithm selection including GPU acceleration
///
/// This function provides the highest level of optimization by automatically
/// selecting the best interpolation strategy based on data characteristics.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub async fn optimized_interpolate_async(measurements: Vec<Measurement>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline_type: SplineType) -> Result<Vec<Measurement>> {
	let measurement_count = measurements.len();
	let target_times = generate_target_times(start, end, resolution);
	let output_points = target_times.len();

	// Check if GPU acceleration should be used
	let use_gpu = match spline_type {
		SplineType::Linear => super::should_use_gpu_interpolation(measurement_count, output_points),
		SplineType::Quadratic => super::quadratic::should_use_gpu_quadratic(measurement_count, output_points),
		SplineType::Cubic => super::cubic::should_use_gpu_cubic(measurement_count, output_points),
		SplineType::Polynomial(degree) => super::polynomial::should_use_gpu_polynomial(measurement_count, output_points, degree),
	};

	if use_gpu {
		// Use GPU acceleration
		return super::auto_interpolate_async(measurements, start, end, resolution, spline_type).await;
	}

	// Strategy selection based on benchmarked thresholds (CPU only)
	match (measurement_count, output_points) {
		// Small datasets - use scalar
		(0..=200, _) => match spline_type {
			SplineType::Linear => super::linear::linear(measurements, start, end, resolution),
			SplineType::Quadratic => super::quadratic::quadratic(measurements, start, end, resolution),
			SplineType::Cubic => super::cubic::cubic(measurements, start, end, resolution),
			SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start, end, resolution, degree),
		},

		// Medium datasets with dense output - use SIMD
		(201..=1000, 512..) => super::simd::auto_interpolate_simd(&measurements, &target_times, spline_type),

		// Large datasets and all other cases - use parallel
		_ => parallel_interpolate(measurements, start, end, resolution, spline_type),
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use bigdecimal::BigDecimal;
	use chrono::{TimeZone, Utc};

	use super::*;

	fn create_test_measurements(count: usize) -> Vec<Measurement> {
		let dataset_id = uuid::Uuid::new_v4();
		(0..count).map(|i| Measurement { id: uuid::Uuid::new_v4(), dataset_id, timestamp: Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::minutes(i as i64), value: BigDecimal::from_str(&format!("{}.0", i * 10)).unwrap() }).collect()
	}

	#[test]
	fn test_parallel_interpolate_small() {
		let measurements = create_test_measurements(50);
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 49, 0).unwrap();

		let result = parallel_interpolate(measurements, start, end, Resolution::Minutes, SplineType::Linear);
		assert!(result.is_ok());
	}

	#[test]
	fn test_parallel_interpolate_large() {
		let measurements = create_test_measurements(2000);
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		// Use a smaller time range to avoid invalid datetime creation
		let end = Utc.with_ymd_and_hms(2023, 1, 2, 9, 19, 0).unwrap(); // ~33 hours instead of 33 days

		let result = parallel_interpolate(measurements, start, end, Resolution::Minutes, SplineType::Linear);
		assert!(result.is_ok());
	}

	#[test]
	fn test_optimized_interpolate() {
		let measurements = create_test_measurements(100);
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 1, 39, 0).unwrap();

		let result = optimized_interpolate(measurements, start, end, Resolution::Minutes, SplineType::Linear);
		assert!(result.is_ok());
	}

	#[test]
	fn test_fast_path_interpolate() {
		let measurements = create_test_measurements(50);
		let start = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
		let end = Utc.with_ymd_and_hms(2023, 1, 1, 0, 49, 0).unwrap();

		let result = fast_path_interpolate(measurements, start, end, Resolution::Minutes, SplineType::Linear);
		assert!(result.is_ok());
	}
}
