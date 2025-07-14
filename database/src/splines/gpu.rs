/* //! GPU-accelerated interpolation using WebGPU/WGPU
//!
//! This module provides high-performance GPU interpolation for dense output scenarios
//! where thousands of interpolated points are needed from large datasets.

use std::borrow::Cow;

use anyhow::{Context, Result};
use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use wgpu::util::DeviceExt;

use crate::{Error, Measurement};

/// GPU compute shader for linear interpolation
const LINEAR_INTERPOLATION_SHADER: &str = r"
@group(0) @binding(0) var<storage, read> input_times: array<f32>;
@group(0) @binding(1) var<storage, read> input_values: array<f32>;
@group(0) @binding(2) var<storage, read> target_times: array<f32>;
@group(0) @binding(3) var<storage, read_write> output_values: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let index = global_id.x;
    let target_count = arrayLength(&target_times);

    if (index >= target_count) {
	return;
    }

    let target_time = target_times[index];
    let input_count = arrayLength(&input_times);

    if (input_count < 2u) {
	output_values[index] = 0.0;
	return;
    }

    // Handle extrapolation cases
    if (target_time <= input_times[0]) {
	// Backward extrapolation using first two points
	let dt = input_times[1] - input_times[0];
	if (abs(dt) < 0.001) {
	    output_values[index] = input_values[0];
	    return;
	}
	let slope = (input_values[1] - input_values[0]) / dt;
	output_values[index] = input_values[0] + slope * (target_time - input_times[0]);
	return;
    }

    if (target_time >= input_times[input_count - 1u]) {
	// Forward extrapolation using last two points
	let dt = input_times[input_count - 1u] - input_times[input_count - 2u];
	if (abs(dt) < 0.001) {
	    output_values[index] = input_values[input_count - 1u];
	    return;
	}
	let slope = (input_values[input_count - 1u] - input_values[input_count - 2u]) / dt;
	output_values[index] = input_values[input_count - 1u] + slope * (target_time - input_times[input_count - 1u]);
	return;
    }

    // Binary search for the correct segment
    var left = 0u;
    var right = input_count - 1u;

    while (left < right - 1u) {
	let mid = (left + right) / 2u;
	if (input_times[mid] <= target_time) {
	    left = mid;
	} else {
	    right = mid;
	}
    }

    // Linear interpolation between left and right
    let t0 = input_times[left];
    let t1 = input_times[right];
    let v0 = input_values[left];
    let v1 = input_values[right];

    let dt = t1 - t0;
    if (abs(dt) < 0.001) {
	output_values[index] = v0;
	return;
    }

    let alpha = (target_time - t0) / dt;
    let clamped_alpha = clamp(alpha, 0.0, 1.0);
    output_values[index] = v0 + clamped_alpha * (v1 - v0);
}
";

/// GPU interpolation method
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuMethod {
	Linear,
	Quadratic,
	Cubic,
}

impl GpuMethod {
	const fn shader_source(self) -> &'static str {
		match self {
			Self::Linear | Self::Quadratic | Self::Cubic => LINEAR_INTERPOLATION_SHADER,
		}
	}
} */

