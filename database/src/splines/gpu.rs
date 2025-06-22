//! GPU-accelerated linear interpolation using WebGPU/WGPU
//!
//! This module provides high-performance GPU interpolation for dense output scenarios
//! where thousands of interpolated points are needed from large datasets.

use std::{borrow::Cow, sync::Arc};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use uuid::Uuid;
use wgpu::util::DeviceExt;
use bigdecimal::ToPrimitive;

use crate::{Error, Measurement};

/// Convert measurements to GPU-compatible f32 format
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
}

/// Convert `DateTime<Utc>` to GPU-compatible f32 format
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
}

/// Convert GPU results back to Measurements
fn convert_gpu_results_to_measurements(
    gpu_results: Vec<f32>, 
    target_times: Vec<DateTime<Utc>>, 
    dataset_id: Uuid
) -> Result<Vec<Measurement>> {
    use bigdecimal::{BigDecimal, FromPrimitive};

    if gpu_results.len() != target_times.len() {
        return Err(anyhow::anyhow!(
            "GPU results length {} does not match target times length {}", 
            gpu_results.len(), 
            target_times.len()
        ));
    }

    let results = gpu_results
        .into_iter()
        .zip(target_times)  // Remove .into_iter()
        .map(|(value, timestamp)| {
            let big_decimal_value = BigDecimal::from_f32(value)
                .unwrap_or_else(|| BigDecimal::from(0));
            
            Measurement {
                id: Uuid::new_v4(),
                dataset_id,
                timestamp,
                value: big_decimal_value,
            }
        })
        .collect();

    Ok(results)
}

/// GPU buffer set for reusable GPU operations
#[derive(Debug)]
pub struct GpuBufferSet {
	pub input_times: wgpu::Buffer,
	pub input_values: wgpu::Buffer,
	pub target_times: wgpu::Buffer,
	pub output: wgpu::Buffer,
	pub max_capacity: usize,
}

/// Simple shared GPU instance storage
static GPU_INSTANCE: tokio::sync::OnceCell<Arc<Mutex<GpuLinearInterpolator>>> = tokio::sync::OnceCell::const_new();

/// Get or create shared GPU instance
async fn get_shared_gpu_instance() -> Result<Arc<GpuLinearInterpolator>> {
    let instance_mutex = GPU_INSTANCE
        .get_or_init(|| async {
            match GpuLinearInterpolator::new().await {
                Ok(gpu) => Arc::new(Mutex::new(gpu)),
                Err(e) => {
                    eprintln!("Failed to initialize GPU: {e}");
                    panic!("GPU initialization failed: {e}");
                }
            }
        })
        .await;

    let gpu_guard = instance_mutex.lock().await;
    let gpu_clone = (*gpu_guard).clone();
    drop(gpu_guard);

    Ok(Arc::new(gpu_clone))
}

/// GPU compute shader for linear interpolation using f32 for better compatibility
const LINEAR_INTERPOLATION_SHADER: &str = r"
@group(0) @binding(0) var<storage, read> input_times: array<f32>;
@group(0) @binding(1) var<storage, read> input_values: array<f32>;
@group(0) @binding(2) var<storage, read> target_times: array<f32>;
@group(0) @binding(3) var<storage, read_write> output_values: array<f32>;

// Custom functions for NaN and infinity checks since WGSL doesn't have built-in versions
fn is_nan(value: f32) -> bool {
    return value != value;
}

fn is_inf(value: f32) -> bool {
    return abs(value) > 3.402823466e+38; // Maximum finite f32 value
}

fn is_finite(value: f32) -> bool {
    return !is_nan(value) && !is_inf(value);
}

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
    
    // Handle extrapolation cases first
    if (target_time <= input_times[0]) {
        // Backward extrapolation using first two points
        let dt = input_times[1] - input_times[0];
        if (abs(dt) < 0.001) {
            output_values[index] = input_values[0];
            return;
        }
        let slope = (input_values[1] - input_values[0]) / dt;
        let extrapolated = input_values[0] + slope * (target_time - input_times[0]);
        
        // Validate extrapolated result
        if (is_finite(extrapolated)) {
            output_values[index] = extrapolated;
        } else {
            output_values[index] = input_values[0]; // Fallback
        }
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
        let extrapolated = input_values[input_count - 1u] + slope * (target_time - input_times[input_count - 1u]);
        
        // Validate extrapolated result
        if (is_finite(extrapolated)) {
            output_values[index] = extrapolated;
        } else {
            output_values[index] = input_values[input_count - 1u]; // Fallback
        }
        return;
    }
    
    // Binary search for the correct segment for interpolation
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
        // Points are too close, use the first value
        output_values[index] = v0;
        return;
    }
    
    let alpha = (target_time - t0) / dt;
    // Clamp alpha to prevent overshooting
    let clamped_alpha = clamp(alpha, 0.0, 1.0);
    let interpolated = v0 + clamped_alpha * (v1 - v0);
    
    // Check for NaN or infinite values using custom functions
    if (is_finite(interpolated)) {
        output_values[index] = interpolated;
    } else {
        output_values[index] = v0; // Fallback to first value
    }
}
";

