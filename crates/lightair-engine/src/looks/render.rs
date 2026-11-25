//! The camera looks' render side on the phone's GPU, with the desktop's passes, order, sizes,
//! formats, maths and constants, so a project looks the same on both. Each frame: the face layout
//! and its data texture, the half-res bilateral and eighth-res tone (S1-S7), the segmentation
//! snapped to the frame, the skin's optical density and the body's height field (G2-G9), the
//! landmark mask (M1), the face mesh, its paint and normals (M2-M4, [`super::mesh`]), the mask's
//! blur (M5/M6), the main pass (F1: warp → skin masks → frequency-separated smoothing → pigments →
//! teeth → makeup → eye light → clarity), Finish (F2: relight, then the makeup look's grade) and
//! the Colour grade's LUT (P2). Textures hold sRGB-encoded values in Rgba8Unorm (Rgba16Float and
//! Rgba32Float where the desktop uses them), as the desktop does, so the constants are the same.
//!
//! Not drawn here: the background (`crate::background`), stickers, motion and atmosphere (no
//! catalogue effect sets them on phones), the makeup look's `.cube` LUT (no bundled LUTs and no
//! makeup preset names one; a desktop path means nothing on a phone), the landmarks overlay and
//! the green key (macOS's own camera effect).

use std::collections::HashMap;
use std::ops::Range;

use lightair_model::{Id, Source, catalog};
use serde_json::{Map, Value};

use super::layout::{self, Channel, Shape};
use super::mesh::{self, Maps, Pose};
use super::{Face, Masks, Pigment, grade};
use crate::gpu::Gpu;

// The colour grade (`grade.rs`), declared here until `looks/mod.rs` declares it.
/// The effect kinds drawn here.
pub const KINDS: [&str; 3] = ["studio.beauty", "studio.light", "studio.grade"];

const LIGHTS_JSON: &str = include_str!("../../assets/effect-stack/lights.json");
const MAKEUP_JSON: &str = include_str!("../../assets/effect-stack/makeup/presets.json");

const RGBA8: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const RGBA16F: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

// ------------------------------------------------------------------------------------- settings

/// A light preset's settings.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LightParams {
    /// Towards the light: x right, y down, z to the camera.
    pub key_dir: [f32; 3],
    pub key_color: [f32; 3],
    pub key_amt: f32,
    pub rim_color: [f32; 3],
    pub rim_amt: f32,
    /// How dark the unlit side gets (0..1).
    pub fill: f32,
    /// Darkens everything outside the person (0..1).
    pub bg_dim: f32,
    pub exposure: f32,
    pub warmth: f32,
}

/// A lights.json preset's settings.
pub fn parse_light(d: &Value) -> LightParams {
    let deg = std::f32::consts::PI / 180.0;
    let az = grade::num(d, "az") * deg;
    let el = grade::num(d, "el") * deg;
    let mut l = LightParams {
        key_dir: [az.sin() * el.cos(), -el.sin(), az.cos() * el.cos()],
        key_color: [1.0; 3],
        key_amt: grade::num(d, "key_amt"),
        rim_color: [1.0; 3],
        rim_amt: grade::num(d, "rim_amt"),
        fill: grade::num(d, "fill"),
        bg_dim: grade::num(d, "bg_dim"),
        exposure: grade::num(d, "exposure"),
        warmth: grade::num(d, "warmth"),
    };
    grade::hex(d, "key", &mut l.key_color);
    grade::hex(d, "rim", &mut l.rim_color);
    l
}

fn json(text: &'static str, cell: &'static std::sync::OnceLock<Value>) -> &'static Value {
    cell.get_or_init(|| serde_json::from_str(text).unwrap_or(Value::Null))
}

