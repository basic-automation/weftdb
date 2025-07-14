use std::{borrow::Cow, collections::HashMap, mem::size_of_val};

use anyhow::{Context, Result, bail};
use bytemuck::cast_slice;
use wgpu::{
	Backends, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BufferBindingType, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, ComputePassDescriptor, ComputePipeline, ComputePipelineDescriptor, Device, DeviceDescriptor, Dx12Compiler, Features, Gles3MinorVersion, Instance, InstanceDescriptor, InstanceFlags, Limits, Maintain, MapMode, PipelineLayoutDescriptor, PowerPreference, Queue, RequestAdapterOptions, ShaderModuleDescriptor, ShaderSource, ShaderStages, util::{BufferInitDescriptor, DeviceExt}
};

use crate::{Error, gpu::Method};

/// GPU accelerated interpolation context
#[derive(Debug)]
pub struct GpuInterpolator {
	device: Device,
	queue: Queue,
	pipelines: HashMap<Method, ComputePipeline>,
	bind_group_layout: BindGroupLayout,
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
		let instance = Instance::new(InstanceDescriptor { backends: Backends::all(), flags: InstanceFlags::default(), dx12_shader_compiler: Dx12Compiler::default(), gles_minor_version: Gles3MinorVersion::Automatic });

		// Get adapter
		let adapter = instance.request_adapter(&RequestAdapterOptions { power_preference: PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false }).await.ok_or(Error::InvalidGpuInputError("No compatible GPU adapter found".to_string()))?;

		// Create device and queue
		let (device, queue) = adapter.request_device(&DeviceDescriptor { label: Some("GPU Interpolator Device"), required_features: Features::empty(), required_limits: Limits::default() }, None).await.context("Failed to create GPU device")?;

		// Create bind group layout
		let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor { label: Some("Interpolation Bind Group Layout"), entries: &[BindGroupLayoutEntry { binding: 0, visibility: ShaderStages::COMPUTE, ty: BindingType::Buffer { ty: BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, BindGroupLayoutEntry { binding: 1, visibility: ShaderStages::COMPUTE, ty: BindingType::Buffer { ty: BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, BindGroupLayoutEntry { binding: 2, visibility: ShaderStages::COMPUTE, ty: BindingType::Buffer { ty: BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None }, count: None }, BindGroupLayoutEntry { binding: 3, visibility: ShaderStages::COMPUTE, ty: BindingType::Buffer { ty: BufferBindingType::Storage { read_only: false }, has_dynamic_offset: false, min_binding_size: None }, count: None }] });

		// Create compute pipelines for each interpolation method
		let mut pipelines = HashMap::new();

		for &method in &[Method::Linear, Method::Quadratic, Method::Cubic] {
			let pipeline = Self::create_compute_pipeline(&device, &bind_group_layout, method);
			pipelines.insert(method, pipeline);
		}

		Ok(Self { device, queue, pipelines, bind_group_layout })
	}

	fn create_compute_pipeline(device: &Device, bind_group_layout: &BindGroupLayout, method: Method) -> ComputePipeline {
		let shader = device.create_shader_module(ShaderModuleDescriptor { label: Some(&format!("{method:?} Interpolation Shader")), source: ShaderSource::Wgsl(Cow::Borrowed(method.shader_source())) });
		let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor { label: Some(&format!("{method:?} Pipeline Layout")), bind_group_layouts: &[bind_group_layout], push_constant_ranges: &[] });
		device.create_compute_pipeline(&ComputePipelineDescriptor { label: Some(&format!("{method:?} Compute Pipeline")), layout: Some(&pipeline_layout), module: &shader, entry_point: "main" })
	}

	/// Perform GPU-accelerated interpolation
	///
	/// # Errors
	///
	/// Returns an error if:
	/// - Buffer creation fails
	/// - GPU computation fails
	/// - Result mapping fails
	pub fn interpolate(&self, input_times: &[f32], input_values: &[f32], target_times: &[f32], method: Method) -> Result<Vec<f32>> {
		if input_times.len() != input_values.len() {
			bail!(Error::InvalidGpuInputError("Input times and values must have the same length".to_string()));
		}

		if target_times.is_empty() {
			bail!(Error::InvalidGpuInputError("Target times cannot be empty".to_string()));
		}

		let pipeline = self.pipelines.get(&method).context("GPU pipeline not found for method")?;

		// Create buffers
		let input_times_buffer = self.device.create_buffer_init(&BufferInitDescriptor { label: Some("Input Times Buffer"), contents: cast_slice(input_times), usage: BufferUsages::STORAGE });

		let input_values_buffer = self.device.create_buffer_init(&BufferInitDescriptor { label: Some("Input Values Buffer"), contents: cast_slice(input_values), usage: BufferUsages::STORAGE });

		let target_times_buffer = self.device.create_buffer_init(&BufferInitDescriptor { label: Some("Target Times Buffer"), contents: cast_slice(target_times), usage: BufferUsages::STORAGE });

		let output_buffer = self.device.create_buffer(&BufferDescriptor { label: Some("Output Buffer"), size: size_of_val(target_times) as u64, usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC, mapped_at_creation: false });

		let staging_buffer = self.device.create_buffer(&BufferDescriptor { label: Some("Staging Buffer"), size: size_of_val(target_times) as u64, usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ, mapped_at_creation: false });

		// Create bind group
		let bind_group = self.device.create_bind_group(&BindGroupDescriptor { label: Some("Interpolation Bind Group"), layout: &self.bind_group_layout, entries: &[BindGroupEntry { binding: 0, resource: input_times_buffer.as_entire_binding() }, BindGroupEntry { binding: 1, resource: input_values_buffer.as_entire_binding() }, BindGroupEntry { binding: 2, resource: target_times_buffer.as_entire_binding() }, BindGroupEntry { binding: 3, resource: output_buffer.as_entire_binding() }] });

		// Execute compute shader
		let mut encoder = self.device.create_command_encoder(&CommandEncoderDescriptor { label: Some("GPU Interpolation Encoder") });

		{
			let mut compute_pass = encoder.begin_compute_pass(&ComputePassDescriptor { label: Some("GPU Interpolation Pass"), timestamp_writes: None });

			compute_pass.set_pipeline(pipeline);
			compute_pass.set_bind_group(0, &bind_group, &[]);

			let workgroup_count = target_times.len().div_ceil(256); // Round up division
			compute_pass.dispatch_workgroups(workgroup_count as u32, 1, 1);
		}

		// Copy result to staging buffer
		encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging_buffer, 0, size_of_val(target_times) as u64);

		self.queue.submit([encoder.finish()]);

		// Read results - Fixed polling for wgpu 0.19
		let buffer_slice = staging_buffer.slice(..);
		let (sender, receiver) = flume::bounded(1);

		buffer_slice.map_async(MapMode::Read, move |result| {
			sender.send(result).ok();
		});

		// Fixed: Use poll with proper wait mode for wgpu 0.19
		self.device.poll(Maintain::Wait);
		receiver.recv()??;

		let data = buffer_slice.get_mapped_range();
		let result: Vec<f32> = cast_slice(&data).to_vec();
		drop(data);
		staging_buffer.unmap();

		Ok(result)
	}
}