/// GPU accelerated linear interpolation context
#[derive(Debug, Clone)]
pub struct GpuLinearInterpolator {
	device: wgpu::Device,
	queue: wgpu::Queue,
	compute_pipeline: wgpu::ComputePipeline,
	bind_group_layout: wgpu::BindGroupLayout,
}

impl GpuLinearInterpolator {
	/// Initialize GPU linear interpolator
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - GPU adapter not found
	/// - Device creation fails
	/// - Shader compilation fails
	pub async fn new() -> Result<Self> {
		Self::new_with_logging(false).await
	}

	/// Initialize GPU linear interpolator with explicit logging control
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - GPU adapter not found
	/// - Device creation fails
	/// - Shader compilation fails
	/// - Pipeline creation fails
	#[allow(clippy::too_many_lines)]
	pub async fn new_with_logging(verbose: bool) -> Result<Self> {
		if verbose {
			println!("🔍 Initializing GPU interpolator...");
		}

		// Create WGPU instance
		let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::all(), ..Default::default() });

		if verbose {
			println!("✅ WGPU instance created");
		}

		// Get adapter
		if verbose {
			println!("🔍 Requesting GPU adapter...");
		}

		let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false }).await.context("Failed to find suitable GPU adapter")?;

		// Log detailed adapter info only if verbose
		if verbose {
			let adapter_info = adapter.get_info();
			println!("🎯 GPU Adapter Details:");
			println!("   Name: {}", adapter_info.name);
			println!("   Backend: {:?}", adapter_info.backend);
			println!("   Device Type: {:?}", adapter_info.device_type);
			println!("   Driver: {}", adapter_info.driver);
			println!("   Driver Info: {}", adapter_info.driver_info);
		}

		// Create device and queue
		if verbose {
			println!("🔍 Creating GPU device and queue...");
		}

		let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor { label: Some("GPU Linear Interpolator Device"), required_features: wgpu::Features::empty(), required_limits: wgpu::Limits::default(), memory_hints: wgpu::MemoryHints::Performance, trace: wgpu::Trace::Off }).await.context("Failed to create GPU device")?;

		if verbose {
			println!("✅ GPU device and queue created successfully");
		}

		// Create bind group layout
		let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("Linear Interpolation Bind Group Layout"), entries: &[wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, wgpu::BindGroupLayoutEntry { binding: 2, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, wgpu::BindGroupLayoutEntry { binding: 3, visibility: wgpu::ShaderStages::COMPUTE, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: false }, has_dynamic_offset: false, min_binding_size: None }, count: None }] });

		// Create compute pipeline
		if verbose {
			println!("🔍 Creating compute pipeline and shader...");
		}

		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("Linear Interpolation Shader"), source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(LINEAR_INTERPOLATION_SHADER)) });

		let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("Linear Interpolation Pipeline Layout"), bind_group_layouts: &[&bind_group_layout], push_constant_ranges: &[] });

		let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
			label: Some("Linear Interpolation Pipeline"),
			layout: Some(&pipeline_layout),
			module: &shader,
			entry_point: Some("main"),
			compilation_options: wgpu::PipelineCompilationOptions::default(),
			cache: None,
		});

		if verbose {
			println!("✅ GPU interpolator fully initialized and ready!");
		}

		Ok(Self { device, queue, compute_pipeline, bind_group_layout })
	}

	/// Should use GPU based on data characteristics
	#[must_use]
	pub const fn should_use_gpu(measurement_count: usize, target_count: usize) -> bool {
		// UPDATED: Much more aggressive GPU usage based on benchmark results
		// GPU shows 1.3-4.2x performance improvement at these levels
		measurement_count >= 1_000 && target_count >= 25_000
	}

	/// Perform GPU-accelerated linear interpolation using f32 for compatibility
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Buffer creation fails
	/// - GPU computation fails
	/// - Result mapping fails
	///
	/// # Panics
	///
	/// Panics if the channel communication fails during async buffer mapping
	pub fn interpolate(&self, input_times: &[f32], input_values: &[f32], target_times: &[f32]) -> Result<Vec<f32>> {
		let input_times_bytes = bytemuck::cast_slice(input_times);
		let input_values_bytes = bytemuck::cast_slice(input_values);
		let target_times_bytes = bytemuck::cast_slice(target_times);

		// Create buffers
		let input_times_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("Input Times Buffer"), contents: input_times_bytes, usage: wgpu::BufferUsages::STORAGE });

		let input_values_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("Input Values Buffer"), contents: input_values_bytes, usage: wgpu::BufferUsages::STORAGE });

		let target_times_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("Target Times Buffer"), contents: target_times_bytes, usage: wgpu::BufferUsages::STORAGE });

		let output_buffer_size = std::mem::size_of_val(target_times) as wgpu::BufferAddress;
		let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Output Buffer"), size: output_buffer_size, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });

		let staging_buffer = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("Staging Buffer"), size: output_buffer_size, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });

		// Create bind group
		let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("Linear Interpolation Bind Group"), layout: &self.bind_group_layout, entries: &[wgpu::BindGroupEntry { binding: 0, resource: input_times_buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: input_values_buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 2, resource: target_times_buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 3, resource: output_buffer.as_entire_binding() }] });

		// Create command encoder and dispatch
		let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Linear Interpolation Encoder") });

		{
			let mut compute_pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("Linear Interpolation Pass"), timestamp_writes: None });
			compute_pass.set_pipeline(&self.compute_pipeline);
			compute_pass.set_bind_group(0, &bind_group, &[]);

			let workgroup_size = 256;
			let num_workgroups = target_times.len().div_ceil(workgroup_size);
			#[allow(clippy::cast_possible_truncation)]
			compute_pass.dispatch_workgroups(num_workgroups as u32, 1, 1);
		}

		encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging_buffer, 0, output_buffer_size);

		// Submit command buffer
		self.queue.submit(std::iter::once(encoder.finish()));

		// Wait for completion and read results
		let (sender, receiver) = flume::unbounded();
		let buffer_slice = staging_buffer.slice(..);

		buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
			let _ = sender.send(result);
		});

		let _ = self.device.poll(wgpu::MaintainBase::Wait);
		receiver.recv().unwrap().context("Failed to map buffer")?;

		let data = buffer_slice.get_mapped_range();
		let result: Vec<f32> = bytemuck::cast_slice(&data).to_vec();

		drop(data);
		staging_buffer.unmap();

		Ok(result)
	}

    /// Interpolate using pre-allocated buffers for reduced overhead
    fn interpolate_with_reused_buffers(&self, input_times: &[f32], input_values: &[f32], target_times: &[f32], buffers: &GpuBufferSet) -> Result<Vec<f32>> {
        // Write data to existing buffers
        self.queue.write_buffer(&buffers.input_times, 0, bytemuck::cast_slice(input_times));
        self.queue.write_buffer(&buffers.input_values, 0, bytemuck::cast_slice(input_values));
        self.queue.write_buffer(&buffers.target_times, 0, bytemuck::cast_slice(target_times));

        // Create staging buffer for reading results
        let output_size = std::mem::size_of_val(target_times);  // Fix manual calculation
        let staging_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Staging Buffer (Reused)"),
            size: output_size as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Create bind group with reused buffers
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Linear Interpolation Bind Group (Reused)"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buffers.input_times.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: buffers.input_values.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: buffers.target_times.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: buffers.output.as_entire_binding() },
            ],
        });

        // Execute compute pass
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Linear Interpolation Encoder (Reused)"),
		});

		{
			let mut compute_pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
				label: Some("Linear Interpolation Pass (Reused)"),
				timestamp_writes: None,
			});
			compute_pass.set_pipeline(&self.compute_pipeline);
			compute_pass.set_bind_group(0, &bind_group, &[]);

			let workgroup_size = 256;
			let num_workgroups = target_times.len().div_ceil(workgroup_size);
			#[allow(clippy::cast_possible_truncation)]
			compute_pass.dispatch_workgroups(num_workgroups as u32, 1, 1);
		}

		encoder.copy_buffer_to_buffer(&buffers.output, 0, &staging_buffer, 0, output_size as u64);
		self.queue.submit(std::iter::once(encoder.finish()));

		// Read results
		let (sender, receiver) = flume::unbounded();
		let buffer_slice = staging_buffer.slice(..);

		buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
			let _ = sender.send(result);
		});

		let _ = self.device.poll(wgpu::MaintainBase::Wait);
		receiver.recv().unwrap().context("Failed to map buffer")?;

		let data = buffer_slice.get_mapped_range();
		let result: Vec<f32> = bytemuck::cast_slice(&data).to_vec();

		drop(data);
		staging_buffer.unmap();

		Ok(result)
	}

	/// Optimized interpolation with buffer reuse and batch processing
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Buffer reuse fails
	/// - GPU interpolation fails
	/// - Input validation fails
	pub fn interpolate_optimized(&self, input_times: &[f32], input_values: &[f32], target_times: &[f32], buffers: Option<&GpuBufferSet>) -> Result<Vec<f32>> {
		// Check if we can reuse existing buffers
		if let Some(reusable_buffers) = buffers {
			if reusable_buffers.max_capacity >= target_times.len().max(input_times.len()) {
				return self.interpolate_with_reused_buffers(input_times, input_values, target_times, reusable_buffers);
			}
		}

		// Fallback to regular interpolation
		self.interpolate(input_times, input_values, target_times)
	}

	/// Pre-allocate GPU buffers to reduce memory allocation overhead
	#[must_use]
	pub fn create_optimized_buffers(&self, max_size: usize) -> GpuBufferSet {
		let buffer_size = (max_size * std::mem::size_of::<f32>()) as u64;
		let large_buffer_size = (max_size * 4 * std::mem::size_of::<f32>()) as u64; // 4x for dense output

		let input_times_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("Input Times Buffer (Optimized)"),
			size: buffer_size,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});

		let input_values_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("Input Values Buffer (Optimized)"),
			size: buffer_size,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});

		let target_times_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("Target Times Buffer (Optimized)"),
			size: large_buffer_size,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});

		let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
			label: Some("Output Buffer (Optimized)"),
			size: large_buffer_size,
			usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
			mapped_at_creation: false,
		});

		GpuBufferSet {
			input_times: input_times_buffer,
			input_values: input_values_buffer,
			target_times: target_times_buffer,
			output: output_buffer,
			max_capacity: max_size,
		}
	}
}