/* /// GPU accelerated interpolation context
#[derive(Debug)]
pub struct GpuInterpolator {
	device: wgpu::Device,
	queue: wgpu::Queue,
	pipelines: std::collections::HashMap<GpuMethod, wgpu::ComputePipeline>,
	bind_group_layout: wgpu::BindGroupLayout,
}

impl GpuInterpolator {
	/// Initialize GPU interpolator with all shader variants
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - GPU adapter not found
	/// - Device creation fails
	/// - Shader compilation fails
	pub async fn new() -> Result<Self> {
		//println!("🚀 Initializing GPU interpolator...");

		// Create WGPU instance
		let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::all(), flags: wgpu::InstanceFlags::default(), dx12_shader_compiler: wgpu::Dx12Compiler::default(), gles_minor_version: wgpu::Gles3MinorVersion::Automatic });

		// Get adapter
		let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false }).await.context("Failed to find suitable GPU adapter")?;

		//println!("🔧 Found GPU adapter: {}", adapter.get_info().name);

		// Create device and queue - Fixed for wgpu 0.19
		let (device, queue) = adapter
			.request_device(
				&wgpu::DeviceDescriptor {
					label: Some("GPU Interpolator Device"),
					required_features: wgpu::Features::empty(),
					required_limits: wgpu::Limits::default(),
					// Removed memory_hints - not available in wgpu 0.19
				},
				None,
			)
			.await
			.context("Failed to create GPU device")?;

		// Create bind group layout
		let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("Interpolation Bind Group Layout"), entries: &[wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, wgpu::BindGroupLayoutEntry { binding: 2, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, wgpu::BindGroupLayoutEntry { binding: 3, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: false }, has_dynamic_offset: false, min_binding_size: None }, count: None }] });

		// Create compute pipelines for each interpolation method
		let mut pipelines = std::collections::HashMap::new();

		for &method in &[GpuMethod::Linear, GpuMethod::Quadratic, GpuMethod::Cubic] {
			let pipeline = Self::create_compute_pipeline(&device, &bind_group_layout, method);
			pipelines.insert(method, pipeline);
		}

		//println!("✅ GPU interpolator initialized with {} pipelines", pipelines.len());

		Ok(Self { device, queue, pipelines, bind_group_layout })
	}

	fn create_compute_pipeline(device: &wgpu::Device, bind_group_layout: &wgpu::BindGroupLayout, method: GpuMethod) -> wgpu::ComputePipeline {
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(&format!("{method:?} Interpolation Shader")), source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(method.shader_source())) });

		let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(&format!("{method:?} Pipeline Layout")), bind_group_layouts: &[bind_group_layout], push_constant_ranges: &[] });

		device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(&format!("{method:?} Compute Pipeline")), layout: Some(&pipeline_layout), module: &shader, entry_point: "main" })
	}

	/// Perform GPU-accelerated interpolation
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Buffer creation fails
	/// - GPU computation fails
	/// - Result mapping fails
	pub fn interpolate(&self, input_times: &[f32], input_values: &[f32], target_times: &[f32], method: GpuMethod) -> Result<Vec<f32>> {
		if input_times.len() != input_values.len() {
			return Err(anyhow::anyhow!("Input times and values must have the same length"));
		}

		if target_times.is_empty() {
			return Ok(Vec::new());
		}

		let pipeline = self.pipelines.get(&method).context("GPU pipeline not found for method")?;

		// Create buffers
		let input_times_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("Input Times Buffer"), contents: bytemuck::cast_slice(input_times), usage: wgpu::BufferUsages::STORAGE });

		let input_values_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("Input Values Buffer"), contents: bytemuck::cast_slice(input_values), usage: wgpu::BufferUsages::STORAGE });

		let target_times_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("Target Times Buffer"), contents: bytemuck::cast_slice(target_times), usage: wgpu::BufferUsages::STORAGE });

		let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Output Buffer"), size: std::mem::size_of_val(target_times) as u64, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });

		let staging_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Staging Buffer"), size: std::mem::size_of_val(target_times) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });

		// Create bind group
		let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("Interpolation Bind Group"), layout: &self.bind_group_layout, entries: &[wgpu::BindGroupEntry { binding: 0, resource: input_times_buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: input_values_buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 2, resource: target_times_buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 3, resource: output_buffer.as_entire_binding() }] });

		// Execute compute shader
		let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("GPU Interpolation Encoder") });

		{
			let mut compute_pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("GPU Interpolation Pass"), timestamp_writes: None });

			compute_pass.set_pipeline(pipeline);
			compute_pass.set_bind_group(0, &bind_group, &[]);

			let workgroup_count = target_times.len().div_ceil(256); // Round up division
			#[allow(clippy::cast_possible_truncation)]
			compute_pass.dispatch_workgroups(workgroup_count as u32, 1, 1);
		}

		// Copy result to staging buffer
		encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging_buffer, 0, std::mem::size_of_val(target_times) as u64);

		self.queue.submit([encoder.finish()]);

		// Read results - Fixed polling for wgpu 0.19
		let buffer_slice = staging_buffer.slice(..);
		let (sender, receiver) = flume::bounded(1);

		buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
			sender.send(result).ok();
		});

		// Fixed: Use poll with proper wait mode for wgpu 0.19
		self.device.poll(wgpu::Maintain::Wait);
		receiver.recv()??;

		let data = buffer_slice.get_mapped_range();
		let result: Vec<f32> = bytemuck::cast_slice(&data).to_vec();
		drop(data);
		staging_buffer.unmap();

		Ok(result)
	}
} */

