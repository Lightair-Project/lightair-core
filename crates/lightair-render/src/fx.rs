//! The effect catalog's shader effects (`fx.*`, the Live filter's) on a clip's picture: the
//! same effects ported to WGSL (fx.wgsl), each one pass or several, run on the GPU before the
//! picture is composited. Their settings are the .effect files' uniforms, read from those files
//! (names, order, defaults), so the two stay in step. The .effect files in `fx/` are copies of
//! the desktop's Lightair Effect Stack module's (`modules/lightair-effect-stack/data/fx/` in the
//! desktop repo), which stays their source; the desktop's `cargo xtask check` fails if they differ.

use std::collections::HashMap;

use crate::sequence::ClipEffect;

const PRELUDE: &str = include_str!("fx_prelude.wgsl");
const EFFECTS: &str = include_str!("fx.wgsl");

macro_rules! effect_files {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../fx/", $name, ".effect")))),*]
    };
}

/// Each effect's .effect file (for its uniforms).
const FILES: &[(&str, &str)] = effect_files!(
    "blur", "bulge", "chroma", "duotone", "edges", "emboss", "glow", "grain", "halftone", "invert", "kaleido", "mirror", "mono", "pixelate",
    "posterize", "scanlines", "sketch", "swirl", "tiltshift", "vhs", "vignette", "wave",
);

/// A uniform of an effect: its name, whether it's a colour, its default.
#[derive(Debug, Clone, PartialEq)]
pub struct Uniform {
    pub name: String,
    pub colour: bool,
    pub default: [f32; 4],
}

/// An effect's uniforms, in order (`u.p[0]`, `u.p[1]`, …).
pub fn uniforms(name: &str) -> Vec<Uniform> {
    let Some((_, file)) = FILES.iter().find(|(n, _)| *n == name) else { return Vec::new() };
    file.lines()
        .filter_map(|l| {
            let l = l.trim().strip_prefix("uniform ")?;
            let (decl, value) = l.split_once('=').map_or((l.trim_end_matches(';'), ""), |(a, b)| (a, b));
            let mut parts = decl.split_whitespace();
            let (ty, name) = (parts.next()?, parts.next()?.trim_end_matches(';'));
            let nums: Vec<f32> = value
                .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            let colour = ty.starts_with("float4") || ty.starts_with("float3");
            let default = if colour {
                [nums.first().copied().unwrap_or(1.0), nums.get(1).copied().unwrap_or(1.0), nums.get(2).copied().unwrap_or(1.0), nums.get(3).copied().unwrap_or(1.0)]
            } else {
                [nums.first().copied().unwrap_or(0.0), 0.0, 0.0, 0.0]
            };
            Some(Uniform { name: name.to_string(), colour, default })
        })
        .collect()
}

/// The effects there are (their names without "fx."), and how many passes each runs.
pub fn passes() -> &'static HashMap<String, usize> {
    static P: std::sync::OnceLock<HashMap<String, usize>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let mut m: HashMap<String, usize> = HashMap::new();
        for line in EFFECTS.lines() {
            let Some(rest) = line.trim().strip_prefix("fn fs_") else { continue };
            let Some(name_pass) = rest.split('(').next() else { continue };
            if let Some((name, pass)) = name_pass.rsplit_once('_')
                && let Ok(n) = pass.parse::<usize>()
            {
                let e = m.entry(name.to_string()).or_insert(0);
                *e = (*e).max(n + 1);
            }
        }
        m
    })
}

/// A setting as the shader takes it: a number, or a colour (OBS's 0xAABBGGRR).
fn value_of(u: &Uniform, settings: &serde_json::Map<String, serde_json::Value>) -> [f32; 4] {
    match settings.get(&u.name) {
        Some(v) if u.colour => match v.as_u64() {
            Some(c) => [(c & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, ((c >> 16) & 0xFF) as f32 / 255.0, ((c >> 24) & 0xFF) as f32 / 255.0],
            None => u.default,
        },
        Some(v) => match v.as_f64().or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 })) {
            Some(x) => [x as f32, 0.0, 0.0, 0.0],
            None => u.default,
        },
        None => u.default,
    }
}

