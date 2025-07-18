use chrono::{DateTime, Utc};
pub use helpers::{convert_datetimes_to_gpu_format_f32, convert_datetimes_to_gpu_format_f64, convert_gpu_results_to_points_f32, convert_gpu_results_to_points_f64, convert_points_to_gpu_format_f32, convert_points_to_gpu_format_f64, get_max_buffer_size};
pub use types::{GpuInterpolator, Method};

use crate::{Point, Resolution, Result, Spline};

mod helpers;
mod shaders;
mod types;

// Use a conservative GPU buffer limit to account for all buffers
const F64_SIZE: usize = std::mem::size_of::<f64>(); // 8 bytes
const F32_SIZE: usize = std::mem::size_of::<f32>(); // 4 bytes
const NUMBER_OF_BUFFERS: usize = 4; // Input times, input values, target times, results

/// GPU-accelerated interpolation with nanosecond precision support and automatic batching
pub async fn gpu_interpolate(points: Vec<Point>, target_times: Vec<DateTime<Utc>>, spline: Spline, resolution: Resolution) -> Result<Vec<Point>> {
	if points.is_empty() || target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Convert spline type to GPU method
	let method = match spline {
		Spline::Linear => Method::Linear,
		Spline::Quadratic => Method::Quadratic,
		Spline::Cubic => Method::Cubic,
		Spline::Polynomial(degree, _) => Method::Polynomial(degree), // Ignore bounds_factor for GPU
	};

	// Create GPU interpolator
	let mut interpolator = GpuInterpolator::new().await?;

	if interpolator.supports_f64() { gpu_interpolate_f64(&points, &target_times, &resolution, &method, &mut interpolator).await } else { gpu_interpolate_f32(&points, &target_times, &resolution, &method, &mut interpolator).await }
}

async fn gpu_interpolate_f64(points: &[Point], target_times: &[DateTime<Utc>], resolution: &Resolution, method: &Method, interpolator: &mut GpuInterpolator) -> Result<Vec<Point>> {
	// Determine data size based on GPU precision support
	let element_size = F64_SIZE;

	// Convert points to GPU format
	let (input_times, input_values) = convert_points_to_gpu_format_f64(&points, resolution)?;

	// Use first point's timestamp as base
	let base_time = points[0].timestamp;
	let target_times_gpu = convert_datetimes_to_gpu_format_f64(&target_times, resolution, base_time)?;

	// Calculate conservative batch size
	let max_single_buffer_size = get_max_buffer_size().await? as usize / NUMBER_OF_BUFFERS / element_size;
	let max_targets_per_batch = (max_single_buffer_size / element_size).min(target_times_gpu.len());

	// Process in batches
	let mut all_results = Vec::with_capacity(target_times_gpu.len());
	//let total_batches = (target_times_gpu.len() + max_targets_per_batch - 1) / max_targets_per_batch;

	for target_batch in target_times_gpu.chunks(max_targets_per_batch) {
		// Perform interpolation
		let batch_results = interpolator.interpolate_f64(&input_times, &input_values, target_batch, method)?;
		all_results.extend(batch_results);
	}

	// Convert results back to Points
	convert_gpu_results_to_points_f64(all_results, target_times)
}

async fn gpu_interpolate_f32(points: &[Point], target_times: &[DateTime<Utc>], resolution: &Resolution, method: &Method, interpolator: &mut GpuInterpolator) -> Result<Vec<Point>> {
	let element_size = F32_SIZE;
	let (input_times, input_values) = convert_points_to_gpu_format_f32(&points, resolution)?;
	let base_time = points[0].timestamp;
	let target_times_gpu = convert_datetimes_to_gpu_format_f32(&target_times, resolution, base_time)?;

	// Calculate conservative batch size
	let max_buffer_binding_size = interpolator.max_storage_buffer_binding_size;
	let workgroup_size = 256;
	let max_compute_workgroups = interpolator.max_compute_workgroups_per_dimension;
	let max_elements_per_batch = (max_buffer_binding_size / element_size).min(target_times_gpu.len()).min(max_compute_workgroups * workgroup_size);

	println!("GPU Interpolation: Max buffer binding size: {} bytes, Max elements per batch: {}", max_buffer_binding_size, max_elements_per_batch);

	// Process in batches
	let mut all_results = Vec::with_capacity(target_times_gpu.len());
	for target_batch in target_times_gpu.chunks(max_elements_per_batch) {
		let batch_results = interpolator.interpolate_f32(&input_times, &input_values, &target_batch, method)?;
		all_results.extend(batch_results);
	}

	convert_gpu_results_to_points_f32(all_results, target_times)
}