/* /// Convert measurements to GPU-compatible f32 format
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn convert_measurements_to_gpu_format(measurements: &[Measurement]) -> (Vec<f32>, Vec<f32>) {
	let mut input_times = Vec::with_capacity(measurements.len());
	let mut input_values = Vec::with_capacity(measurements.len());

	let base_time = measurements[0].timestamp;

	for measurement in measurements {
		// Convert timestamp to seconds offset from base time
		let time_offset = (measurement.timestamp - base_time).num_seconds() as f32;
		input_times.push(time_offset);

		// Convert BigDecimal to f32
		let value = measurement.value.to_f64().unwrap_or(0.0) as f32;
		input_values.push(value);
	}

	(input_times, input_values)
} */

/* /// Convert `DateTime`<Utc> to GPU-compatible f32 format
#[allow(clippy::cast_precision_loss)]
fn convert_datetimes_to_gpu_format(target_times: &[DateTime<Utc>]) -> Vec<f32> {
	if target_times.is_empty() {
		return Vec::new();
	}

	let base_time = target_times[0];
	let mut gpu_times = Vec::with_capacity(target_times.len());

	for &target_time in target_times {
		let time_offset = (target_time - base_time).num_seconds() as f32;
		gpu_times.push(time_offset);
	}

	gpu_times
} */

/* /// Convert GPU results back to Measurements
fn convert_gpu_results_to_measurements(gpu_results: Vec<f32>, target_times: Vec<DateTime<Utc>>, dataset_id: Uuid) -> Result<Vec<Measurement>> {
	if gpu_results.len() != target_times.len() {
		return Err(anyhow::anyhow!("GPU results length {} does not match target times length {}", gpu_results.len(), target_times.len()));
	}

	let results = gpu_results
		.into_iter()
		.zip(target_times)
		.map(|(value, timestamp)| {
			let big_decimal_value = BigDecimal::from_f64(f64::from(value)).unwrap_or_else(|| BigDecimal::from(0));
			Measurement { id: Uuid::new_v4(), dataset_id, timestamp, value: big_decimal_value }
		})
		.collect();

	Ok(results)
} */

/* /// Simple linear interpolation function for compatibility with existing code
///
/// # Errors
///
/// Returns an error if GPU initialization or computation fails
pub async fn gpu_linear_interpolate_optimized(measurements: Vec<Measurement>, target_times: Vec<DateTime<Utc>>) -> Result<Vec<Measurement>> {
	gpu_interpolate_auto(measurements, target_times, crate::SplineType::Linear).await
} */

/* /// GPU-accelerated interpolation with automatic method selection
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements for interpolation
/// - GPU initialization fails
/// - Data conversion fails
/// - GPU computation fails
pub async fn gpu_interpolate_auto(measurements: Vec<Measurement>, target_times: Vec<DateTime<Utc>>, spline_type: crate::SplineType) -> Result<Vec<Measurement>> {
	if measurements.len() < 2 {
		return Err(Error::InsufficientMeasurementsError.into());
	}

	if target_times.is_empty() {
		return Ok(Vec::new());
	}

	// Determine GPU method from spline type
	let gpu_method = match spline_type {
		crate::SplineType::Linear => GpuMethod::Linear,
		crate::SplineType::Quadratic => GpuMethod::Quadratic,
		crate::SplineType::Cubic => GpuMethod::Cubic,
		crate::SplineType::Polynomial(degree) => {
			match degree {
				1 => GpuMethod::Linear,
				2 => GpuMethod::Quadratic,
				_ => GpuMethod::Cubic, // Use cubic for higher degrees
			}
		}
	};

	//println!("🚀 Using GPU {:?} interpolation for {} measurements -> {} points",
	//    gpu_method, measurements.len(), target_times.len());

	let gpu_instance = GpuInterpolator::new().await?;
	let dataset_id = measurements[0].dataset_id;

	// Sort measurements by timestamp
	let mut sorted_measurements = measurements;
	sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

	// Convert to GPU format
	let (input_times, input_values) = convert_measurements_to_gpu_format(&sorted_measurements);
	let gpu_target_times = convert_datetimes_to_gpu_format(&target_times);

	// Perform GPU interpolation
	let _start_time = std::time::Instant::now();
	let gpu_results = gpu_instance.interpolate(&input_times, &input_values, &gpu_target_times, gpu_method)?;
	let _gpu_duration = _start_time.elapsed();

	//println!("✅ GPU interpolation completed in {:.2}ms", _gpu_duration.as_millis());

	// Convert results back to Measurements
	convert_gpu_results_to_measurements(gpu_results, target_times, dataset_id)
} */

