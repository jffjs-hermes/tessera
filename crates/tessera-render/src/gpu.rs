use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::atlas::{ATLAS_COLS, ATLAS_H, ATLAS_ROWS, ATLAS_W, Atlas, GLYPH_PX};

/// One grid cell, already resolved to atlas slot + colors.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Cell {
    pub x: u32,
    pub y: u32,
    pub glyph: u32,
    pub fg: [f32; 4],
    pub bg: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    grid: [u32; 2],
    cell_px: [f32; 2],
    screen: [f32; 2],
    atlas_cols: u32,
    atlas_rows: u32,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    inst_buf: wgpu::Buffer,
    inst_capacity: usize,
    atlas_tex: wgpu::Texture,
    grid: (u32, u32),
    scale: u32,
}

impl Renderer {
    /// `grid` is the number of cells; `scale` is the integer pixel scale of each 8px glyph.
    pub fn new(
        event_loop: &winit::event_loop::ActiveEventLoop,
        window: Arc<Window>,
        grid: (u32, u32),
        scale: u32,
    ) -> anyhow_free::Result<Self> {
        let size = window.inner_size();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
                Box::new(event_loop.owned_display_handle()),
            ));
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))?;
        tracing::info!(adapter = ?adapter.get_info().name, backend = ?adapter.get_info().backend, "gpu adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&device, &config);

        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_tex.create_view(&Default::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::bytes_of(&Globals::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let inst_capacity = (grid.0 * grid.1) as usize;
        let inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cells"),
            size: (inst_capacity * std::mem::size_of::<Cell>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let attrs =
            wgpu::vertex_attr_array![0 => Uint32x2, 1 => Uint32, 2 => Float32x4, 3 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cells"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Cell>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });

        let mut r = Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            globals_buf,
            inst_buf,
            inst_capacity,
            atlas_tex,
            grid,
            scale,
        };
        r.write_globals();
        Ok(r)
    }

    /// Window size in physical pixels needed for the grid at the current scale.
    pub fn ideal_size(&self) -> (u32, u32) {
        (
            self.grid.0 * GLYPH_PX * self.scale,
            self.grid.1 * GLYPH_PX * self.scale,
        )
    }

    /// The wgpu device the renderer drives. Exposed so the devtools egui layer
    /// can share the same device/queue/surface instead of creating its own.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue the renderer submits on.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The surface color format the cells render into; egui must use the same
    /// format when it paints on top of the same frame.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn upload_atlas(&self, atlas: &Atlas) {
        self.queue.write_texture(
            self.atlas_tex.as_image_copy(),
            &atlas.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_W),
                rows_per_image: Some(ATLAS_H),
            },
            wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.write_globals();
    }

    fn write_globals(&mut self) {
        // Largest integer scale that fits the window, so glyphs stay crisp.
        let fit = (self.config.width / (self.grid.0 * GLYPH_PX))
            .min(self.config.height / (self.grid.1 * GLYPH_PX))
            .max(1);
        self.scale = fit;
        let px = (GLYPH_PX * fit) as f32;
        let g = Globals {
            grid: [self.grid.0, self.grid.1],
            cell_px: [px, px],
            screen: [self.config.width as f32, self.config.height as f32],
            atlas_cols: ATLAS_COLS,
            atlas_rows: ATLAS_ROWS,
        };
        self.queue
            .write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));
    }

    pub fn render(&mut self, cells: &[Cell]) {
        self.render_with(cells, |_, _, _, _| Vec::new());
    }

    /// Render the cell grid, then let `overlay` draw into the same surface
    /// frame before it is presented. `overlay` receives the device, queue, the
    /// current frame's texture view, and the shared command encoder, and may
    /// append its own work (e.g. the egui devtools pass) to the encoder; any
    /// command buffers it returns (e.g. egui's per-frame user buffers) are
    /// submitted before the encoded frame. The cells always clear the frame
    /// first; an overlay that wants to composite on top must use
    /// `LoadOp::Load` for its own pass.
    pub fn render_with(
        &mut self,
        cells: &[Cell],
        overlay: impl FnOnce(
            &wgpu::Device,
            &wgpu::Queue,
            &wgpu::TextureView,
            &mut wgpu::CommandEncoder,
        ) -> Vec<wgpu::CommandBuffer>,
    ) {
        let cells = &cells[..cells.len().min(self.inst_capacity)];
        self.queue
            .write_buffer(&self.inst_buf, 0, bytemuck::cast_slice(cells));
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.inst_buf.slice(..));
            pass.draw(0..6, 0..cells.len() as u32);
        }
        let extra = overlay(&self.device, &self.queue, &view, &mut enc);
        let encoded = enc.finish();
        self.queue.submit(extra.into_iter().chain([encoded]));
        self.queue.present(frame);
    }
}

/// Tiny local alias so this crate needs no error-handling dependency.
pub mod anyhow_free {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