/// The lights' names, as the desktop lists them (`lights.json`).
pub fn light_names() -> Vec<String> {
    serde_json::from_str::<Value>(LIGHTS_JSON)
        .ok()
        .and_then(|v| {
            v["lights"].as_array().map(|a| {
                a.iter()
                    .filter_map(|l| l["name"].as_str().map(String::from))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// The makeup looks' names (the Look picker's choices after "None").
pub fn makeup_names() -> Vec<String> {
    serde_json::from_str::<Value>(MAKEUP_JSON)
        .ok()
        .and_then(|v| {
            v["presets"].as_array().map(|a| {
                a.iter()
                    .filter_map(|l| l["name"].as_str().map(String::from))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// The named lights.json preset, if there is one (an empty or unknown name: the light is off).
pub fn light_preset(name: &str) -> Option<LightParams> {
    static ROOT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    grade::find_in(json(LIGHTS_JSON, &ROOT), "lights", name).map(parse_light)
}

/// A makeup look's settings as read from the source: amounts 0..1, the look's grade −1..1, colours
/// sRGB 0..1.
#[derive(Debug, Clone, PartialEq)]
pub struct Makeup {
    pub filter_amt: f32,
    pub makeup_amt: f32,
    pub exposure: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub warmth: f32,
    pub tint: f32,
    pub fade: f32,
    /// `fl_lut`: read, not drawn (see the module's notes).
    pub lut: String,
    pub lip: [f32; 3],
    pub lip_opacity: f32,
    pub lip_gloss: f32,
    pub blush: [f32; 3],
    pub blush_opacity: f32,
    pub blush_style: i32,
    pub shadow: [f32; 3],
    pub shadow_opacity: f32,
    /// 0 none, 1 classic, 2 winged, 3 cat eye, 4 smoky (face-liner.png channels 1..4).
    pub liner_style: i32,
    pub contour: f32,
    pub highlight: f32,
    pub brow: f32,
}

/// What the looks draw for a camera, read from its settings as the desktop reads them: sliders
/// /100 within their ranges, the desktop's defaults for what's missing.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub smooth: f32,
    pub foundation: f32,
    pub skin_tone: f32,
    pub redness: f32,
    /// `shadows`: dark circles.
    pub pouch: f32,
    pub teeth: f32,
    pub eyelight: f32,
    /// `contrast`: clarity.
    pub clarity: f32,
    pub shape: Shape,
    pub all_skin: bool,
    pub show_landmarks: bool,
    pub debug_view: i32,
    pub enhance_on: bool,
    pub makeup: Makeup,
    /// The light preset, when its name is one.
    pub light: Option<LightParams>,
    /// `light_amt`/100, 0 with the Light tab off.
    pub light_amt: f32,
    pub filter_preset: String,
    /// The filters.json preset, when its name is one (it has a LUT).
    pub filter: Option<grade::GradeParams>,
    /// `filter_preset_amt`/100, 0 with the Filters tab off.
    pub filter_amt: f32,
}

/// `obs_data_get_int`: the value (a double truncated), `default` when the key is missing.
fn int(s: &Map<String, Value>, k: &str, default: i64) -> i64 {
    match s.get(k) {
        None | Some(Value::Null) => default,
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .unwrap_or(0),
    }
}

fn boolean(s: &Map<String, Value>, k: &str, default: bool) -> bool {
    match s.get(k) {
        None | Some(Value::Null) => default,
        Some(v) => v.as_bool().unwrap_or(false),
    }
}

fn string(s: &Map<String, Value>, k: &str, default: &str) -> String {
    match s.get(k) {
        None | Some(Value::Null) => default.into(),
        Some(v) => v.as_str().unwrap_or("").into(),
    }
}

/// A slider (stored as an int, /100), held to its range (0..100, or −100..100 bipolar).
fn slider(s: &Map<String, Value>, k: &str, default: i64, bipolar: bool) -> f32 {
    int(s, k, default).clamp(if bipolar { -100 } else { 0 }, 100) as f32 / 100.0
}

/// "#rrggbb" → 0xAABBGGRR, white otherwise.
fn hex_to_obs_color(hex: &str) -> i64 {
    let Some(digits) = hex.strip_prefix('#').filter(|_| hex.len() >= 7) else {
        return 0xFFFF_FFFF;
    };
    let v = digits
        .chars()
        .map_while(|c| c.to_digit(16))
        .fold(0u64, |a, d| a.wrapping_mul(16).wrapping_add(d as u64)) as u32;
    let (r, g, b) = ((v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF);
    0xFF00_0000 | ((b as i64) << 16) | ((g as i64) << 8) | r as i64
}

/// 0xAABBGGRR → sRGB 0..1.
fn obs_color_to_rgb(c: i64) -> [f32; 3] {
    [
        (c & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        ((c >> 16) & 0xFF) as f32 / 255.0,
    ]
}

/// A makeup look's values into the settings.
fn apply_makeup_preset(s: &mut Map<String, Value>, name: &str) {
    static ROOT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let p = json(MAKEUP_JSON, &ROOT)
        .get("presets")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|p| p.get("name").and_then(Value::as_str).unwrap_or("") == name)
        });
    let fl = p.and_then(|p| p.get("filter")).filter(|v| v.is_object());
    let mk = p.and_then(|p| p.get("makeup")).filter(|v| v.is_object());
    // preset JSON uses 0..1 floats; settings use -100..100 ints (`set_pct`, :83-87)
    let pct = |src: Option<&Value>, k: &str| {
        let v = src
            .and_then(|o| o.get(k))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        Value::from((v * 100.0 + if v >= 0.0 { 0.5 } else { -0.5 }) as i64)
    };
    let text = |src: Option<&Value>, k: &str| {
        src.and_then(|o| o.get(k))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    for k in [
        "exposure",
        "contrast",
        "saturation",
        "warmth",
        "tint",
        "fade",
    ] {
        s.insert(format!("fl_{k}"), pct(fl, k));
    }
    s.insert("fl_lut".into(), Value::from(text(fl, "lut")));
    for k in ["lip_color", "blush_color", "shadow_color"] {
        let c = if mk.is_some() {
            hex_to_obs_color(&text(mk, k))
        } else {
            0xFFFF_FFFF
        };
        s.insert(format!("mk_{k}"), Value::from(c));
    }
    for k in [
        "lip_opacity",
        "lip_gloss",
        "blush_opacity",
        "shadow_opacity",
    ] {
        s.insert(format!("mk_{k}"), pct(mk, k));
    }
    let ls = text(mk, "liner_style");
    let style = ["none", "classic", "winged", "cat", "smoky"]
        .iter()
        .rposition(|n| *n == ls)
        .filter(|i| *i >= 1)
        .unwrap_or(0) as i64;
    s.insert("mk_liner_style".into(), Value::from(style));
    for k in ["contour", "highlight", "brow"] {
        s.insert(format!("mk_{k}"), pct(mk, k));
    }
    let blush_style = mk
        .and_then(|o| o.get("blush_style"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    s.insert("mk_blush_style".into(), Value::from(blush_style));
}

impl Settings {
    /// The source's looks as the desktop's looks get them
    /// (`lightair_model::catalog::studio_settings`), or `None` when no `studio.beauty`,
    /// `studio.light` or `studio.grade` is switched on.
    pub fn from_source(source: &Source) -> Option<Settings> {
        source
            .effects
            .iter()
            .any(|e| e.enabled && KINDS.contains(&e.kind.as_str()))
            .then(|| Settings::from_map(&catalog::studio_settings(&source.effects)))
    }

    /// The settings parsing on one settings object.
    pub fn from_map(settings: &Map<String, Value>) -> Settings {
        let mut s = settings.clone();
        // a look not yet applied sets its values
        let preset = string(&s, "makeup_preset", "None");
        if preset != string(&s, "makeup_applied", "") {
            apply_makeup_preset(&mut s, &preset);
            s.insert("makeup_applied".into(), Value::from(preset));
        }
        let s = &s;

        let mut shape = Shape::default();
        const BIPOLAR: [&str; 12] = [
            "chin",
            "forehead",
            "eye_spacing",
            "eye_position",
            "eye_tilt",
            "eye_outer",
            "nose_length",
            "mouth_size",
            "mouth_position",
            "brow_thickness",
            "brow_position",
            "skin_tone",
        ];
        for (k, v) in Shape::KEYS.iter().zip(shape.fields_mut()) {
            *v = slider(s, k, 0, BIPOLAR.contains(k));
        }

        // the makeup look
        let pct = |k: &str, bipolar: bool| slider(s, k, 0, bipolar);
        let mut liner_style = int(s, "mk_liner_style", 0) as i32;
        if !(0..5).contains(&liner_style) {
            liner_style = 0;
        }
        // settings saved when eyeliner was an amount slider: any amount becomes the winged style
        if !s.contains_key("mk_liner_style") && int(s, "mk_liner", 0) > 0 {
            liner_style = 2;
        }
        let makeup = Makeup {
            filter_amt: slider(s, "filter_amt", 70, false),
            makeup_amt: slider(s, "makeup_amt", 70, false),
            exposure: pct("fl_exposure", true),
            contrast: pct("fl_contrast", true),
            saturation: pct("fl_saturation", true),
            warmth: pct("fl_warmth", true),
            tint: pct("fl_tint", true),
            fade: pct("fl_fade", false),
            lut: string(s, "fl_lut", ""),
            lip: obs_color_to_rgb(int(s, "mk_lip_color", 0xFFFF_FFFF)),
            lip_opacity: pct("mk_lip_opacity", false),
            lip_gloss: pct("mk_lip_gloss", false),
            blush: obs_color_to_rgb(int(s, "mk_blush_color", 0xFFFF_FFFF)),
            blush_opacity: pct("mk_blush_opacity", false),
            blush_style: int(s, "mk_blush_style", 0) as i32,
            shadow: obs_color_to_rgb(int(s, "mk_shadow_color", 0xFFFF_FFFF)),
            shadow_opacity: pct("mk_shadow_opacity", false),
            liner_style,
            contour: pct("mk_contour", false),
            highlight: pct("mk_highlight", false),
            brow: pct("mk_brow", false),
        };

        // light and the Colour grade
        let filters_on = boolean(s, "filters_on", true);
        let lights_on = boolean(s, "light_on", true);
        let filter_preset = string(s, "filter_preset", "");
        let mut out = Settings {
            smooth: slider(s, "smooth", 50, false),
            foundation: slider(s, "foundation", 20, false),
            skin_tone: slider(s, "skin_tone", 0, true),
            redness: slider(s, "redness", 0, false),
            pouch: slider(s, "shadows", 0, false),
            teeth: slider(s, "teeth", 0, false),
            eyelight: slider(s, "eyelight", 30, false),
            clarity: slider(s, "contrast", 0, false),
            shape,
            all_skin: boolean(s, "all_skin", true),
            show_landmarks: boolean(s, "show_landmarks", false),
            debug_view: int(s, "debug_view", 0) as i32,
            enhance_on: boolean(s, "enhance_on", true),
            makeup,
            light: light_preset(&string(s, "light_preset", "")),
            light_amt: if lights_on {
                slider(s, "light_amt", 60, false)
            } else {
                0.0
            },
            filter: grade::preset(&filter_preset),
            filter_preset,
            filter_amt: if filters_on {
                slider(s, "filter_preset_amt", 80, false)
            } else {
                0.0
            },
        };
        // the Looks screen's part switches: a part switched off draws
        // nothing of its own, and its stored values stay as they are for when it's back on
        if !boolean(s, "skin_on", true) {
            out.skin_off();
        }
        if !boolean(s, "shape_on", true) {
            out.shape = Shape::default();
        }
        if !boolean(s, "makeup_on", true) {
            out.makeup_off();
        }
        out
    }

    fn skin_off(&mut self) {
        self.smooth = 0.0;
        self.foundation = 0.0;
        self.skin_tone = 0.0;
        self.redness = 0.0;
        self.pouch = 0.0;
        self.eyelight = 0.0;
        self.teeth = 0.0;
        self.clarity = 0.0;
    }

    fn makeup_off(&mut self) {
        self.makeup.makeup_amt = 0.0;
        self.makeup.filter_amt = 0.0;
    }

    /// The thermal governor's step 3 ("costly effects lightened"): the face parts (Skin,
    /// Shape, Makeup) stop; Light and the Colour grade stay. The settings themselves are kept.
    pub fn lighten(&mut self) {
        self.skin_off();
        self.shape = Shape::default();
        self.makeup_off();
    }

    /// Whether the light draws.
    pub fn light_active(&self) -> bool {
        self.light.is_some() && self.light_amt > 0.0
    }

    /// Whether the makeup look draws anything (the `.cube` LUT aside).
    fn makeup_active(&self) -> bool {
        let m = &self.makeup;
        let grade = m.filter_amt > 0.0
            && (m.exposure != 0.0
                || m.contrast != 0.0
                || m.saturation != 0.0
                || m.warmth != 0.0
                || m.tint != 0.0
                || m.fade != 0.0);
        let paint = m.makeup_amt > 0.0
            && (m.lip_opacity > 0.0
                || m.blush_opacity > 0.0
                || m.shadow_opacity > 0.0
                || m.liner_style > 0
                || m.contour > 0.0
                || m.highlight > 0.0
                || m.brow > 0.0);
        grade || paint
    }

    /// Whether the looks draw anything.
    pub fn active(&self) -> bool {
        let fx = (self.filter.is_some() && self.filter_amt > 0.0) || self.light_active();
        if self.makeup_active() || fx {
            return true;
        }
        if self.pouch > 0.0
            || self.teeth > 0.0
            || self.eyelight > 0.0
            || self.clarity > 0.0
            || !self.all_skin
            || self.show_landmarks
            || self.debug_view != 0
        {
            return true;
        }
        if self.shape.any() {
            return true;
        }
        self.smooth > 0.0 || self.foundation > 0.0 || self.skin_tone != 0.0 || self.redness > 0.0
    }
}

// ------------------------------------------------------------------------------------- the GPU

/// One draw of a pass: the fragment entry (its pipeline), the channels it writes, its vertices.
pub(crate) struct Draw {
    pub entry: &'static str,
    pub writes: wgpu::ColorWrites,
    pub vertices: Range<u32>,
}

impl Draw {
    pub fn all(entry: &'static str, vertices: Range<u32>) -> Draw {
        Draw {
            entry,
            writes: wgpu::ColorWrites::ALL,
            vertices,
        }
    }
}

/// The looks' shaders, bind group layouts, pipelines (made when first used) and samplers.
pub(crate) struct Kit {
    blur: wgpu::ShaderModule,
    fin: wgpu::ShaderModule,
    post: wgpu::ShaderModule,
    blur_layout: wgpu::BindGroupLayout,
    final_layout: wgpu::BindGroupLayout,
    blur_pl: wgpu::PipelineLayout,
    final_pl: wgpu::PipelineLayout,
    pipes: HashMap<(&'static str, wgpu::TextureFormat, u32), wgpu::RenderPipeline>,
    /// Linear, clamped.
    sampler: wgpu::Sampler,
    /// The same on the mipmapped mesh maps (linear filtering includes the mip levels, as on the
    /// desktop).
    mip_sampler: wgpu::Sampler,
}

const FINAL_ENTRIES: [&str; 2] = ["fs_main", "fs_finish"];
const GEOMETRY_ENTRIES: [&str; 3] = ["fs_paint", "fs_mesh_copy", "fs_solid"];

impl Kit {
    fn new(device: &wgpu::Device) -> Kit {
        let module = |label, src: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            })
        };
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let uniform = wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let tex = |filterable| wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let sampler = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let blur_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("looks blur"),
            entries: &[
                entry(0, uniform),
                entry(1, tex(true)),
                entry(2, tex(true)),
                entry(3, sampler),
            ],
        });
        let mut final_entries = vec![entry(0, uniform)];
        for b in 1..=9 {
            // data_tex (4) is RGBA32F, read with textureLoad
            final_entries.push(entry(b, tex(b != 4)));
        }
        final_entries.push(entry(10, sampler));
        let final_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("looks final"),
            entries: &final_entries,
        });
        let pl = |layout| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            })
        };
        let linear = |mip| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: if mip {
                    wgpu::MipmapFilterMode::Linear
                } else {
                    wgpu::MipmapFilterMode::Nearest
                },
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                ..Default::default()
            })
        };
        Kit {
            blur: module("looks blur", include_str!("gpu/blur.wgsl")),
            fin: module("looks final", include_str!("gpu/final.wgsl")),
            post: module("looks post", include_str!("gpu/post.wgsl")),
            blur_pl: pl(&blur_layout),
            final_pl: pl(&final_layout),
            blur_layout,
            final_layout,
            pipes: HashMap::new(),
            sampler: linear(false),
            mip_sampler: linear(true),
        }
    }

    fn pipeline(
        &mut self,
        device: &wgpu::Device,
        entry: &'static str,
        format: wgpu::TextureFormat,
        writes: wgpu::ColorWrites,
    ) -> wgpu::RenderPipeline {
        let key = (entry, format, writes.bits());
        if let Some(p) = self.pipes.get(&key) {
            return p.clone();
        }
        let (module, layout) = if FINAL_ENTRIES.contains(&entry) {
            (&self.fin, &self.final_pl)
        } else if entry == "fs_filter_lut" {
            (&self.post, &self.blur_pl)
        } else {
            (&self.blur, &self.blur_pl)
        };
        let geometry = GEOMETRY_ENTRIES.contains(&entry);
        let attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
        ];
        let buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: 16,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        })];
        let p = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(entry),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some(if geometry { "vs_geom" } else { "vs" }),
                compilation_options: Default::default(),
                buffers: if geometry { &buffers } else { &[] },
            },
            // culling off
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                // blending off throughout
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: writes,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipes.insert(key, p.clone());
        p
    }

    /// One pass into `out` (cleared to 0 first): `textures` in binding order after the uniform
    /// (2 for the blur and post layouts, 9 for the final one), optional geometry (x, y, u, v per
    /// vertex), its draws.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pass(
        &mut self,
        gpu: &Gpu,
        enc: &mut wgpu::CommandEncoder,
        out: &wgpu::Texture,
        uniform: &[f32],
        textures: &[&wgpu::Texture],
        geometry: Option<&[f32]>,
        draws: &[Draw],
        mipmapped: bool,
    ) {
        let device = gpu.device();
        let mut bytes: Vec<u8> = uniform.iter().flat_map(|v| v.to_le_bytes()).collect();
        bytes.resize(bytes.len().div_ceil(16).max(1) * 16, 0);
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("looks uniform"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue().write_buffer(&ubuf, 0, &bytes);
        let views: Vec<wgpu::TextureView> = textures
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        let is_final = draws
            .first()
            .is_some_and(|d| FINAL_ENTRIES.contains(&d.entry));
        let sampler = if mipmapped {
            &self.mip_sampler
        } else {
            &self.sampler
        };
        let mut entries = vec![wgpu::BindGroupEntry {
            binding: 0,
            resource: ubuf.as_entire_binding(),
        }];
        for (i, v) in views.iter().enumerate() {
            entries.push(wgpu::BindGroupEntry {
                binding: i as u32 + 1,
                resource: wgpu::BindingResource::TextureView(v),
            });
        }
        entries.push(wgpu::BindGroupEntry {
            binding: views.len() as u32 + 1,
            resource: wgpu::BindingResource::Sampler(sampler),
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: if is_final {
                &self.final_layout
            } else {
                &self.blur_layout
            },
            entries: &entries,
        });
        let vbuf = geometry.filter(|g| !g.is_empty()).map(|g| {
            let bytes: Vec<u8> = g.iter().flat_map(|v| v.to_le_bytes()).collect();
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("looks geometry"),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.queue().write_buffer(&b, 0, &bytes);
            b
        });
        let pipes: Vec<wgpu::RenderPipeline> = draws
            .iter()
            .map(|d| self.pipeline(device, d.entry, out.format(), d.writes))
            .collect();
        let view = out.create_view(&Default::default());
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("looks"),
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
        rp.set_bind_group(0, &group, &[]);
        if let Some(b) = &vbuf {
            rp.set_vertex_buffer(0, b.slice(..));
        } else if geometry.is_some() {
            return; // nothing to draw: the target stays cleared
        }
        for (d, p) in draws.iter().zip(&pipes) {
            if d.vertices.is_empty() {
                continue;
            }
            rp.set_pipeline(p);
            rp.draw(d.vertices.clone(), 0..1);
        }
    }

    /// One full-frame pass of
    /// the blur effect's `entry`, `texel` = 1 / the input size (blur_pass) or as given
    /// (guided_pass).
    #[allow(clippy::too_many_arguments)]
    fn blur(
        &mut self,
        gpu: &Gpu,
        enc: &mut wgpu::CommandEncoder,
        entry: &'static str,
        image: &wgpu::Texture,
        guide: &wgpu::Texture,
        out: &wgpu::Texture,
        texel: [f32; 2],
        dir: [f32; 2],
        step: f32,
        sigma: f32,
    ) {
        let u = [
            texel[0], texel[1], dir[0], dir[1], step, sigma, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        ];
        self.pass(
            gpu,
            enc,
            out,
            &u,
            &[image, guide],
            None,
            &[Draw::all(entry, 0..3)],
            false,
        );
    }
}

