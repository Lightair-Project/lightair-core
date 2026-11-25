//! The engine's own GPU work, on lightair-render's shared device: the
//! program texture drawn onto the preview and the encoders, YUV feeds converted to RGBA, and the
//! platforms' targets.

use std::collections::HashMap;

use lightair_render::SharedGpu;

#[cfg(target_os = "android")]
pub mod android;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
pub mod spirv;
pub mod window;

use crate::feeds::Yuv;

pub struct Gpu {
    pub shared: SharedGpu,
    sampler: wgpu::Sampler,
    blit_module: wgpu::ShaderModule,
    blit_layout: wgpu::BindGroupLayout,
    blits: parking_lot::Mutex<HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>>,
    yuv_layout: wgpu::BindGroupLayout,
    yuv: wgpu::RenderPipeline,
    /// Android with the Vulkan extensions it needs: the camera's buffers imported zero-copy.
    #[cfg(target_os = "android")]
    pub importer: Option<parking_lot::Mutex<android::Importer>>,
}

/// Whether camera frames come in zero-copy (Android: decided once, before the device exists).
pub fn zero_copy() -> bool {
    #[cfg(target_os = "android")]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(android::make_device)
    }
    #[cfg(not(target_os = "android"))]
    false
}

fn entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::VERTEX_FRAGMENT, ty, count: None }
}

fn uniform() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

fn tex() -> wgpu::BindingType {
    wgpu::BindingType::Texture {
        sample_type: wgpu::TextureSampleType::Float { filterable: true },
        view_dimension: wgpu::TextureViewDimension::D2,
        multisampled: false,
    }
}

