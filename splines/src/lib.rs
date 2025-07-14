#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::multiple_crate_versions, clippy::used_underscore_binding, clippy::similar_names, clippy::module_name_repetitions, clippy::module_inception)]

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
pub(crate) use gpu::gpu_interpolate;
pub(crate) use helpers::{estimate_output_points, generate_target_times, is_uniformly_spaced, should_use_gpu};
use optimizations::{apply_fast_path, cpu_interpolate};
pub(crate) use splines::{ cubic, cubic_simd, linear, linear_simd, polynomial, polynomial_simd, quadratic, quadratic_simd };
pub use types::{Error, Point, Resolution, Spline};

mod gpu;
mod helpers;
mod optimizations;
mod splines;
mod types;

/// Main async interpolation function with GPU acceleration support
///
/// This is the primary entry point for all interpolation operations in the library.
/// It automatically selects the optimal interpolation strategy (GPU, CPU, SIMD, Parallel)
/// based on dataset characteristics and performance benchmarks.
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - Invalid time range
/// - GPU initialization fails (with CPU fallback)
/// - All interpolation methods fail
pub async fn auto_interpolate(points: Vec<Point>, start: DateTime<Utc>, end: DateTime<Utc>, resolution: Resolution, spline: Spline) -> Result<Vec<Point>> {
	if points.len() < spline.number_of_points_required() {
		bail!(Error::InsufficientMeasurementsError);
	}

	if start >= end {
		bail!(Error::InvalidTimeRangeError);
	}

	let estimated_output_points = estimate_output_points(start, end, resolution);
	let use_gpu = should_use_gpu(points.len(), estimated_output_points);
	let spline = apply_fast_path(spline, points.len());

	if use_gpu {
		// Generate target times for GPU
		let target_times = generate_target_times(start, end, resolution);

		// Try GPU interpolation with fallback - clone measurements to avoid ownership issues
		match gpu_interpolate(points.clone(), target_times, spline).await {
			Ok(result) => return Ok(result),
			Err(_) => (),
		}
	}

	// CPU implementation with optimal strategy selection
	cpu_interpolate(points, start, end, resolution, spline).await
}
