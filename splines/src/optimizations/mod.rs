use anyhow::Result;
use chrono::{DateTime, Utc};
pub use fast_path::apply_fast_path;
use rayon::prelude::*;

use crate::{Point, Resolution, Spline, cubic, cubic_simd, generate_target_times, linear, linear_simd, polynomial, polynomial_simd, quadratic, quadratic_simd};

mod fast_path;

/// Threshold for switching to parallel processing
//const PARALLEL_THRESHOLD: usize = 1000; // ← Lower threshold based on your results
const SIMD_THRESHOLD: usize = 200; // ← Adjust based on SIMD performance
const SIMD_THRESHOLD_PLUS_ONE: usize = SIMD_THRESHOLD + 1; // For SIMD, we need at least one more than the threshold

/// Optimized interpolation with intelligent algorithm selection
///
/// This function provides the highest level of optimization by automatically
/// selecting the best interpolation strategy based on data characteristics.
///
/// # Errors
///
/// Returns an error if the underlying interpolation algorithm fails
pub async fn cpu_interpolate(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> Result<Vec<Point>> {
	let input_count = points.len();
	let target_times = generate_target_times(start, end, resolution);
	let output_count = target_times.len();

	match spline {
		Spline::Linear => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar linear interpolation
					linear(points, start, end, resolution)
				},
				(input, output) if input >= SIMD_THRESHOLD && output >= SIMD_THRESHOLD_PLUS_ONE => {
					// Use parallel SIMD interpolation
					parallel_simd_interpolate(&points, &target_times, spline, resolution)
				},
				_ => {
					// Use SIMD linear interpolation
					simd_interpolate(&points, &target_times, spline, resolution)
				}
			}
		},
		Spline::Quadratic => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar quadratic interpolation
					quadratic(points, start, end, resolution)
				},
				(input, output) if input >= SIMD_THRESHOLD && output >= SIMD_THRESHOLD_PLUS_ONE => {
					// Use parallel SIMD interpolation
					parallel_simd_interpolate(&points, &target_times, spline, resolution)
				},
				_ => {
					// Use SIMD quadratic interpolation
					simd_interpolate(&points, &target_times, spline, resolution)
				}
			}
		},
		Spline::Cubic => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar cubic interpolation
					cubic(points, start, end, resolution)
				},
				(input, output) if input >= SIMD_THRESHOLD && output >= SIMD_THRESHOLD_PLUS_ONE => {
					// Use parallel SIMD interpolation
					parallel_simd_interpolate(&points, &target_times, spline, resolution)
				},
				_ => {
					// Use SIMD cubic interpolation
					simd_interpolate(&points, &target_times, spline, resolution)
				}
			}
		},
		Spline::Polynomial(degree, bounds_factor) => {
			match (input_count, output_count) {
				(input, output) if input < SIMD_THRESHOLD && output < SIMD_THRESHOLD_PLUS_ONE => {
					// Use scalar polynomial interpolation
					polynomial(points, start, end, resolution, degree, bounds_factor)
				},
				(input, output) if input >= SIMD_THRESHOLD && output >= SIMD_THRESHOLD_PLUS_ONE => {
					// Use parallel SIMD interpolation
					parallel_simd_interpolate(&points, &target_times, spline, resolution)
				},
				_ => {
					// Use SIMD polynomial interpolation
					simd_interpolate(&points, &target_times, spline, resolution)
				}
			}
		}
	}
}

/// Auto-select SIMD interpolation method based on spline type
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation fails
pub fn simd_interpolate(points: &[Point], target_times: &[DateTime<Utc>], spline: Spline, resolution: Resolution) -> Result<Vec<Point>> {
	match spline {
		Spline::Linear => linear_simd(points, target_times, resolution),
		Spline::Quadratic => quadratic_simd(points, target_times, resolution),
		Spline::Cubic => cubic_simd(points, target_times, resolution),
		Spline::Polynomial(degree, bounds_factor) => polynomial_simd(points, target_times, resolution, degree, bounds_factor),
	}
}

/// Enhanced SIMD interpolation with parallel processing for very large datasets
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation fails
pub fn parallel_simd_interpolate(points: &[Point], target_times: &[DateTime<Utc>], spline: Spline, resolution: Resolution) -> Result<Vec<Point>> {
	// Split target times into chunks and process in parallel
	let chunk_size = num_cpus::get() * 4; // Adjust chunk size based on available CPUs
	let results: Result<Vec<Vec<Point>>> = target_times.par_chunks(chunk_size).map(|chunk| simd_interpolate(points, chunk, spline, resolution)).collect();

	let chunk_results = results?;

	// Flatten results
	Ok(chunk_results.into_iter().flatten().collect())
}