/// A colour look-up table ("fx.lut", settings `file`: a .cube file's path, `amount` 0…1): each
/// colour graded through a 3D table, as the desktop's LUT filter does. Pictures come in
/// premultiplied; the table is read with the colour unpremultiplied, in the picture's own
/// (sRGB-encoded) values, as .cube files expect.
const LUT_SHADER: &str = r#"
struct L {
    // x amount, y the table's size
    a: vec4<f32>,
    lo: vec4<f32>,
    hi: vec4<f32>,
};
@group(0) @binding(0) var<uniform> l: L;
@group(0) @binding(1) var picture: texture_2d<f32>;
@group(0) @binding(2) var table: texture_3d<f32>;
@group(0) @binding(3) var lin: sampler;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    let xy = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VOut;
    o.pos = vec4<f32>(xy.x * 2.0 - 1.0, 1.0 - xy.y * 2.0, 0.0, 1.0);
    o.uv = xy;
    return o;
}

@fragment
fn fs_lut(v: VOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(picture, lin, v.uv, 0.0);
    if c.a <= 0.0 {
        return c;
    }
    let rgb = clamp(c.rgb / c.a, vec3<f32>(0.0), vec3<f32>(1.0));
    let n = l.a.y;
    let t = clamp((rgb - l.lo.xyz) / max(l.hi.xyz - l.lo.xyz, vec3<f32>(1e-6)), vec3<f32>(0.0), vec3<f32>(1.0));
    let graded = textureSampleLevel(table, lin, (t * (n - 1.0) + 0.5) / n, 0.0).rgb;
    let out = mix(rgb, clamp(graded, vec3<f32>(0.0), vec3<f32>(1.0)), clamp(l.a.x, 0.0, 1.0));
    return vec4<f32>(out * c.a, c.a);
}
"#;

/// The largest table read (65³, the biggest common one).
pub const LUT_MAX: usize = 65;

/// A 3D table from a .cube file (Adobe/Resolve): its size, its domain (low, high) and its
/// entries, red changing fastest. 1D tables and tables over [`LUT_MAX`] are refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Cube {
    pub size: usize,
    pub lo: [f32; 3],
    pub hi: [f32; 3],
    pub entries: Vec<[f32; 3]>,
}

pub fn parse_cube(text: &str) -> Result<Cube, String> {
    let (mut size, mut lo, mut hi) = (0usize, [0.0f32; 3], [1.0f32; 3]);
    let mut entries = Vec::new();
    let three = |rest: &str| -> Option<[f32; 3]> {
        let v: Vec<f32> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        (v.len() == 3).then(|| [v[0], v[1], v[2]])
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match word {
            "TITLE" => {}
            "LUT_1D_SIZE" => return Err("a 1D table (only 3D ones are read)".into()),
            "LUT_3D_SIZE" => size = rest.trim().parse().map_err(|_| format!("a size of “{}”", rest.trim()))?,
            "DOMAIN_MIN" => lo = three(rest).ok_or("an unreadable DOMAIN_MIN")?,
            "DOMAIN_MAX" => hi = three(rest).ok_or("an unreadable DOMAIN_MAX")?,
            _ if word.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '.') => {
                entries.push(three(line).ok_or_else(|| format!("an unreadable line: “{line}”"))?);
            }
            // other keywords (LUT_3D_INPUT_RANGE, …) don't change the table
            _ => {}
        }
    }
    if !(2..=LUT_MAX).contains(&size) {
        return Err(format!("a table of {size} (2 to {LUT_MAX} are read)"));
    }
    if entries.len() != size * size * size {
        return Err(format!("{} entries for a table of {size}³", entries.len()));
    }
    Ok(Cube { size, lo, hi, entries })
}

/// A table on the GPU, and the file's time it was read at.
struct Lut {
    modified: Option<std::time::SystemTime>,
    table: Option<(wgpu::Texture, Cube)>,
}

/// The effects' pipelines, made as they're first used.
pub struct Fx {
    device: wgpu::Device,
    queue: wgpu::Queue,
    module: wgpu::ShaderModule,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: HashMap<String, wgpu::RenderPipeline>,
    linear: wgpu::Sampler,
    point: wgpu::Sampler,
    /// Three targets per size, passes going round them (never writing what they read).
    targets: HashMap<(u32, u32), [wgpu::Texture; 3]>,
    /// "fx.lut": its pipeline and layout (made when first used), and the tables read, by file.
    lut: Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
    luts: HashMap<std::path::PathBuf, Lut>,
}