fn inv(t: &wgpu::Texture) -> [f32; 2] {
    [1.0 / t.width() as f32, 1.0 / t.height() as f32]
}

fn texture(
    gpu: &Gpu,
    label: &str,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    target: bool,
) -> wgpu::Texture {
    gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: if target {
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
        } else {
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST
        },
        view_formats: &[],
    })
}

/// The render targets for one camera size; the M5/M6 blur gets its own targets, so it never
/// reads and writes one texture.
struct Targets {
    size: (u32, u32),
    half: wgpu::Texture,
    tmp: wgpu::Texture,
    smooth: wgpu::Texture,
    q: wgpu::Texture,
    e: wgpu::Texture,
    tmp_e: wgpu::Texture,
    tone: wgpu::Texture,
    seg: wgpu::Texture,
    skin_q: wgpu::Texture,
    skin_e: wgpu::Texture,
    skin_t: wgpu::Texture,
    skin_tone: wgpu::Texture,
    tone_pb: wgpu::Texture,
    mask_raw: wgpu::Texture,
    mask_tmp: wgpu::Texture,
    mask: wgpu::Texture,
    out: wgpu::Texture,
    data: wgpu::Texture,
    /// Made when first needed: the paint, the face normals, Finish, the grade's LUT pass.
    paint: Option<wgpu::Texture>,
    fnorm: Option<wgpu::Texture>,
    fin: Option<wgpu::Texture>,
    lut: Option<wgpu::Texture>,
}

