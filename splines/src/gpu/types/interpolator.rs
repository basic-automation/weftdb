use std::{borrow::Cow, collections::HashMap, mem::size_of_val};

use anyhow::{Context, Result, bail};
use bytemuck::cast_slice;
use wgpu::{
    Backends, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BufferBindingType, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, ComputePassDescriptor, ComputePipeline, ComputePipelineDescriptor, Device, DeviceDescriptor, Dx12Compiler, Features, Gles3MinorVersion, Instance, InstanceDescriptor, InstanceFlags, Limits, PipelineLayoutDescriptor, PowerPreference, Queue, RequestAdapterOptions, ShaderModuleDescriptor, ShaderSource, ShaderStages
};
use wgpu::util::{BufferInitDescriptor, DeviceExt};

use crate::{gpu::Method};

/// GPU accelerated interpolation context
#[derive(Debug)]
pub struct GpuInterpolator {
    device: Device,
    queue: Queue,
    pipelines: HashMap<String, ComputePipeline>,
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
        // Create WGPU instance
        let instance = Instance::new(InstanceDescriptor {
            backends: Backends::all(),
            flags: InstanceFlags::default(),
            dx12_shader_compiler: Dx12Compiler::Fxc,
            gles_minor_version: Gles3MinorVersion::Automatic,
        });

