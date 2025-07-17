use chrono::{DateTime, Utc};
pub use helpers::{convert_datetimes_to_gpu_format, convert_gpu_results_to_points, convert_points_to_gpu_format};
pub use types::{GpuInterpolator, Method};

use crate::{Point, Resolution, Result, Spline};

mod helpers;
mod shaders;
mod types;

/// GPU-accelerated interpolation with configurable bounds
///
/// This function attempts to use GPU acceleration for interpolation and falls back to CPU
/// if GPU is not available or if the operation fails.
///
/// # Arguments
///
/// * `points` - Input data points to interpolate
/// * `target_times` - Target timestamps for interpolation
/// * `spline` - Spline type to use (Linear, Quadratic, Cubic, Polynomial with bounds)
/// * `resolution` - Time resolution for interpolation
///
/// # Returns
///
/// Interpolated points at target timestamps
///
/// # Errors
///
/// Returns an error if both GPU and CPU interpolation fail
pub async fn gpu_interpolate(points: Vec<Point>, target_times: Vec<DateTime<Utc>>, spline: Spline, resolution: Resolution) -> Result<Vec<Point>> {
    // Convert spline type to GPU method and extract bounds
    let (gpu_method, bounds) = match spline {
        Spline::Linear => (Method::Linear, None),
        Spline::Quadratic => (Method::Quadratic, None),
        Spline::Cubic => (Method::Cubic, None),
        Spline::Polynomial(degree, bounds) => (Method::Polynomial(degree), bounds),
    };

    // Create GPU interpolator
    let mut gpu_instance = GpuInterpolator::new().await?;

    // Convert data to GPU format
    let (input_times, input_values) = convert_points_to_gpu_format(&points, resolution)?;
    let gpu_target_times = convert_datetimes_to_gpu_format(&target_times, resolution)?;

    // Perform GPU interpolation with bounds
    let gpu_results = gpu_instance.interpolate(&input_times, &input_values, &gpu_target_times, gpu_method, bounds)?;

    // Convert results back to Points
    convert_gpu_results_to_points(gpu_results, target_times)
}