impl Targets {
    fn new(gpu: &Gpu, (w, h): (u32, u32)) -> Targets {
        let half = ((w / 2).max(1), (h / 2).max(1));
        let quarter = ((w / 4).max(1), (h / 4).max(1));
        let eighth = ((w / 8).max(1), (h / 8).max(1));
        let t = |label, size, format| texture(gpu, label, size, format, true);
        Targets {
            size: (w, h),
            half: t("looks half", half, RGBA8),
            tmp: t("looks tmp", half, RGBA8),
            smooth: t("looks smooth", half, RGBA8),
            q: t("looks q", quarter, RGBA8),
            e: t("looks e", eighth, RGBA8),
            tmp_e: t("looks tmp_e", eighth, RGBA8),
            tone: t("looks tone", eighth, RGBA8),
            seg: t("looks seg", half, RGBA8),
            skin_q: t("looks skin_q", quarter, RGBA16F),
            skin_e: t("looks skin_e", eighth, RGBA16F),
            skin_t: t("looks skin_t", eighth, RGBA16F),
            skin_tone: t("looks skin_tone", eighth, RGBA16F),
            tone_pb: t("looks tone_pb", eighth, RGBA16F),
            mask_raw: t("looks mask raw", half, RGBA8),
            mask_tmp: t("looks mask tmp", half, RGBA8),
            mask: t("looks mask", half, RGBA8),
            out: t("looks out", (w, h), RGBA8),
            // the data texture: DATA_TEX_WIDTH×1 RGBA32F
            data: texture(
                gpu,
                "looks data",
                (layout::DATA_TEX_WIDTH as u32, 1),
                wgpu::TextureFormat::Rgba32Float,
                false,
            ),
            paint: None,
            fnorm: None,
            fin: None,
            lut: None,
        }
    }
}

fn lazy(
    slot: &mut Option<wgpu::Texture>,
    gpu: &Gpu,
    label: &str,
    size: (u32, u32),
) -> wgpu::Texture {
    slot.get_or_insert_with(|| texture(gpu, label, size, RGBA8, true))
        .clone()
}

/// The segmentation's texture and the body
/// height field's targets at its size (`tr_pm1`, `tr_pm2`).
struct PersonTex {
    /// The mask's serial and whether it is the person-only fallback.
    key: (u64, bool),
    tex: wgpu::Texture,
    pm1: wgpu::Texture,
    pm2: wgpu::Texture,
}

/// A segmentation to upload: its key (`PersonTex::key`), RGBA bytes and size.
type Upload = ((u64, bool), Vec<u8>, (u32, u32));

struct PerSource {
    t: Targets,
    person: Option<PersonTex>,
    pose: Pose,
}

/// A person-only mask laid out as the multiclass one (R, G, B 0, A person), turned upright (the
/// mask is as the picture was pushed, `feeds::Mask`).
fn upright_person(m: &crate::feeds::Mask) -> Option<(Vec<u8>, (u32, u32))> {
    let (mw, mh) = m.size;
    if mw == 0 || mh == 0 || m.bytes.len() < (mw * mh) as usize {
        return None;
    }
    let turned = m.rotation % 2 == 1;
    let (w, h) = if turned { (mh, mw) } else { (mw, mh) };
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            // where an upright point is in the mask as computed (gpu/keying.wgsl's mask_uv)
            let mut u = [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32];
            if m.mirrored {
                u[0] = 1.0 - u[0];
            }
            let s = match m.rotation % 4 {
                1 => [u[1], 1.0 - u[0]],
                2 => [1.0 - u[0], 1.0 - u[1]],
                3 => [1.0 - u[1], u[0]],
                _ => u,
            };
            let sx = ((s[0] * mw as f32) as u32).min(mw - 1);
            let sy = ((s[1] * mh as f32) as u32).min(mh - 1);
            out[((y * w + x) * 4 + 3) as usize] = m.bytes[(sy * mw + sx) as usize];
        }
    }
    Some((out, (w, h)))
}

