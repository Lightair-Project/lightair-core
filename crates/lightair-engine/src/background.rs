//! Background blur, removal and replace on a camera: the desktop's
//! `studio.bg_blur`, `studio.bg_remove` and `studio.bg_replace` effects drawn on the phone.
//! The host segments the camera (Vision, MediaPipe) when the engine asks
//! (`la_studio_mask_wanted`) and pushes a person mask (`la_studio_push_mask`); the engine
//! composites camera, mask and what goes behind on its own GPU before the scene draws the layer,
//! so lightair-render needs no mask input.

use lightair_model::Source;
use serde_json::Value;

use crate::feeds::Mask;
use crate::gpu::Gpu;

/// What a camera's background effect asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    /// 1 blur, 2 remove (what's below shows through), 3 a colour, 4 an image.
    pub mode: u32,
    /// 0–100.
    pub blur: f32,
    pub feather: f32,
    /// −100–100: the edge drawn in (−) or out (+).
    pub shift: f32,
    /// 0xAABBGGRR, as the desktop stores colours.
    pub colour: u32,
    pub image: Option<String>,
    /// Vision's levels: 0 fast, 1 balanced, 2 accurate.
    pub quality: u32,
}

pub const KINDS: [&str; 3] = ["studio.bg_blur", "studio.bg_remove", "studio.bg_replace"];

/// The source's background effect (the last switched on), if it has one.
pub fn look(source: &Source) -> Option<Look> {
    let e = source
        .effects
        .iter()
        .rev()
        .find(|e| e.enabled && KINDS.contains(&e.kind.as_str()))?;
    let d = lightair_model::catalog::def(&e.kind)?;
    let mut s = serde_json::Map::new();
    s.extend(d.defaults());
    if let Value::Object(own) = &e.settings {
        s.extend(own.clone());
    }
    let num = |k: &str, default: f64| s.get(k).and_then(Value::as_f64).unwrap_or(default) as f32;
    let mode = match e.kind.as_str() {
        "studio.bg_blur" => 1,
        "studio.bg_remove" => 2,
        _ => match s.get("bg_mode").and_then(Value::as_i64) {
            Some(4) => 4,
            _ => 3,
        },
    };
    let image = s
        .get("bg_image")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())
        .map(String::from);
    Some(Look {
        // an image not chosen yet: its colour
        mode: if mode == 4 && image.is_none() {
            3
        } else {
            mode
        },
        blur: num("bg_blur", 60.0),
        feather: num("bg_feather", 30.0),
        shift: num("bg_shift", 0.0),
        colour: s
            .get("bg_color")
            .and_then(Value::as_u64)
            .unwrap_or(0xFF1A_1A1A) as u32,
        image,
        quality: s
            .get("bg_quality")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .min(2) as u32,
    })
}

/// The mask's threshold and softness for an edge shift and feather (both as the desktop's).
pub fn edge(shift: f32, feather: f32) -> (f32, f32) {
    let threshold = (0.5 - shift.clamp(-100.0, 100.0) * 0.004).clamp(0.05, 0.95);
    let softness = 0.01 + feather.clamp(0.0, 100.0) / 100.0 * 0.25;
    (threshold, softness)
}

/// The cover scale of a picture of `image` size behind one of `out` size (centred, cropped).
pub fn cover(out: (u32, u32), image: (u32, u32)) -> (f32, f32) {
    let ao = out.0 as f32 / out.1.max(1) as f32;
    let ai = image.0 as f32 / image.1.max(1) as f32;
    if ao > ai {
        (1.0, ai / ao)
    } else {
        (ao / ai, 1.0)
    }
}

pub struct Pipelines {
    layout: wgpu::BindGroupLayout,
    down: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    composite: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
}

/// A camera's textures for its background (kept between frames).
#[derive(Default)]
pub struct State {
    mask: Option<(u64, wgpu::Texture)>,
    down: Option<(wgpu::Texture, wgpu::Texture)>,
    out: Option<wgpu::Texture>,
}

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Pipelines {
    pub fn new(device: &wgpu::Device) -> Pipelines {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("background"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/background.wgsl").into()),
        });
        let tex = || wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("background"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(1, tex()),
                entry(
                    2,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
                entry(3, tex()),
                entry(4, tex()),
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |fs: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("background"),
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
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Pipelines {
            down: pipeline("fs_down"),
            blur: pipeline("fs_blur"),
            composite: pipeline("fs_composite"),
            layout,
            sampler,
        }
    }
}

fn target(gpu: &Gpu, size: (u32, u32)) -> wgpu::Texture {
    gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("background"),
        size: wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn size(t: &wgpu::Texture) -> (u32, u32) {
    (t.width(), t.height())
}

/// The mask as a texture (uploaded again only when it's a new one).
fn mask_texture<'a>(gpu: &Gpu, st: &'a mut State, mask: &Mask) -> &'a wgpu::Texture {
    let fresh =
        !matches!(&st.mask, Some((serial, t)) if *serial == mask.serial && size(t) == mask.size);
    if fresh {
        let reuse = st
            .mask
            .take()
            .map(|(_, t)| t)
            .filter(|t| size(t) == mask.size);
        let t = reuse.unwrap_or_else(|| {
            gpu.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("person mask"),
                size: wgpu::Extent3d {
                    width: mask.size.0,
                    height: mask.size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        });
        gpu.queue().write_texture(
            t.as_image_copy(),
            &mask.bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(mask.size.0),
                rows_per_image: Some(mask.size.1),
            },
            wgpu::Extent3d {
                width: mask.size.0,
                height: mask.size.1,
                depth_or_array_layers: 1,
            },
        );
        st.mask = Some((mask.serial, t));
    }
    &st.mask.as_ref().expect("just set").1
}