/// One effect to run: one of the catalog's (its passes and settings), or a table.
enum Run {
    Catalog(String, usize, Vec<[f32; 4]>),
    Lut(std::path::PathBuf, f32),
}

impl Fx {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Fx {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-fx"),
            source: wgpu::ShaderSource::Wgsl(format!("{PRELUDE}\n{EFFECTS}").into()),
        });
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let samp = |binding| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                tex(1),
                tex(2),
                samp(3),
                samp(4),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
        let sampler = |filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: filter,
                min_filter: filter,
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                ..Default::default()
            })
        };
        Fx {
            device: device.clone(),
            queue: queue.clone(),
            module,
            layout,
            pipeline_layout,
            pipelines: HashMap::new(),
            linear: sampler(wgpu::FilterMode::Linear),
            point: sampler(wgpu::FilterMode::Nearest),
            targets: HashMap::new(),
            lut: None,
            luts: HashMap::new(),
        }
    }

    fn lut_pipeline(&mut self) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
        if let Some(p) = &self.lut {
            return p.clone();
        }
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-lut"),
            source: wgpu::ShaderSource::Wgsl(LUT_SHADER.into()),
        });
        let tex = |binding, view_dimension| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension,
                multisampled: false,
            },
            count: None,
        };
        let layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightair-lut"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex(1, wgpu::TextureViewDimension::D2),
                tex(2, wgpu::TextureViewDimension::D3),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fs_lut"),
            layout: Some(&pipeline_layout),
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
                entry_point: Some("fs_lut"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.lut = Some((pipeline, layout));
        self.lut.clone().unwrap()
    }

    /// The table in `file`, read again when the file changes (none: unreadable, logged once).
    fn table(&mut self, file: &std::path::Path) -> Option<(wgpu::Texture, Cube)> {
        let modified = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        if let Some(l) = self.luts.get(file)
            && l.modified == modified
        {
            return l.table.clone();
        }
        let table = match std::fs::read_to_string(file).map_err(|e| e.to_string()).and_then(|t| parse_cube(&t)) {
            Ok(cube) => {
                let n = cube.size as u32;
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("lightair-lut"),
                    size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D3,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                let bytes: Vec<u8> = cube.entries.iter().flat_map(|e| [to8(e[0]), to8(e[1]), to8(e[2]), 255]).collect();
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &bytes,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * n), rows_per_image: Some(n) },
                    wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
                );
                Some((texture, cube))
            }
            Err(e) => {
                tracing::warn!(file = %file.display(), %e, "a LUT that wasn't read");
                None
            }
        };
        self.luts.insert(file.to_path_buf(), Lut { modified, table: table.clone() });
        table
    }

    fn pipeline(&mut self, entry: &str) -> &wgpu::RenderPipeline {
        if !self.pipelines.contains_key(entry) {
            let p = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState { module: &self.module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.pipelines.insert(entry.to_string(), p);
        }
        &self.pipelines[entry]
    }

    /// Runs `effects` (those switched on and known) on `input` (`size`) at `time` seconds: the
    /// texture holding the result, or `None` when there was nothing to run.
    pub fn run(&mut self, input: &wgpu::Texture, size: (u32, u32), effects: &[ClipEffect], time: f32) -> Option<wgpu::Texture> {
        let runs: Vec<Run> = effects
            .iter()
            .filter(|e| e.on)
            .filter_map(|e| {
                let name = e.kind.strip_prefix("fx.")?.to_string();
                if name == "lut" {
                    let file = e.settings.get("file")?.as_str().filter(|f| !f.is_empty())?;
                    let amount = e.settings.get("amount").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                    return Some(Run::Lut(file.into(), amount));
                }
                let n = *passes().get(&name)?;
                let values = uniforms(&name).iter().map(|u| value_of(u, &e.settings)).collect();
                Some(Run::Catalog(name, n, values))
            })
            .collect();
        if runs.is_empty() {
            return None;
        }
        let targets = self.targets.entry(size).or_insert_with(|| {
            let make = || {
                self.device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
            };
            [make(), make(), make()]
        });
        let targets = targets.clone();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        // each effect reads the last one's result as its source
        let mut current: Option<usize> = None;
        for run in runs {
            let (name, n, values) = match run {
                Run::Catalog(name, n, values) => (name, n, values),
                Run::Lut(file, amount) => {
                    let Some((table, cube)) = self.table(&file) else { continue };
                    let (pipeline, layout) = self.lut_pipeline();
                    let out = (0..3).find(|i| Some(*i) != current).unwrap_or(0);
                    let mut u = [0f32; 12];
                    u[0] = amount;
                    u[1] = cube.size as f32;
                    u[4..7].copy_from_slice(&cube.lo);
                    u[8..11].copy_from_slice(&cube.hi);
                    let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: None,
                        size: (u.len() * 4) as u64,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let bytes: Vec<u8> = u.iter().flat_map(|v| v.to_le_bytes()).collect();
                    self.queue.write_buffer(&buffer, 0, &bytes);
                    let picture = match current {
                        None => input.create_view(&Default::default()),
                        Some(i) => targets[i].create_view(&Default::default()),
                    };
                    let table_view = table.create_view(&wgpu::TextureViewDescriptor {
                        dimension: Some(wgpu::TextureViewDimension::D3),
                        ..Default::default()
                    });
                    let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: &layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&picture) },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(&table_view),
                            },
                            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.linear) },
                        ],
                    });
                    let view = targets[out].create_view(&Default::default());
                    {
                        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: None,
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
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
                        rp.set_pipeline(&pipeline);
                        rp.set_bind_group(0, &group, &[]);
                        rp.draw(0..3, 0..1);
                    }
                    current = Some(out);
                    continue;
                }
            };
            let source_view = match current {
                None => input.create_view(&Default::default()),
                Some(i) => targets[i].create_view(&Default::default()),
            };
            let mut image = current;
            for pass in 0..n {
                let out = (0..3).find(|i| Some(*i) != image && Some(*i) != current).unwrap_or(0);
                // size, texel; clock; p[8]
                let mut u = [0f32; 40];
                u[0..4].copy_from_slice(&[size.0 as f32, size.1 as f32, 1.0 / size.0 as f32, 1.0 / size.1 as f32]);
                u[4] = time;
                for (k, v) in values.iter().take(8).enumerate() {
                    u[8 + k * 4..12 + k * 4].copy_from_slice(v);
                }
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: (u.len() * 4) as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bytes: Vec<u8> = u.iter().flat_map(|v| v.to_le_bytes()).collect();
                self.queue.write_buffer(&buffer, 0, &bytes);
                let image_view = match image {
                    None => input.create_view(&Default::default()),
                    Some(i) => targets[i].create_view(&Default::default()),
                };
                let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&image_view) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&source_view) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.linear) },
                        wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.point) },
                    ],
                });
                let entry = format!("fs_{name}_{pass}");
                let pipeline = self.pipeline(&entry).clone();
                let view = targets[out].create_view(&Default::default());
                {
                    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    rp.set_pipeline(&pipeline);
                    rp.set_bind_group(0, &group, &[]);
                    rp.draw(0..3, 0..1);
                }
                image = Some(out);
            }
            current = image;
        }
        self.queue.submit([encoder.finish()]);
        current.map(|i| targets[i].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_effect_has_its_passes_and_uniforms() {
        for (name, _) in FILES {
            assert!(passes().get(*name).is_some_and(|n| *n >= 1), "{name} has no passes in fx.wgsl");
        }
        assert_eq!(passes()["blur"], 4);
        let mono = uniforms("mono");
        assert_eq!(mono.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["amount", "tint_color", "tint"]);
        assert!(mono[1].colour && (mono[1].default[1] - 0.86).abs() < 1e-6);
        assert_eq!(uniforms("mirror")[0].name, "axis");
        let mut s = serde_json::Map::new();
        s.insert("tint_color".into(), serde_json::json!(0xFF0000FFu32));
        assert_eq!(value_of(&mono[1], &s), [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn cube_files_are_read() {
        let cube = parse_cube(
            "# a comment\nTITLE \"Warm\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n\n\
             0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1.0 1.0 1.0\n",
        )
        .unwrap();
        assert_eq!((cube.size, cube.entries.len(), cube.entries[1]), (2, 8, [1.0, 0.0, 0.0]), "red changes fastest");
        assert!(parse_cube("LUT_3D_SIZE 2\n0 0 0\n").unwrap_err().contains("1 entries"));
        assert!(parse_cube("LUT_1D_SIZE 4\n").is_err());
        assert!(parse_cube("LUT_3D_SIZE 129\n").is_err());
    }
}