/// The camera looks' render side for every camera that has them: the pipelines, each
/// camera's own textures, and each grade preset's LUT.
pub struct Looks {
    kit: Kit,
    sources: HashMap<Id, PerSource>,
    luts: HashMap<String, Option<wgpu::Texture>>,
    maps: Maps,
}

impl Looks {
    pub fn new(gpu: &Gpu) -> Looks {
        Looks {
            kit: Kit::new(gpu.device()),
            sources: HashMap::new(),
            luts: HashMap::new(),
            maps: Maps::default(),
        }
    }

    /// Drops the textures of cameras not in `keep`.
    pub fn forget_others(&mut self, keep: &[Id]) {
        self.sources.retain(|id, _| keep.contains(id));
    }

    /// `camera` (upright, sRGB-encoded RGBA) with the looks (up to the background): the main pass
    /// F1, then Finish (F2) and the Colour grade's LUT (P2) where they draw. `face_weight` is the
    /// eased face weight; `person` stands in as a person-only segmentation when there's no
    /// multiclass mask. Returns `camera` itself when the looks would draw nothing; otherwise a
    /// texture of this camera's, valid until its next draw.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        source: Id,
        camera: &wgpu::Texture,
        settings: &Settings,
        face: Option<&Face>,
        face_weight: f32,
        masks: &Masks,
        pigment: &Pigment,
        person: Option<&crate::feeds::Mask>,
    ) -> wgpu::Texture {
        let s = settings;
        let (w, h) = (camera.width(), camera.height());
        // nothing active → the looks are skipped
        if !s.active() || w == 0 || h == 0 {
            return camera.clone();
        }
        let Looks {
            kit,
            sources,
            luts,
            maps,
        } = self;
        let st = sources.entry(source).or_insert_with(|| PerSource {
            t: Targets::new(gpu, (w, h)),
            person: None,
            pose: Pose::default(),
        });
        if st.t.size != (w, h) {
            st.t = Targets::new(gpu, (w, h));
        }
        let (wf, hf) = (w as f32, h as f32);
        let en = if s.enhance_on { 1.0 } else { 0.0 };
        let m = &s.makeup;

        // --- landmarks -> layout ---
        let shape = if s.enhance_on {
            s.shape
        } else {
            Shape::default()
        };
        let lay = layout::build(face, &shape, wf, hf, face_weight, m.blush_style);
        gpu.queue().write_texture(
            st.t.data.as_image_copy(),
            &lay.bytes(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(layout::DATA_TEX_WIDTH as u32 * 16),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: layout::DATA_TEX_WIDTH as u32,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        // --- smoothing + tone passes ---
        let t = &mut st.t;
        let hw = t.half.width();
        let iod_half = (if lay.has_face { lay.iod_px } else { wf * 0.1 }) * 0.5;
        // base layer for frequency separation: edge-preserving blur at a fixed, face-relative scale
        let step = (iod_half * 0.16 / 8.0).max(0.5);
        let sigma = 0.09;
        let mut enc = gpu.device().create_command_encoder(&Default::default());
        let e = &mut enc;
        let full = [1.0 / wf, 1.0 / hf];
        kit.blur(
            gpu, e, "fs_copy", camera, camera, &t.half, full, [0.0; 2], 0.0, 0.0,
        ); // S1
        kit.blur(
            gpu,
            e,
            "fs_bilateral",
            &t.half,
            &t.half,
            &t.tmp,
            inv(&t.half),
            [1.0, 0.0],
            step,
            sigma,
        ); // S2
        kit.blur(
            gpu,
            e,
            "fs_bilateral",
            &t.tmp,
            &t.tmp,
            &t.smooth,
            inv(&t.tmp),
            [0.0, 1.0],
            step,
            sigma,
        ); // S3
        kit.blur(
            gpu,
            e,
            "fs_copy",
            &t.smooth,
            &t.smooth,
            &t.q,
            inv(&t.smooth),
            [0.0; 2],
            0.0,
            0.0,
        ); // S4
        kit.blur(
            gpu,
            e,
            "fs_copy",
            &t.q,
            &t.q,
            &t.e,
            inv(&t.q),
            [0.0; 2],
            0.0,
            0.0,
        ); // S5
        kit.blur(
            gpu,
            e,
            "fs_gauss",
            &t.e,
            &t.e,
            &t.tmp_e,
            inv(&t.e),
            [1.0, 0.0],
            1.5,
            0.0,
        ); // S6
        kit.blur(
            gpu,
            e,
            "fs_gauss",
            &t.tmp_e,
            &t.tmp_e,
            &t.tone,
            inv(&t.tmp_e),
            [0.0, 1.0],
            1.5,
            0.0,
        ); // S7
        let mut tone = t.tone.clone();

        // --- learned segmentation: R skin, G face skin, B hair, A person ---
        let fresh: Option<Upload> = match (&masks.seg, person) {
            (Some((bytes, size, serial)), _) => {
                let key = (*serial, false);
                let same = st
                    .person
                    .as_ref()
                    .is_some_and(|p| p.key == key && (p.tex.width(), p.tex.height()) == *size);
                (!same && size.0 > 0 && size.1 > 0 && bytes.len() >= (size.0 * size.1 * 4) as usize)
                    .then(|| (key, bytes.to_vec(), *size))
            }
            (None, Some(pm)) => {
                let key = (pm.serial, true);
                if st.person.as_ref().is_some_and(|p| p.key == key) {
                    None
                } else {
                    upright_person(pm).map(|(b, sz)| (key, b, sz))
                }
            }
            (None, None) => {
                st.person = None;
                None
            }
        };
        if let Some((key, bytes, size)) = fresh {
            let same_size = st
                .person
                .as_ref()
                .is_some_and(|p| (p.tex.width(), p.tex.height()) == size);
            if !same_size {
                st.person = Some(PersonTex {
                    key,
                    tex: texture(gpu, "looks person", size, RGBA8, false),
                    pm1: texture(gpu, "looks pm1", size, RGBA8, true),
                    pm2: texture(gpu, "looks pm2", size, RGBA8, true),
                });
            }
            let p = st.person.as_mut().expect("just made");
            p.key = key;
            gpu.queue().write_texture(
                p.tex.as_image_copy(),
                &bytes[..(size.0 * size.1 * 4) as usize],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.0 * 4),
                    rows_per_image: Some(size.1),
                },
                wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
            );
        }
        let t = &mut st.t;
        let mut seg_ok = false;
        let mut pm_size = (0u32, 0u32);
        if let Some(p) = &st.person {
            let (pm_w, pm_h) = (p.tex.width(), p.tex.height());
            pm_size = (pm_w, pm_h);
            // G2: snap the 256px mask to the frame's real edges
            kit.blur(
                gpu,
                e,
                "fs_refine",
                &p.tex,
                &t.half,
                &t.seg,
                inv(&p.tex),
                [0.0; 2],
                0.0,
                0.07,
            );
            // G3-G6: skin-weighted optical density, blurred by normalised convolution
            let ps = (iod_half * 2.0 * 0.25 / 8.0 / 2.0).max(0.75);
            kit.blur(
                gpu,
                e,
                "fs_skin_od",
                &t.half,
                &t.seg,
                &t.skin_q,
                [0.0; 2],
                [0.0; 2],
                0.0,
                0.0,
            );
            kit.blur(
                gpu,
                e,
                "fs_copy",
                &t.skin_q,
                &t.skin_q,
                &t.skin_e,
                inv(&t.skin_q),
                [0.0; 2],
                0.0,
                0.0,
            );
            kit.blur(
                gpu,
                e,
                "fs_gauss",
                &t.skin_e,
                &t.skin_e,
                &t.skin_t,
                inv(&t.skin_e),
                [1.0, 0.0],
                ps,
                0.0,
            );
            kit.blur(
                gpu,
                e,
                "fs_gauss",
                &t.skin_t,
                &t.skin_t,
                &t.skin_tone,
                inv(&t.skin_t),
                [0.0, 1.0],
                ps,
                0.0,
            );
            // G7/G8: soft body height field for whole-body light normals, from the snapped mask
            let stp = (pm_w as f32 * 0.012).max(1.0);
            kit.blur(
                gpu,
                e,
                "fs_gauss",
                &t.seg,
                &t.seg,
                &p.pm1,
                inv(&t.seg),
                [1.0, 0.0],
                stp * hw as f32 / pm_w as f32,
                0.0,
            );
            kit.blur(
                gpu,
                e,
                "fs_gauss",
                &p.pm1,
                &p.pm1,
                &p.pm2,
                inv(&p.pm1),
                [0.0, 1.0],
                stp,
                0.0,
            );
            // G9: packed into tone's alpha
            kit.blur(
                gpu,
                e,
                "fs_with_alpha",
                &t.tone,
                &p.pm2,
                &t.tone_pb,
                [0.0; 2],
                [0.0; 2],
                0.0,
                0.0,
            );
            tone = t.tone_pb.clone();
            seg_ok = true;
        }

        // --- makeup masks (lips / eye interior / brows), soft-edged ---
        let mask_draws = layout::mask_draws(face, &lay, face_weight, wf, hf);
        if let Some(draws) = &mask_draws {
            // M1: half-res target, ortho in full-frame px
            let mut geometry = Vec::new();
            let mut list = Vec::new();
            for d in draws {
                let first = (geometry.len() / 4) as u32;
                for p in &d.triangles {
                    geometry.extend([p[0], p[1], d.value, 0.0]);
                }
                let writes = match d.channel {
                    Channel::R => wgpu::ColorWrites::RED,
                    Channel::B => wgpu::ColorWrites::BLUE,
                    Channel::A => wgpu::ColorWrites::ALPHA,
                };
                list.push(Draw {
                    entry: "fs_solid",
                    writes,
                    vertices: first..(geometry.len() / 4) as u32,
                });
            }
            let mut u = [0.0f32; 12];
            u[6] = wf;
            u[7] = hf;
            kit.pass(
                gpu,
                e,
                &t.mask_raw,
                &u,
                &[&t.half, &t.half],
                Some(&geometry),
                &list,
                false,
            );
        }
        // M2: the mesh and the head pose
        let mesh_v = face.and_then(|f| mesh::update(f, &lay, face_weight, wf, hf, &mut st.pose));
        let mesh_ok = mesh_v.is_some();
        // M3, M4 (their frame-sized targets made only once a mesh is drawn)
        let (paint_t, fnorm_t) = if mesh_ok {
            (
                lazy(&mut t.paint, gpu, "looks paint", (w, h)),
                lazy(&mut t.fnorm, gpu, "looks facenorm", (w, h)),
            )
        } else {
            (t.smooth.clone(), t.smooth.clone())
        };
        let paint = mesh::render_paint(kit, gpu, e, maps, mesh_v.as_deref(), m, (w, h), &paint_t);
        let facenorm = mesh::render_facenorm(
            kit,
            gpu,
            e,
            maps,
            mesh_v.as_deref(),
            s.light_active(),
            (w, h),
            &fnorm_t,
        );
        // M5/M6
        if mask_draws.is_some() {
            let ms = (iod_half * 0.012).max(0.6);
            kit.blur(
                gpu,
                e,
                "fs_gauss",
                &t.mask_raw,
                &t.mask_raw,
                &t.mask_tmp,
                inv(&t.mask_raw),
                [1.0, 0.0],
                ms,
                0.0,
            );
            kit.blur(
                gpu,
                e,
                "fs_gauss",
                &t.mask_tmp,
                &t.mask_tmp,
                &t.mask,
                inv(&t.mask_tmp),
                [0.0, 1.0],
                ms,
                0.0,
            );
        }
        let has_mask = mask_draws.is_some();

        // --- final composite --- (F1)
        let li = s.light.unwrap_or_default();
        let light_amt = if s.light.is_some() { s.light_amt } else { 0.0 };
        let has_person = seg_ok; // seg && person_blur
        let pose = st.pose;
        let mut u = [0.0f32; 100];
        {
            let mut put = |i: usize, v: [f32; 4]| u[i * 4..i * 4 + 4].copy_from_slice(&v);
            put(
                0,
                [
                    wf,
                    hf,
                    if lay.has_face { face_weight } else { 0.0 },
                    lay.warp_count as f32,
                ],
            );
            put(1, lay.bounds);
            put(
                2,
                [
                    if s.all_skin { 1.0 } else { 0.0 },
                    en * s.smooth,
                    en * s.foundation,
                    en * s.skin_tone,
                ],
            );
            put(
                3,
                [en * s.pouch, en * s.teeth, en * s.eyelight, en * s.clarity],
            );
            put(
                4,
                [
                    s.debug_view as f32,
                    if paint { 1.0 } else { 0.0 },
                    en * m.filter_amt,
                    if has_mask { en * m.makeup_amt } else { 0.0 },
                ],
            );
            put(5, [m.exposure, m.contrast, m.saturation, m.warmth]);
            put(6, [m.tint, m.fade, 0.0, 0.0]);
            put(7, [m.lip[0], m.lip[1], m.lip[2], m.lip_opacity]);
            put(8, [m.blush[0], m.blush[1], m.blush[2], m.blush_opacity]);
            put(9, [m.shadow[0], m.shadow[1], m.shadow[2], m.shadow_opacity]);
            put(10, [0.0, m.contour, m.highlight, m.brow]);
            put(
                11,
                [
                    m.lip_gloss,
                    light_amt,
                    if has_person { 1.0 } else { 0.0 },
                    if facenorm { 1.0 } else { 0.0 },
                ],
            );
            put(12, [li.key_dir[0], li.key_dir[1], li.key_dir[2], 0.0]);
            put(
                13,
                [
                    li.key_color[0],
                    li.key_color[1],
                    li.key_color[2],
                    li.key_amt,
                ],
            );
            put(
                14,
                [
                    li.rim_color[0],
                    li.rim_color[1],
                    li.rim_color[2],
                    li.rim_amt,
                ],
            );
            put(15, [li.fill, li.bg_dim, li.exposure, li.warmth]);
            let pt = if has_person {
                [1.0 / pm_size.0 as f32, 1.0 / pm_size.1 as f32]
            } else {
                [0.0; 2]
            };
            put(
                16,
                [
                    pt[0],
                    pt[1],
                    if seg_ok { 1.0 } else { 0.0 },
                    if has_mask { 1.0 } else { 0.0 },
                ],
            );
            let pg = pigment;
            put(
                17,
                [
                    pg.mel[0],
                    pg.mel[1],
                    pg.mel[2],
                    if s.enhance_on { s.redness } else { 0.0 },
                ],
            );
            put(
                18,
                [
                    pg.hem[0],
                    pg.hem[1],
                    pg.hem[2],
                    if mesh_ok { pose.scale } else { 0.0 },
                ],
            );
            put(19, [pg.inv[0], pg.inv[1], pg.inv[2], 0.0]);
            put(20, [pg.inv[3], pg.inv[4], pg.inv[5], 0.0]);
            for (k, a) in pose.axes.iter().take(3).enumerate() {
                put(21 + k, [a[0], a[1], a[2], 0.0]);
            }
            put(24, [pose.center[0], pose.center[1], 0.0, 0.0]);
        }
        let mask_tex = if has_mask { &t.mask } else { &t.smooth };
        let paint_tex = if paint { &paint_t } else { &t.smooth };
        let seg_tex = if seg_ok { &t.seg } else { &tone };
        let skin_od_tex = if seg_ok { &t.skin_tone } else { &tone };
        kit.pass(
            gpu,
            e,
            &t.out,
            &u,
            &[
                camera,
                &t.smooth,
                &tone,
                &t.data,
                mask_tex,
                paint_tex,
                seg_tex,
                skin_od_tex,
                &t.smooth,
            ],
            None,
            &[Draw::all("fs_main", 0..3)],
            false,
        );
        let mut out = t.out.clone();

        // Finish: light + the look's colour grade (F2)
        let grade_on = en * m.filter_amt > 0.0;
        if s.debug_view == 0 && (s.light_active() || grade_on) {
            let fin = lazy(&mut t.fin, gpu, "looks finish", (w, h));
            let fnorm = if facenorm { &fnorm_t } else { &out };
            kit.pass(
                gpu,
                e,
                &fin,
                &u,
                &[
                    &out,
                    &t.smooth,
                    &tone,
                    &t.data,
                    mask_tex,
                    paint_tex,
                    seg_tex,
                    skin_od_tex,
                    fnorm,
                ],
                None,
                &[Draw::all("fs_finish", 0..3)],
                false,
            );
            out = fin;
        }

        // the Colour grade's LUT (P2)
        if let Some(g) = &s.filter
            && s.filter_amt > 0.0
        {
            let lut = luts
                .entry(s.filter_preset.clone())
                .or_insert_with(|| {
                    let n = grade::LUT_N as u32;
                    let px = grade::build_lut(g, grade::LUT_N);
                    Some(crate::gpu::texture_on(
                        gpu.device(),
                        gpu.queue(),
                        &px,
                        (n * n, n),
                    ))
                })
                .clone();
            if let Some(lut) = lut {
                let dst = lazy(&mut t.lut, gpu, "looks grade", (w, h));
                kit.pass(
                    gpu,
                    e,
                    &dst,
                    &[grade::LUT_N as f32, s.filter_amt, 0.0, 0.0],
                    &[&out, &lut],
                    None,
                    &[Draw::all("fs_filter_lut", 0..3)],
                    false,
                );
                out = dst;
            }
        }
        gpu.queue().submit([enc.finish()]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera with these effects, each with exactly the settings given (the rest are the
    /// catalogue's, as `studio_settings` fills them).
    fn source(effects: &[(&str, Value)]) -> Source {
        let mut s = Source::new("Cam", "lightair_camera", serde_json::json!({}));
        for (kind, own) in effects {
            let own = if own.is_null() {
                serde_json::json!({})
            } else {
                own.clone()
            };
            s.effects
                .push(lightair_model::Effect::new(*kind, *kind, own));
        }
        s
    }

    /// A camera with these effects as the catalogue creates them (every default stored).
    fn created(kinds: &[&str]) -> Source {
        let mut s = Source::new("Cam", "lightair_camera", serde_json::json!({}));
        for kind in kinds {
            s.effects
                .push(catalog::def(kind).expect("a catalogue effect").create());
        }
        s
    }

    /// The Looks screen's part switches inside Beauty: a part switched off draws nothing of its
    /// own, and its values come back with it when it's switched on again.
    #[test]
    fn a_beauty_part_switched_off_draws_nothing_and_keeps_its_values() {
        let own = |skin: bool, shape: bool, makeup: bool| {
            serde_json::json!({
                "smooth": 80, "face_slim": 100, "makeup_preset": "Berry",
                "skin_on": skin, "shape_on": shape, "makeup_on": makeup,
            })
        };
        let on =
            Settings::from_source(&source(&[("studio.beauty", own(true, true, true))])).unwrap();
        assert!(on.smooth > 0.0 && on.shape != Shape::default() && on.makeup.makeup_amt > 0.0);

        let off = Settings::from_source(&source(&[("studio.beauty", own(false, false, false))]));
        if let Some(off) = off {
            assert_eq!(off.smooth, 0.0, "no smoothing");
            assert_eq!(off.shape, Shape::default(), "no reshaping");
            assert_eq!(
                (off.makeup.makeup_amt, off.makeup.filter_amt),
                (0.0, 0.0),
                "no makeup"
            );
        }
        // each switch takes its own part only
        let no_skin =
            Settings::from_source(&source(&[("studio.beauty", own(false, true, true))])).unwrap();
        assert_eq!(no_skin.smooth, 0.0);
        assert_eq!(
            (no_skin.shape, no_skin.makeup.makeup_amt),
            (on.shape, on.makeup.makeup_amt)
        );
        // back on: the same values as before (they were kept in the effect's settings)
        let again =
            Settings::from_source(&source(&[("studio.beauty", own(true, true, true))])).unwrap();
        assert_eq!((again.smooth, again.shape), (on.smooth, on.shape));
    }

    #[test]
    fn the_effects_switched_on_decide_what_is_drawn() {
        assert!(Settings::from_source(&source(&[])).is_none());
        let mut off = created(&["studio.beauty"]);
        off.effects[0].enabled = false;
        assert!(Settings::from_source(&off).is_none(), "switched off");

        // beauty alone: drawn, with no light and no grade
        let s = Settings::from_source(&created(&["studio.beauty"])).unwrap();
        assert!(s.active() && !s.light_active());
        assert!(s.light.is_none() && s.filter.is_none());
        // without stored makeup values the look is "None": white, nothing on
        let bare = Settings::from_source(&source(&[("studio.beauty", Value::Null)])).unwrap();
        assert_eq!(bare.makeup.lip, [1.0; 3]);

        // all three: the light and the grade come on with their presets
        let s = Settings::from_source(&created(&["studio.beauty", "studio.light", "studio.grade"]))
            .unwrap();
        assert!(s.light_active() && s.light.is_some());
        assert_eq!(s.filter_preset, "Portra");
        assert!(s.filter.is_some() && s.filter_amt > 0.0);
    }

    #[test]
    fn sliders_presets_and_switches_map_to_the_looks_values() {
        let s = Settings::from_source(&source(&[
            (
                "studio.beauty",
                serde_json::json!({
                    "smooth": 150, "skin_tone": -40, "chin": -30, "eye_size": 25,
                    "contrast": 10, "shadows": 20, "enhance_on": false, "makeup_preset": "Berry",
                }),
            ),
            (
                "studio.light",
                serde_json::json!({"light_preset": "Neon", "light_amt": 40}),
            ),
            (
                "studio.grade",
                serde_json::json!({"filter_preset": "", "filter_preset_amt": 50}),
            ),
        ]))
        .unwrap();
        assert_eq!(s.smooth, 1.0, "held to 0..100");
        assert_eq!(
            (s.skin_tone, s.shape.chin, s.shape.eye_size),
            (-0.4, -0.3, 0.25)
        );
        assert_eq!((s.clarity, s.pouch), (0.1, 0.2));
        assert!(!s.enhance_on);
        // Berry's look applied: winged liner, its lip colour (#…), its grade
        assert_eq!(s.makeup.liner_style, 2);
        assert!(s.makeup.lip_opacity > 0.0);
        assert_eq!(s.makeup.contrast, 0.08);
        let neon = s.light.unwrap();
        assert_eq!((neon.rim_amt, neon.bg_dim), (0.9, 0.3));
        assert_eq!(s.light_amt, 0.4);
        assert!(s.filter.is_none(), "an empty grade preset is off");

        // a look tweaked by hand keeps the effect's own values (catalogue: makeup_applied)
        let t = Settings::from_source(&source(&[(
            "studio.beauty",
            serde_json::json!({"makeup_preset": "Berry", "mk_lip_opacity": 90, "mk_liner_style": 0}),
        )]))
        .unwrap();
        assert_eq!((t.makeup.lip_opacity, t.makeup.liner_style), (0.9, 0));
    }

    #[test]
    fn nothing_on_draws_nothing() {
        let s = Settings::from_source(&source(&[(
            "studio.beauty",
            serde_json::json!({"smooth": 0, "foundation": 0, "eyelight": 0}),
        )]))
        .unwrap();
        assert!(!s.active(), "the filter is skipped");
    }

    #[test]
    fn a_person_mask_turns_upright() {
        let m = crate::feeds::Mask {
            bytes: vec![1, 2, 3, 4, 5, 6],
            size: (3, 2),
            rotation: 1,
            mirrored: false,
            serial: 1,
        };
        let (rgba, size) = upright_person(&m).unwrap();
        assert_eq!(size, (2, 3));
        let a: Vec<u8> = rgba.chunks(4).map(|p| p[3]).collect();
        assert!(rgba.chunks(4).all(|p| p[..3] == [0, 0, 0]));
        // upright (x, y) reads the mask at (y, 1 - x): a quarter turn
        assert_eq!(a, vec![4, 1, 5, 2, 6, 3]);
    }

    fn pigment() -> Pigment {
        // the melanin and haemoglobin priors and the inverse of [mel hem U]
        let unit = |v: [f32; 3]| {
            let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            [v[0] / n, v[1] / n, v[2] / n]
        };
        let mel = unit([0.33, 0.51, 0.79]);
        let hem = unit([0.16, 0.79, 0.59]);
        let u3 = [0.577_350_26f32; 3];
        let c = [mel, hem, u3]; // columns
        let m = |r: usize, k: usize| c[k][r] as f64;
        let det = m(0, 0) * (m(1, 1) * m(2, 2) - m(1, 2) * m(2, 1))
            - m(0, 1) * (m(1, 0) * m(2, 2) - m(1, 2) * m(2, 0))
            + m(0, 2) * (m(1, 0) * m(2, 1) - m(1, 1) * m(2, 0));
        let mut inv = [0.0f32; 9];
        for r in 0..3 {
            for k in 0..3 {
                let (r1, r2, k1, k2) = ((k + 1) % 3, (k + 2) % 3, (r + 1) % 3, (r + 2) % 3);
                inv[r * 3 + k] = ((m(r1, k1) * m(r2, k2) - m(r1, k2) * m(r2, k1)) / det) as f32;
            }
        }
        Pigment {
            calibrated: false,
            mel,
            hem,
            inv,
        }
    }

    /// The synthetic face with a mesh: MediaPipe's canonical mesh, 2.5 px per cm, centred.
    fn face_with_mesh() -> Face {
        let mut f = layout::tests::synthetic_face();
        let r = super::super::Region::Mesh as usize;
        f.region_start[r] = f.pts.len();
        f.region_count[r] = mesh::VERTS;
        for p in &mesh::POS {
            f.pts
                .push([(32.0 + 2.5 * p[0]) / 64.0, (30.0 - 2.5 * p[1]) / 64.0]);
        }
        f
    }

    fn read(gpu: &Gpu, t: &wgpu::Texture) -> Vec<u8> {
        let (w, h) = (t.width(), t.height());
        let row = (w * 4).div_ceil(256) * 256;
        let buf = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device().create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            t.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue().submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        gpu.wait();
        let data = buf.slice(..).get_mapped_range().expect("mapped").to_vec();
        let mut out = Vec::new();
        for y in 0..h {
            out.extend_from_slice(&data[(y * row) as usize..(y * row + w * 4) as usize]);
        }
        out
    }

    #[test]
    fn the_whole_draw_runs_with_and_without_a_face_and_masks() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("no GPU: skipped");
            return;
        };
        let (w, h) = (64u32, 64u32);
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                // a skin-ish gradient
                px.extend([150 + (x / 2) as u8, 110 + (y / 2) as u8, 90, 255]);
            }
        }
        let camera = gpu.texture(&px, (w, h));
        let id = Id::new();
        let mut looks = Looks::new(&gpu);
        let pig = pigment();
        let none = Masks { seg: None };

        // beauty alone, no face, no masks: nothing is skin, so the picture comes back as it was
        let beauty = Settings::from_source(&created(&["studio.beauty"])).unwrap();
        let out = looks.draw(&gpu, id, &camera, &beauty, None, 0.0, &none, &pig, None);
        let got = read(&gpu, &out);
        let worst = got
            .iter()
            .zip(&px)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 2, "off by {worst}");

        // everything on: a face with its mesh, the multiclass mask, makeup, light and grade
        let all = Settings::from_source(&source(&[
            (
                "studio.beauty",
                serde_json::json!({
                    "makeup_preset": "Bronze Glam", "eye_size": 40, "face_slim": 30,
                    "shadows": 50, "teeth": 50, "contrast": 30, "redness": 40,
                }),
            ),
            (
                "studio.light",
                serde_json::json!({"light_preset": "Rembrandt"}),
            ),
            ("studio.grade", Value::Null),
        ]))
        .unwrap();
        let face = face_with_mesh();
        // the mesh path runs: some triangles face the camera
        let lay = layout::build(Some(&face), &Shape::default(), 64.0, 64.0, 1.0, 0);
        let verts = mesh::update(&face, &lay, 1.0, 64.0, 64.0, &mut Pose::default()).unwrap();
        assert!(verts.chunks(4).any(|v| v[0] != 0.0));
        let seg: Vec<u8> = (0..16 * 16).flat_map(|_| [200u8, 180, 10, 255]).collect();
        let masks = Masks {
            seg: Some((std::sync::Arc::new(seg), (16, 16), 1)),
        };
        let out = looks.draw(
            &gpu,
            id,
            &camera,
            &all,
            Some(&face),
            1.0,
            &masks,
            &pig,
            None,
        );
        let got = read(&gpu, &out);
        assert_eq!(got.len(), (w * h * 4) as usize);
        assert!(got.chunks(4).all(|p| p[3] == 255), "alpha kept");
        assert_ne!(got, px, "the looks changed the picture");

        // the face gone, a person mask only (person-only segmentation)
        let person = crate::feeds::Mask {
            bytes: vec![255; 32 * 32],
            size: (32, 32),
            rotation: 1,
            mirrored: true,
            serial: 9,
        };
        let out = looks.draw(
            &gpu,
            id,
            &camera,
            &all,
            None,
            0.0,
            &none,
            &pig,
            Some(&person),
        );
        assert_eq!((out.width(), out.height()), (w, h));
        let _ = read(&gpu, &out);

        // grade only: Finish is skipped, the LUT runs
        let grade = Settings::from_source(&created(&["studio.grade"])).unwrap();
        let out = looks.draw(&gpu, id, &camera, &grade, None, 0.0, &none, &pig, None);
        assert_ne!(read(&gpu, &out), px);
    }
}