        // Request adapter
        let adapter = instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }).await.context("Failed to find an appropriate adapter")?;

        // Request device - remove memory_hints field
        let (device, queue) = adapter.request_device(&DeviceDescriptor {
            label: None,
            required_features: Features::empty(),
            required_limits: Limits::default(),
        }, None).await.context("Failed to create device")?;

        // Create bind group layout
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("Interpolation Bind Group Layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::COMPUTE,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::COMPUTE,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::COMPUTE,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 3,
                    visibility: ShaderStages::COMPUTE,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 4,
                    visibility: ShaderStages::COMPUTE,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        // Create pipelines for basic methods
        let mut pipelines = HashMap::new();

        // Add basic methods
        for &method in &[Method::Linear, Method::Quadratic, Method::Cubic] {
            let pipeline = Self::create_compute_pipeline(&device, &bind_group_layout, method);
            let key = Self::method_to_key(method);
            pipelines.insert(key, pipeline);
        }

        Ok(Self { device, queue, pipelines, bind_group_layout })
    }

    fn method_to_key(method: Method) -> String {
        match method {
            Method::Linear => "linear".to_string(),
            Method::Quadratic => "quadratic".to_string(),
            Method::Cubic => "cubic".to_string(),
            Method::Polynomial(degree) => format!("polynomial_{}", degree),
        }
    }

    fn create_compute_pipeline(device: &Device, bind_group_layout: &BindGroupLayout, method: Method) -> ComputePipeline {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some(&format!("{method:?} Interpolation Shader")),
            source: ShaderSource::Wgsl(Cow::Borrowed(method.shader_source())),
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some(&format!("{method:?} Pipeline Layout")),
            bind_group_layouts: &[bind_group_layout],
            push_constant_ranges: &[],
        });
        device.create_compute_pipeline(&ComputePipelineDescriptor {
            label: Some(&format!("{method:?} Compute Pipeline")),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: "main",
        })
    }

    /// Perform GPU-accelerated interpolation with configurable bounds
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Buffer creation fails
    /// - GPU computation fails
    /// - Result mapping fails
    pub fn interpolate(&mut self, input_times: &[f32], input_values: &[f32], target_times: &[f32], method: Method, bounds_factor: Option<f64>) -> Result<Vec<f32>> {
        if input_times.len() != input_values.len() {
            bail!("Input times and values must have the same length");
        }

        if target_times.is_empty() {
            bail!("Target times cannot be empty");
        }

        // Get or create pipeline first and get an owned reference
        let pipeline = {
            let key = Self::method_to_key(method);
            if !self.pipelines.contains_key(&key) {
                let pipeline = Self::create_compute_pipeline(&self.device, &self.bind_group_layout, method);
                self.pipelines.insert(key.clone(), pipeline);
            }
            // Get an owned reference to avoid borrow issues
            self.pipelines.get(&key).unwrap() as *const ComputePipeline
        };

        // Now we can safely use self.device
        let pipeline = unsafe { &*pipeline };

        // Create buffers
        let input_times_buffer = self.device.create_buffer_init(&BufferInitDescriptor {
            label: Some("Input Times Buffer"),
            contents: cast_slice(input_times),
            usage: BufferUsages::STORAGE,
        });

        let input_values_buffer = self.device.create_buffer_init(&BufferInitDescriptor {
            label: Some("Input Values Buffer"),
            contents: cast_slice(input_values),
            usage: BufferUsages::STORAGE,
        });

        let target_times_buffer = self.device.create_buffer_init(&BufferInitDescriptor {
            label: Some("Target Times Buffer"),
            contents: cast_slice(target_times),
            usage: BufferUsages::STORAGE,
        });

        let output_buffer = self.device.create_buffer(&BufferDescriptor {
            label: Some("Output Buffer"),
            size: (target_times.len() * size_of_val(&0f32)) as u64,
            usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        // Create uniform buffer based on method and bounds
        let uniform_data = match method {
            Method::Polynomial(degree) => {
                // For polynomial method, pass [offset, degree, bounds_factor_bits]
                let bounds_bits = match bounds_factor {
                    Some(factor) => factor.to_bits(),
                    None => f64::NAN.to_bits(), // Use NaN to represent unbounded
                };
                [0u32, degree as u32, bounds_bits as u32]
            }
            _ => {
                // For other methods, pass [0, 0, bounds_factor_bits]
                let bounds_bits = match bounds_factor {
                    Some(factor) => factor.to_bits(),
                    None => f64::NAN.to_bits(), // Use NaN to represent unbounded
                };
                [0u32, 0u32, bounds_bits as u32]
            }
        };

        let uniform_buffer = self.device.create_buffer_init(&BufferInitDescriptor {
            label: Some("Uniform Buffer"),
            contents: cast_slice(&uniform_data),
            usage: BufferUsages::UNIFORM,
        });

        // Create bind group
        let bind_group = self.device.create_bind_group(&BindGroupDescriptor {
            label: Some("Interpolation Bind Group"),
            layout: &self.bind_group_layout,
            entries: &[
                BindGroupEntry { binding: 0, resource: input_times_buffer.as_entire_binding() },
                BindGroupEntry { binding: 1, resource: input_values_buffer.as_entire_binding() },
                BindGroupEntry { binding: 2, resource: target_times_buffer.as_entire_binding() },
                BindGroupEntry { binding: 3, resource: output_buffer.as_entire_binding() },
                BindGroupEntry { binding: 4, resource: uniform_buffer.as_entire_binding() },
            ],
        });

        // Execute compute shader
        let mut encoder = self.device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("Compute Encoder"),
        });
        
        {
            let mut compute_pass = encoder.begin_compute_pass(&ComputePassDescriptor {
                label: Some("Interpolation Compute Pass"),
                timestamp_writes: None,
            });
            compute_pass.set_pipeline(pipeline);
            compute_pass.set_bind_group(0, &bind_group, &[]);
            compute_pass.dispatch_workgroups((target_times.len() as u32 + 255) / 256, 1, 1);
        }

        // Copy output buffer to staging buffer
        let staging_buffer = self.device.create_buffer(&BufferDescriptor {
            label: Some("Staging Buffer"),
            size: (target_times.len() * size_of_val(&0f32)) as u64,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging_buffer, 0, staging_buffer.size());

        self.queue.submit(std::iter::once(encoder.finish()));

        // Read results using async block
        let buffer_slice = staging_buffer.slice(..);
        
        // Use a simpler approach with pollster for blocking
        let result = pollster::block_on(async {
            let (sender, receiver) = futures_channel::oneshot::channel();
            buffer_slice.map_async(wgpu::MapMode::Read, move |v| {
                sender.send(v).unwrap();
            });

            self.device.poll(wgpu::Maintain::wait()).panic_on_timeout();
            receiver.await.unwrap()
        });

        match result {
            Ok(()) => {
                let data = buffer_slice.get_mapped_range();
                let result: Vec<f32> = cast_slice(&data).to_vec();
                drop(data);
                staging_buffer.unmap();
                Ok(result)
            }
            Err(e) => {
                bail!("Failed to read GPU results: {:?}", e);
            }
        }
    }
}
