use chrono::{DateTime, Utc};
pub use helpers::{convert_datetimes_to_gpu_format, convert_gpu_results_to_points, convert_points_to_gpu_format};
pub use types::{GpuInterpolator, Method};

use crate::{Error, Point, Result, Spline};

mod helpers;
mod shaders;
mod types;

/// GPU-accelerated interpolation with automatic method selection
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient points for interpolation
/// - GPU initialization fails
/// - Data conversion fails
/// - GPU computation fails
pub async fn gpu_interpolate(points: Vec<Point>, target_times: Vec<DateTime<Utc>>, spline: Spline) -> Result<Vec<Point>> {
	if points.len() < 2 {
		return Err(Error::InsufficientPointsError.into());
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Determine GPU method from spline type
	let gpu_method = match spline {
		Spline::Linear => Method::Linear,
		Spline::Quadratic => Method::Quadratic,
		Spline::Cubic => Method::Cubic,
		Spline::Polynomial(degree) => match degree {
			1 => Method::Linear,
			2 => Method::Quadratic,
			3 => Method::Cubic,
			_ => Method::Polynomial(degree),
		},
	};

	let gpu_instance = GpuInterpolator::new().await?;

	// Sort points by timestamp
	let mut sorted_points = points;
	sorted_points.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Convert to GPU format
	let (input_times, input_values) = convert_points_to_gpu_format(&sorted_points);
	let gpu_target_times = convert_datetimes_to_gpu_format(&target_times);

	// Perform GPU interpolation
	let gpu_results = gpu_instance.interpolate(&input_times, &input_values, &gpu_target_times, gpu_method)?;

	// Convert results back to Points
	convert_gpu_results_to_points(gpu_results, target_times)
}
