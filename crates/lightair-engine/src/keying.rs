//! Sharpen and the keyers on phones: the desktop's `sharpness_filter_v2`, `chroma_key_filter_v2`,
//! `color_key_filter_v2` and `luma_key_filter_v2` (OBS filter ids, which lightair-render doesn't
//! draw). Drawn here on the engine's GPU, before the scene draws the layer, one pass per effect in
//! the source's order (`gpu/keying.wgsl`).

use std::collections::HashMap;

use lightair_model::{Id, Source};
use serde_json::{Map, Value};

use crate::gpu::Gpu;

pub const KINDS: [&str; 4] =
    ["sharpness_filter_v2", "chroma_key_filter_v2", "color_key_filter_v2", "luma_key_filter_v2"];

/// One effect's pass: (mode, key colour rgb, params).
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub mode: u32,
    pub key: [f32; 3],
    pub b: [f32; 3],
    /// The grade's: d (contrast, saturation, warmth, tint), e (exposure, fade, intensity,
    /// mono), f (the shadows' tint).
    pub more: [f32; 12],
}

fn settings(e: &lightair_model::Effect) -> Map<String, Value> {
    let mut s = Map::new();
    if let Some(d) = lightair_model::catalog::def(&e.kind) {
        s.extend(d.defaults());
    }
    if let Value::Object(own) = &e.settings {
        s.extend(own.clone());
    }
    s
}

/// 0xAABBGGRR, as the desktop stores colours, as rgb 0–1.
fn rgb(abgr: u64) -> [f32; 3] {
    [(abgr & 0xFF) as f32 / 255.0, ((abgr >> 8) & 0xFF) as f32 / 255.0, ((abgr >> 16) & 0xFF) as f32 / 255.0]
}

/// The source's sharpen and keyer passes, in order (the ones switched on).
pub fn steps(source: &Source) -> Vec<Step> {
    source
        .effects
        .iter()
        .filter(|e| e.enabled && KINDS.contains(&e.kind.as_str()))
        .map(|e| {
            let s = settings(e);
            let num = |k: &str, d: f64| s.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
            let key = s.get("key_color").and_then(Value::as_u64).unwrap_or(0xFF00FF00);
            match e.kind.as_str() {
                "sharpness_filter_v2" => Step {
                    mode: 1,
                    key: [0.0; 3],
                    b: [num("sharpness", 0.08).clamp(0.0, 1.0), 0.0, 0.0],
                    more: [0.0; 12],
                },
                "chroma_key_filter_v2" => {
                    let key = match s.get("key_color_type").and_then(Value::as_str) {
                        Some("blue") => [0.0, 0.0, 1.0],
                        Some("magenta") => [1.0, 0.0, 1.0],
                        Some("custom") => rgb(key),
                        _ => [0.0, 1.0, 0.0],
                    };
                    Step {
                        more: [0.0; 12],
                        mode: 2,
                        key,
                        // the desktop's 1–1000 scales, as fractions
                        b: [
                            num("similarity", 400.0) / 1000.0 * 0.4,
                            num("smoothness", 80.0) / 1000.0 * 0.4,
                            num("spill", 100.0) / 1000.0 * 0.4,
                        ],
                    }
                }
                "color_key_filter_v2" => Step {
                    more: [0.0; 12],
                    mode: 3,
                    key: rgb(key),
                    b: [num("similarity", 80.0) / 1000.0, num("smoothness", 50.0) / 1000.0, 0.0],
                },
                _ => Step {
                    more: [0.0; 12],
                    mode: 4,
                    key: [0.0; 3],
                    b: [num("luma_min", 0.0), num("luma_max", 1.0), num("luma_min_smooth", 0.02).max(0.002)],
                },
            }
        })
        .collect()
}

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub struct Pipeline {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    /// A 1×1 mask for passes that use none.
    white: wgpu::Texture,
}

/// The person mask a light or the skin smoothing reads: its texture, quarter turns, mirrored.
pub struct MaskIn<'a> {
    pub texture: &'a wgpu::Texture,
    pub rotation: u32,
    pub mirrored: bool,
}

/// Each source's person mask as a texture (uploaded again only when it's a new one).
#[derive(Default)]
pub struct Masks(HashMap<Id, (u64, wgpu::Texture)>);

impl Masks {
    pub fn forget_others(&mut self, keep: &[Id]) {
        self.0.retain(|id, _| keep.contains(id));
    }

    pub fn texture(&mut self, gpu: &Gpu, source: Id, mask: &crate::feeds::Mask) -> wgpu::Texture {
        let fresh = !matches!(self.0.get(&source), Some((serial, t)) if *serial == mask.serial && (t.width(), t.height()) == mask.size);
        if fresh {
            let t = gpu.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("person mask (looks)"),
                size: wgpu::Extent3d {
                    width: mask.size.0.max(1),
                    height: mask.size.1.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            gpu.queue().write_texture(
                t.as_image_copy(),
                &mask.bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(mask.size.0),
                    rows_per_image: Some(mask.size.1),
                },
                wgpu::Extent3d { width: mask.size.0, height: mask.size.1, depth_or_array_layers: 1 },
            );
            self.0.insert(source, (mask.serial, t));
        }
        self.0[&source].1.clone()
    }
}