/// One full-target pass of `pipeline`.
#[allow(clippy::too_many_arguments)]
fn pass(
    gpu: &Gpu,
    p: &Pipelines,
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    params: [f32; 16],
    src: &wgpu::Texture,
    mask: &wgpu::Texture,
    behind: &wgpu::Texture,
    dst: &wgpu::Texture,
) {
    let bytes: Vec<u8> = params.iter().flat_map(|v| v.to_le_bytes()).collect();
    let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    gpu.queue().write_buffer(&buffer, 0, &bytes);
    let (sv, mv, bv, dv) = (
        src.create_view(&Default::default()),
        mask.create_view(&Default::default()),
        behind.create_view(&Default::default()),
        dst.create_view(&Default::default()),
    );
    let group = gpu.device().create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &p.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&sv),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&p.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&mv),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(&bv),
            },
        ],
    });
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("background"),
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
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, &group, &[]);
    rp.draw(0..3, 0..1);
}

/// The camera with its background done as `look` asks, as a new texture of the camera's size.
pub fn draw(
    gpu: &Gpu,
    p: &Pipelines,
    st: &mut State,
    camera: &wgpu::Texture,
    mask: &Mask,
    image: Option<&wgpu::Texture>,
    look: &Look,
) -> wgpu::Texture {
    let full = size(camera);
    let mut encoder = gpu.device().create_command_encoder(&Default::default());
    let mask_t = mask_texture(gpu, st, mask).clone();
    let mut behind = camera.clone();
    if look.mode == 1 {
        let quarter = ((full.0 / 4).max(1), (full.1 / 4).max(1));
        let fresh = !matches!(&st.down, Some((d, _)) if size(d) == quarter);
        if fresh {
            st.down = Some((target(gpu, quarter), target(gpu, quarter)));
        }
        let (down, tmp) = st.down.clone().expect("just set");
        let src_texel = [1.0 / full.0 as f32, 1.0 / full.1 as f32];
        let q_texel = [1.0 / quarter.0 as f32, 1.0 / quarter.1 as f32];
        let spacing = 0.5 + look.blur.clamp(0.0, 100.0) / 100.0 * 3.5;
        let mut params = [0f32; 16];
        params[..2].copy_from_slice(&src_texel);
        pass(
            gpu,
            p,
            &mut encoder,
            &p.down,
            params,
            camera,
            &mask_t,
            camera,
            &down,
        );
        for (dir, from, to) in [([1.0, 0.0], &down, &tmp), ([0.0, 1.0], &tmp, &down)] {
            let mut params = [0f32; 16];
            params[..2].copy_from_slice(&q_texel);
            params[2..4].copy_from_slice(&dir);
            params[12] = spacing;
            pass(
                gpu,
                p,
                &mut encoder,
                &p.blur,
                params,
                from,
                &mask_t,
                from,
                to,
            );
        }
        behind = down;
    } else if look.mode == 4
        && let Some(i) = image
    {
        behind = i.clone();
    }
    let out = st
        .out
        .take()
        .filter(|t| size(t) == full)
        .unwrap_or_else(|| target(gpu, full));
    let (threshold, softness) = edge(look.shift, look.feather);
    let c = look.colour;
    let (sx, sy) = if look.mode == 4 {
        cover(full, size(&behind))
    } else {
        (1.0, 1.0)
    };
    let params = [
        look.mode as f32,
        threshold,
        softness,
        0.0,
        (c & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        ((c >> 16) & 0xFF) as f32 / 255.0,
        1.0,
        mask.rotation as f32,
        if mask.mirrored { 1.0 } else { 0.0 },
        sx,
        sy,
        0.0,
        0.0,
        0.0,
        0.0,
    ];
    pass(
        gpu,
        p,
        &mut encoder,
        &p.composite,
        params,
        camera,
        &mask_t,
        &behind,
        &out,
    );
    gpu.queue().submit([encoder.finish()]);
    st.out = Some(out.clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_and_cover_are_read_as_the_desktop_sets_them() {
        assert_eq!(edge(0.0, 0.0), (0.5, 0.01));
        let (t, s) = edge(100.0, 100.0);
        assert!((t - 0.1).abs() < 1e-6 && (s - 0.26).abs() < 1e-6);
        // a square picture behind a portrait camera: cropped at the sides
        let (x, y) = cover((1080, 1920), (1000, 1000));
        assert!((x - 0.5625).abs() < 1e-4 && y == 1.0);
        assert_eq!(cover((1920, 1080), (1920, 1080)), (1.0, 1.0));
    }
}