/// GPU-accelerated interpolation with CPU fallback
///
/// # Errors
///
/// Returns an error if both GPU and CPU interpolation fail
pub async fn gpu_interpolate_with_fallback(measurements: Vec<Measurement>, target_times: Vec<DateTime<Utc>>, spline_type: crate::SplineType) -> Result<Vec<Measurement>> {
	// Try GPU first
	match gpu_interpolate_auto(measurements.clone(), target_times.clone(), spline_type).await {
		Ok(result) => {
			//println!("✅ GPU interpolation successful");
			Ok(result)
		}
		Err(gpu_error) => {
			println!("⚠️ GPU interpolation failed: {gpu_error}, falling back to CPU");

			// Fallback to CPU implementation
			// Generate time range for CPU functions
			if target_times.is_empty() {
				return Ok(Vec::new());
			}

			let start_time = target_times[0];
			let end_time = target_times[target_times.len() - 1];

			// Use appropriate resolution based on time span
			let time_span = end_time - start_time;
			let resolution = if time_span <= chrono::Duration::seconds(1) {
				crate::Resolution::Milliseconds
			} else if time_span <= chrono::Duration::minutes(1) {
				crate::Resolution::Seconds
			} else {
				crate::Resolution::Minutes
			};

			match spline_type {
				crate::SplineType::Linear => super::linear::linear(measurements, start_time, end_time, resolution),
				crate::SplineType::Quadratic => super::quadratic::quadratic(measurements, start_time, end_time, resolution),
				crate::SplineType::Cubic => super::cubic::cubic(measurements, start_time, end_time, resolution),
				crate::SplineType::Polynomial(degree) => super::polynomial::polynomial(measurements, start_time, end_time, resolution, degree),
			}
		}
	}
}

/// Check GPU availability
///
/// # Errors
///
/// Returns an error if GPU initialization fails
pub async fn test_gpu_availability() -> Result<bool> {
	match GpuInterpolator::new().await {
		Ok(_) => {
			println!("✅ GPU acceleration available");
			Ok(true)
		}
		Err(e) => {
			println!("❌ GPU acceleration not available: {e}");
			Ok(false)
		}
	}
}

/// Determine if GPU should be used based on dataset characteristics
#[must_use]
pub fn should_use_gpu_interpolation(measurement_count: usize, estimated_output_points: usize) -> bool {
	// Updated thresholds based on the performance characteristics
	// GPU has high initialization overhead, so we need much larger datasets
	let min_measurements = 50_000; // Increased from 800
	let min_output_points = 500_000; // Increased from 20,000

	// GPU is beneficial for very dense output scenarios
	let density_factor = if measurement_count > 0 {
		#[allow(clippy::cast_precision_loss)]
		let output_f64 = estimated_output_points as f64;
		#[allow(clippy::cast_precision_loss)]
		let measurement_f64 = measurement_count as f64;
		output_f64 / measurement_f64
	} else {
		0.0
	};

	measurement_count >= min_measurements && estimated_output_points >= min_output_points && density_factor >= 5.0
	// Reduced from 10.0
}
