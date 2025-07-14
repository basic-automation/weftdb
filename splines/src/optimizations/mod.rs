use anyhow::Result;
use chrono::{DateTime, Utc};
pub use fast_path::apply_fast_path;
use rayon::prelude::*;

use crate::{Point, Resolution, Spline, cubic, cubic_simd, generate_target_times, linear, linear_simd, polynomial, polynomial_simd, quadratic, quadratic_simd};

mod fast_path;

/// Threshold for switching to parallel processing
const PARALLEL_THRESHOLD: usize = 1000; // ← Lower threshold based on your results
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
	let point_count = points.len();
	let target_times = generate_target_times(start, end, resolution);
	let output_points = target_times.len();

	// Strategy selection based on benchmarked thresholds
	match (point_count, output_points) {
		(0..=SIMD_THRESHOLD, _) => match spline {
			Spline::Linear => linear(points, start, end, resolution),
			Spline::Quadratic => quadratic(points, start, end, resolution),
			Spline::Cubic => cubic(points, start, end, resolution),
			Spline::Polynomial(degree) => polynomial(points, start, end, resolution, degree),
		},

		// Medium datasets with dense output - use SIMD
		(SIMD_THRESHOLD_PLUS_ONE..=PARALLEL_THRESHOLD, 512..) => simd_interpolate(&points, &target_times, spline, resolution),

		// Large datasets and all other cases - use parallel
		_ => parallel_simd_interpolate(&points, &target_times, spline, resolution),
	}
}

/// Auto-select SIMD interpolation method based on spline type
///
/// # Errors
///
/// Returns an error if the underlying SIMD interpolation fails
fn simd_interpolate(points: &[Point], target_times: &[DateTime<Utc>], spline: Spline, resolution: Resolution) -> Result<Vec<Point>> {
	match spline {
		Spline::Linear => linear_simd(points, target_times, resolution),
		Spline::Quadratic => quadratic_simd(points, target_times, resolution),
		Spline::Cubic => cubic_simd(points, target_times, resolution),
		Spline::Polynomial(degree) => polynomial_simd(points, target_times, resolution, degree),
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