/// Optimized GPU interpolation with buffer caching for repeated operations
///
/// # Errors
///
/// Returns an error if:
/// - Insufficient measurements (< 2 points)
/// - GPU initialization fails
/// - Data conversion fails
/// - GPU computation fails
pub async fn gpu_linear_interpolate_optimized(
    measurements: Vec<Measurement>, 
    target_times: Vec<DateTime<Utc>>, 
    dataset_id: Uuid
) -> Result<Vec<Measurement>> {
    if measurements.len() < 2 {
        return Err(Error::InsufficientMeasurementsError.into());
    }

    if target_times.is_empty() {
        return Ok(Vec::new());
    }

    // Use shared GPU instance
    let gpu_instance = get_shared_gpu_instance().await?;

    // Pre-process data efficiently
    let mut sorted_measurements = measurements;
    sorted_measurements.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    // Convert to GPU format without Result wrapping
    let (input_times, input_values) = convert_measurements_to_gpu_format(&sorted_measurements);
    let gpu_target_times = convert_datetimes_to_gpu_format(&target_times);

    // For large datasets, create optimized buffers
    let optimized_buffers = if target_times.len() > 10_000 {
        Some(gpu_instance.create_optimized_buffers(target_times.len().max(input_times.len())))
    } else {
        None
    };

    // Use optimized interpolation with buffer reuse
    let gpu_results = gpu_instance.interpolate_optimized(
        &input_times, 
        &input_values, 
        &gpu_target_times, 
        optimized_buffers.as_ref()
    )?;

    // Convert results back to Measurements
    convert_gpu_results_to_measurements(gpu_results, target_times, dataset_id)
}