fn pipeline(
    device: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lightair-engine"),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// (Kr-ish coefficients) for BT.601, BT.709, BT.2020: R = Y + a·Cr, G = Y − b·Cb − c·Cr, B = Y + d·Cb.
fn coefficients(matrix: u32) -> [f32; 4] {
    match matrix {
        1 => [1.5748, 0.187_324, 0.468_124, 1.8556],
        2 => [1.4746, 0.164_553, 0.571_353, 1.8814],
        _ => [1.402, 0.344_136, 0.714_136, 1.772],
    }
}

impl Gpu {
    pub fn new() -> Result<Gpu, String> {
        // Android: the device with the zero-copy import's extensions, if it can be had
        let zero_copy = zero_copy();
        let _ = zero_copy;
        let shared = lightair_render::shared_gpu()?;
        #[cfg(target_os = "android")]
        let importer = zero_copy.then(|| android::Importer::new(&shared.device)).flatten().map(parking_lot::Mutex::new);
        let device = &shared.device;
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let blit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        let filtering = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[entry(0, uniform()), entry(1, tex()), entry(2, filtering)],
        });
        let yuv_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(include_str!("yuv.wgsl").into()),
        });
        let yuv_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[entry(0, uniform()), entry(1, tex()), entry(2, tex()), entry(3, filtering)],
        });
        let yuv = pipeline(device, &yuv_module, &yuv_layout, wgpu::TextureFormat::Rgba8Unorm);
        Ok(Gpu {
            shared,
            sampler,
            blit_module,
            blit_layout,
            blits: parking_lot::Mutex::new(HashMap::new()),
            yuv_layout,
            yuv,
            #[cfg(target_os = "android")]
            importer,
        })
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.shared.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.shared.queue
    }

    fn uniform(&self, values: &[f32]) -> wgpu::Buffer {
        let buffer = self.shared.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (values.len() * 4).max(16) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.shared.queue.write_buffer(&buffer, 0, &bytes);
        buffer
    }

    /// Draws `src` (sampled as linear light) over all of `dst`, in `encoder`.
    pub fn blit(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::Texture,
        dst: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) {
        let device = self.shared.device.clone();
        let pipe = self
            .blits
            .lock()
            .entry(format)
            .or_insert_with(|| pipeline(&device, &self.blit_module, &self.blit_layout, format))
            .clone();
        let encode = if format.is_srgb() { 0.0 } else { 1.0 };
        let u = self.uniform(&[encode, 0.0, 0.0, 0.0]);
        let view = src.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipe);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// A YUV frame as an upright RGBA texture (`out` reused when its size fits).
    pub fn yuv_to_rgba(&self, f: &Yuv, out: Option<wgpu::Texture>) -> wgpu::Texture {
        let device = &self.shared.device;
        let queue = &self.shared.queue;
        let (w, h) = (f.width & !1, f.height & !1);
        let turned = f.rotation % 180 == 90;
        let (ow, oh) = if turned { (h, w) } else { (w, h) };
        let plane = |format, size: (u32, u32), data: &[u8], bpp: u32| {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                t.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.0 * bpp),
                    rows_per_image: Some(size.1),
                },
                wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            );
            t
        };
        let luma = plane(wgpu::TextureFormat::R8Unorm, (w, h), &f.y, 1);
        let chroma = plane(wgpu::TextureFormat::Rg8Unorm, (w / 2, h / 2), &f.uv, 2);
        let out = out.filter(|t| (t.width(), t.height()) == (ow, oh)).unwrap_or_else(|| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("feed"),
                size: wgpu::Extent3d { width: ow, height: oh, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        });
        let k = coefficients(f.matrix);
        let u = self.uniform(&[
            k[0],
            k[1],
            k[2],
            k[3],
            if f.full_range { 0.0 } else { 1.0 },
            (f.rotation / 90) as f32,
            if f.mirrored { 1.0 } else { 0.0 },
            0.0,
        ]);
        let (lv, cv, ov) = (
            luma.create_view(&Default::default()),
            chroma.create_view(&Default::default()),
            out.create_view(&Default::default()),
        );
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.yuv_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&lv) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&cv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &ov,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.yuv);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
        out
    }

    /// An RGBA texture from bytes (images, GIF frames, made pictures).
    pub fn texture(&self, rgba: &[u8], size: (u32, u32)) -> wgpu::Texture {
        texture_on(&self.shared.device, &self.shared.queue, rgba, size)
    }

    /// A texture made to be drawn into outside wgpu (Android's camera import): cleared and then
    /// sampled once through wgpu, so wgpu tracks it as an initialised, sampled texture.
    pub fn prime(&self, t: &wgpu::Texture) {
        let device = &self.shared.device;
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let view = t.create_view(&Default::default());
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("prime"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        // sampled once (the YUV pass reads any float texture): wgpu tracks it as sampled
        let scratch = self.target((1, 1), wgpu::TextureFormat::Rgba8Unorm);
        let out = scratch.create_view(&Default::default());
        let tv = t.create_view(&Default::default());
        let u = self.uniform(&[0.0; 8]);
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.yuv_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: u.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&tv) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&tv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("prime"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.yuv);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.shared.queue.submit([encoder.finish()]);
    }

    /// New RGBA bytes into a texture of their size.
    pub fn write(&self, t: &wgpu::Texture, rgba: &[u8]) {
        let (w, h) = (t.width(), t.height());
        self.shared.queue.write_texture(
            t.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// A render target the engine owns (tests' encoders, the camera clean frame).
    pub fn target(&self, size: (u32, u32), format: wgpu::TextureFormat) -> wgpu::Texture {
        self.shared.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// Waits until the GPU has done everything submitted.
    pub fn wait(&self) {
        let _ = self.shared.device.poll(wgpu::PollType::wait_indefinitely());
    }
}

/// An RGBA texture from bytes, on any thread.
pub fn texture_on(device: &wgpu::Device, queue: &wgpu::Queue, rgba: &[u8], size: (u32, u32)) -> wgpu::Texture {
    let t = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("picture"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    queue.write_texture(
        t.as_image_copy(),
        rgba,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size.0 * 4), rows_per_image: Some(size.1) },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
    t
}