/// Each source's two targets (passes go back and forth between them).
#[derive(Default)]
pub struct Targets(HashMap<Id, (wgpu::Texture, wgpu::Texture)>);

impl Targets {
    pub fn forget_others(&mut self, keep: &[Id]) {
        self.0.retain(|id, _| keep.contains(id));
    }
}

impl Pipeline {
    pub fn new(device: &wgpu::Device) -> Pipeline {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("keying"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/keying.wgsl").into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("keying"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(2, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                entry(
                    3,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("keying"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let white = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("no mask"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Pipeline { layout, pipeline, sampler, white }
    }
}

fn target(gpu: &Gpu, size: (u32, u32)) -> wgpu::Texture {
    gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("keying"),
        size: wgpu::Extent3d { width: size.0.max(1), height: size.1.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// `picture` (premultiplied or not) through `steps`, as a new straight-alpha texture of its size.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    gpu: &Gpu,
    p: &Pipeline,
    targets: &mut Targets,
    source: Id,
    picture: &wgpu::Texture,
    premultiplied: bool,
    steps: &[Step],
    mask: Option<MaskIn>,
) -> wgpu::Texture {
    let size = (picture.width(), picture.height());
    gpu.queue().write_texture(
        p.white.as_image_copy(),
        &[255],
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(1), rows_per_image: Some(1) },
        wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
    );
    let (mask_t, rotation, mirrored, has_mask) = match &mask {
        Some(m) => (m.texture, m.rotation, m.mirrored, true),
        None => (&p.white, 0, false, false),
    };
    let mv = mask_t.create_view(&Default::default());
    let fresh = !matches!(targets.0.get(&source), Some((a, _)) if (a.width(), a.height()) == size);
    if fresh {
        targets.0.insert(source, (target(gpu, size), target(gpu, size)));
    }
    let (a, b) = targets.0.get(&source).cloned().expect("just made");
    let mut encoder = gpu.device().create_command_encoder(&Default::default());
    let mut from = picture.clone();
    let mut to = a;
    let mut spare = b;
    for (n, s) in steps.iter().enumerate() {
        let mut params = [0f32; 28];
        params[..12].copy_from_slice(&[
            s.mode as f32,
            if n == 0 && premultiplied { 1.0 } else { 0.0 },
            1.0 / size.0.max(1) as f32,
            1.0 / size.1.max(1) as f32,
            s.key[0],
            s.key[1],
            s.key[2],
            1.0,
            s.b[0],
            s.b[1],
            s.b[2],
            0.0,
        ]);
        params[12..24].copy_from_slice(&s.more);
        params[24..].copy_from_slice(&[
            rotation as f32,
            if mirrored { 1.0 } else { 0.0 },
            if has_mask { 1.0 } else { 0.0 },
            0.0,
        ]);
        let bytes: Vec<u8> = params.iter().flat_map(|v| v.to_le_bytes()).collect();
        let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue().write_buffer(&buffer, 0, &bytes);
        let (sv, dv) = (from.create_view(&Default::default()), to.create_view(&Default::default()));
        let group = gpu.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &p.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&sv) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&p.sampler) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&mv) },
            ],
        });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("keying"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dv,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&p.pipeline);
            rp.set_bind_group(0, &group, &[]);
            rp.draw(0..3, 0..1);
        }
        from = to.clone();
        std::mem::swap(&mut to, &mut spare);
    }
    gpu.queue().submit([encoder.finish()]);
    from
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_effect_reads_its_settings_as_the_desktop_writes_them() {
        let mut s = Source::new("Cam", "lightair_camera", serde_json::json!({}));
        let mut chroma = lightair_model::Effect::new(
            "Chroma key",
            "chroma_key_filter_v2",
            serde_json::json!({ "key_color_type": "blue", "similarity": 500 }),
        );
        chroma.enabled = true;
        s.effects.push(chroma);
        s.effects.push(lightair_model::Effect::new(
            "Colour key",
            "color_key_filter_v2",
            serde_json::json!({ "key_color": 0xFF0000FFu32 }),
        ));
        let mut off = lightair_model::Effect::new("Off", "sharpness_filter_v2", Value::Null);
        off.enabled = false;
        s.effects.push(off);
        let st = steps(&s);
        assert_eq!(st.len(), 2, "the switched-off one is left out");
        assert_eq!((st[0].mode, st[0].key), (2, [0.0, 0.0, 1.0]));
        assert!((st[0].b[0] - 0.2).abs() < 1e-6);
        assert_eq!((st[1].mode, st[1].key), (3, [1.0, 0.0, 0.0]), "0xAABBGGRR");
    }
}