/// GPU-accelerated linear interpolation with CPU fallback
///
/// # Errors
///
/// Returns an error if both GPU and CPU interpolation fail
pub async fn gpu_linear_interpolate_with_fallback(
    measurements: Vec<Measurement>, 
    target_times: Vec<DateTime<Utc>>, 
    dataset_id: Uuid
) -> Result<Vec<Measurement>> {
    // Try GPU first
    match gpu_linear_interpolate_optimized(measurements.clone(), target_times.clone(), dataset_id).await {
        Ok(result) => Ok(result),
        Err(_gpu_error) => {
            // Fallback to CPU linear interpolation
            println!("⚠️  GPU linear fallback to CPU");
            
            // Reconstruct time range for CPU fallback
            if target_times.is_empty() {
                return Ok(Vec::new());
            }
            
            let start = target_times[0];
            let end = target_times[target_times.len() - 1];
            
            // Determine resolution based on time spacing
            let resolution = if target_times.len() > 1 {
                let time_diff = target_times[1] - target_times[0];
                if time_diff <= chrono::Duration::microseconds(1) {
                    crate::splines::Resolution::Nanoseconds
                } else if time_diff <= chrono::Duration::milliseconds(1) {
                    crate::splines::Resolution::Microseconds
                } else if time_diff <= chrono::Duration::seconds(1) {
                    crate::splines::Resolution::Milliseconds
                } else if time_diff <= chrono::Duration::minutes(1) {
                    crate::splines::Resolution::Seconds
                } else {
                    crate::splines::Resolution::Minutes
                }
            } else {
                crate::splines::Resolution::Seconds
            };
            
            crate::splines::linear::linear(measurements, start, end, resolution)
        }
    }
}

/// Test GPU availability for benchmarks
///
/// # Errors
///
/// Returns an error if GPU initialization fails
pub async fn test_gpu_availability() -> Result<bool> {
    match GpuLinearInterpolator::new().await {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

