//! wgpu backend.
//!
//! The same WGSL runs on Metal here, Vulkan on the Void host and D3D12 on the
//! Windows host, which is the only reason a GPU path is worth having across
//! three vantage points at all.
//!
//! This is **not** on the proxy's hot path, and that is a decision rather than
//! an omission: for HTTP-sized objects the kernel launch and the host-device
//! transfer dominate, BLAKE3 already does 1–3 GB/s on one core, and the TLS
//! work is handled by the CPU's crypto extensions. A shader there would be
//! measurably slower. Where it pays is many image pairs at once, which is
//! exactly what comparing a page across eight browser builds is.

use crate::cpu;
use crate::image::Luma;
use anyhow::{anyhow, Context, Result};
use wgpu::util::DeviceExt;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    pub adapter_name: String,
    pub backend: String,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    width: u32,
    height: u32,
    windows_x: u32,
    windows_y: u32,
    window: u32,
    _pad: [u32; 3],
}

impl Gpu {
    /// Acquire an adapter. Returns an error rather than panicking when there
    /// is none: a headless CI box and the Void host may legitimately have no
    /// usable adapter, and the CPU path is the answer there.
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|e| anyhow!("no usable GPU adapter: {e}"))?;

        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("lwdbg-parity"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .context("requesting a GPU device")?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ssim"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/ssim.wgsl").into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ssim-layout"),
            entries: &[
                storage_entry(0, true),
                storage_entry(1, true),
                storage_entry(2, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ssim-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ssim-pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            layout,
            adapter_name: info.name,
            backend: format!("{:?}", info.backend),
        })
    }

    pub fn ssim(&self, a: &Luma, b: &Luma) -> Result<f64> {
        let (a, b) = Luma::common_crop(a, b)?;
        let wx = a.width / cpu::WINDOW;
        let wy = a.height / cpu::WINDOW;
        if wx == 0 || wy == 0 {
            // Below one window there is nothing to parallelise.
            return cpu::ssim(&a, &b);
        }

        let fa = a.to_f32();
        let fb = b.to_f32();
        let buf_a = self.storage(&fa, "a");
        let buf_b = self.storage(&fb, "b");
        let n_out = wx * wy;
        let out_size = (n_out * std::mem::size_of::<f32>()) as u64;

        let buf_out = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ssim-out"),
            size: out_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ssim-readback"),
            size: out_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let params = Params {
            width: a.width as u32,
            height: a.height as u32,
            windows_x: wx as u32,
            windows_y: wy as u32,
            window: cpu::WINDOW as u32,
            _pad: [0; 3],
        };
        let buf_params = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("ssim-params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ssim-bind"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buf_a.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buf_b.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: buf_out.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: buf_params.as_entire_binding(),
                },
            ],
        });

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ssim-encoder"),
            });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ssim-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            // Workgroup is 8x8, so round the window grid up to it.
            pass.dispatch_workgroups(wx.div_ceil(8) as u32, wy.div_ceil(8) as u32, 1);
        }
        enc.copy_buffer_to_buffer(&buf_out, 0, &readback, 0, out_size);
        self.queue.submit(Some(enc.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        // Block until the submission has executed and the map callback has
        // run. Without the wait, `rx.recv()` would hang on a backend that
        // only invokes callbacks from a poll.
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        rx.recv()
            .context("GPU readback channel closed")?
            .context("mapping the readback buffer")?;

        let data = slice.get_mapped_range();
        let scores: &[f32] = bytemuck::cast_slice(&data);
        let sum: f64 = scores.iter().map(|&v| v as f64).sum();
        let mean = sum / n_out as f64;
        drop(data);
        readback.unmap();
        Ok(mean)
    }

    fn storage(&self, data: &[f32], label: &str) -> wgpu::Buffer {
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents: bytemuck::cast_slice(data),
                usage: wgpu::BufferUsages::STORAGE,
            })
    }
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}
