//! The compositor (docs/plans/screen-studio.md, 4.2): one result frame from the screen's frame,
//! the camera view (zoom), the pointer and its clicks, on the backdrop. `wgpu` (Metal on macOS,
//! Direct3D 12 on Windows), headless; the frame comes back as RGBA bytes.
//!
//! The backdrop and shadow use the same maths as the live `lightair_backdrop` source, and the
//! pointer the same shapes (`shapes`), so Live and the Editor look alike.

use std::collections::HashMap;

use crate::edit::Look;
use crate::shapes::{self, Bitmap, RES};
use crate::zoom::View;

/// The backdrops' colours (0xRRGGBB) and angle (degrees): the same list as the live source.
pub const BACKDROPS: &[([u32; 3], f32)] = &[
    ([0x1E1B4B, 0x6D28D9, 0xF472B6], 35.0),
    ([0x0B3B5C, 0x1D7FA8, 0x7DD3C0], 120.0),
    ([0x3B0A0A, 0xB4361C, 0xF6A04D], 60.0),
    ([0x0F3D3A, 0x2F9E7A, 0xC8F2D8], 150.0),
    ([0x111214, 0x2A2D33, 0x4A4F59], 90.0),
    ([0x8A6A4A, 0xD8BC94, 0xF3E6D0], 20.0),
];

const SHADER: &str = r#"
struct U {
    out_size: vec4<f32>,
    rect: vec4<f32>,
    uv: vec4<f32>,
    mode: vec4<f32>,
    pivot: vec4<f32>,
    c0: vec4<f32>,
    c1: vec4<f32>,
    c2: vec4<f32>,
    dir_shadow: vec4<f32>,
    shadow_rect: vec4<f32>,
    // x: masks in use, y: a mask's block size (texture pixels), z: sharpening (0..1)
    mask_info: vec4<f32>,
    masks: array<vec4<f32>, 4>,
    // picture layers: x: kept inside a box around the centre (turned with the layer), yz: its
    // half size (output pixels)
    clip: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct V { @builtin(position) pos: vec4<f32>, @location(0) local: vec2<f32>, @location(1) px: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    var corners = array<vec2<f32>, 6>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0));
    let local = corners[i];
    var p = u.rect.xy + local * u.rect.zw;
    // turned around the pivot
    let a = u.mode.w;
    let d = p - u.pivot.xy;
    p = u.pivot.xy + vec2(d.x * cos(a) - d.y * sin(a), d.x * sin(a) + d.y * cos(a));
    var out: V;
    out.pos = vec4(p.x / u.out_size.x * 2.0 - 1.0, 1.0 - p.y / u.out_size.y * 2.0, 0.0, 1.0);
    out.local = local;
    out.px = p;
    // a picture layer tilted in 3D, with perspective (as Lightair's libobs transform filter):
    // around the horizontal axis, then the vertical, the eye two picture-sizes away
    let tilt = u.dir_shadow.xy;
    if (u.mode.x > 2.5 && abs(tilt.x) + abs(tilt.y) > 0.0001) {
        let q = (local - vec2(0.5, 0.5)) * u.rect.zw;
        let y1 = q.y * cos(tilt.x);
        let z1 = q.y * sin(tilt.x);
        let x2 = q.x * cos(tilt.y) + z1 * sin(tilt.y);
        let z2 = -q.x * sin(tilt.y) + z1 * cos(tilt.y);
        let w = 1.0 + z2 / (max(u.rect.z, u.rect.w) * 2.0);
        let f = vec2(x2, y1) / w;
        let c = u.pivot.xy;
        let t = c + vec2(f.x * cos(a) - f.y * sin(a), f.x * sin(a) + f.y * cos(a));
        out.pos = vec4((t.x / u.out_size.x * 2.0 - 1.0) * w, (1.0 - t.y / u.out_size.y * 2.0) * w, 0.0, w);
        out.px = t;
    }
    return out;
}

// sRGB-encoded colour to linear light (a compositor that blends as libobs does: in linear
// light, into an sRGB target that encodes again)
fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((max(c, vec3(0.0)) + vec3(0.055)) / 1.055, vec3(2.4));
    return select(hi, lo, c <= vec3(0.04045));
}

fn rounded_box(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2(r, r);
    return length(max(q, vec2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    let mode = u.mode.x;
    if (mode < 0.5) {
        // backdrop: a three-colour gradient, and the shadow under the screen
        let uv = v.px / u.out_size.xy;
        let t = clamp(dot(uv - vec2(0.5, 0.5), u.dir_shadow.xy) + 0.5, 0.0, 1.0);
        let lo = mix(u.c0.rgb, u.c1.rgb, clamp(t * 2.0, 0.0, 1.0));
        let hi = mix(u.c1.rgb, u.c2.rgb, clamp(t * 2.0 - 1.0, 0.0, 1.0));
        var c = mix(lo, hi, step(0.5, t));
        let blur = u.dir_shadow.z;
        let centre = u.shadow_rect.xy + u.shadow_rect.zw * 0.5 + vec2(0.0, blur * 0.35);
        let d = rounded_box(v.px - centre, u.shadow_rect.zw * 0.5, u.mode.y);
        let s = 1.0 - smoothstep(-blur, blur, d);
        c = c * (1.0 - u.dir_shadow.w * s);
        return vec4(c, 1.0);
    }
    var uv = u.uv.xy + v.local * u.uv.zw;
    if (mode < 1.5) {
        let dims = vec2<f32>(textureDimensions(tex));
        // hidden areas: big blocks of colour
        var masked = false;
        for (var i = 0u; i < u32(u.mask_info.x); i++) {
            let m = u.masks[i];
            if (uv.x >= m.x && uv.y >= m.y && uv.x <= m.x + m.z && uv.y <= m.y + m.w) {
                masked = true;
            }
        }
        if (masked) {
            let cell = vec2(u.mask_info.y, u.mask_info.y) / dims;
            uv = (floor(uv / cell) + vec2(0.5, 0.5)) * cell;
        }
        var colour = textureSampleLevel(tex, samp, uv, 0.0);
        // zoomed in: contrast-adaptive sharpening (AMD's CAS)
        let sharp = u.mask_info.z;
        if (sharp > 0.0 && !masked) {
            let d = 1.0 / dims;
            let n = textureSampleLevel(tex, samp, uv - vec2(0.0, d.y), 0.0).rgb;
            let s = textureSampleLevel(tex, samp, uv + vec2(0.0, d.y), 0.0).rgb;
            let e = textureSampleLevel(tex, samp, uv + vec2(d.x, 0.0), 0.0).rgb;
            let w = textureSampleLevel(tex, samp, uv - vec2(d.x, 0.0), 0.0).rgb;
            let c = colour.rgb;
            let mn = min(c, min(min(n, s), min(e, w)));
            let mx = max(c, max(max(n, s), max(e, w)));
            let amp = sqrt(clamp(min(mn, vec3(1.0) - mx) / max(mx, vec3(1e-4)), vec3(0.0), vec3(1.0)));
            let wt = -amp / mix(8.0, 5.0, sharp);
            colour = vec4(clamp((c + (n + s + e + w) * wt) / (vec3(1.0) + 4.0 * wt), vec3(0.0), vec3(1.0)), colour.a);
        }
        // the screen, its corners rounded
        let half = u.rect.zw * 0.5;
        let d = rounded_box(v.px - (u.rect.xy + half), half, u.mode.y);
        let a = clamp(0.5 - d, 0.0, 1.0);
        return vec4(colour.rgb, 1.0) * a * u.mode.z;
    }
    let colour = textureSample(tex, samp, uv);
    if (mode > 2.5) {
        // a picture layer (straight alpha): its hidden areas pixelated, through its filter
        var c = colour;
        let dims = vec2<f32>(textureDimensions(tex));
        let uv0 = uv;
        for (var i = 0u; i < u32(u.mask_info.x); i++) {
            let m = u.masks[i];
            if (uv.x >= m.x && uv.y >= m.y && uv.x <= m.x + m.z && uv.y <= m.y + m.w) {
                let cell = vec2(u.mask_info.y, u.mask_info.y) / dims;
                uv = (floor(uv / cell) + vec2(0.5, 0.5)) * cell;
                c = textureSampleLevel(tex, samp, uv, 0.0);
            }
        }
        let blur = u.c1.y;
        if (blur > 0.0) {
            var sum = c;
            for (var i = 0; i < 12; i++) {
                let a = f32(i) * 0.5236;
                let r = blur * 0.02 * (1.0 + f32(i % 2));
                sum += textureSampleLevel(tex, samp, uv + vec2(cos(a), sin(a)) * r, 0.0);
            }
            c = sum / 13.0;
        }
        var rgb = c.rgb + vec3(u.c0.x);
        rgb = (rgb - vec3(0.5)) * u.c0.y + vec3(0.5);
        let lum = dot(rgb, vec3(0.299, 0.587, 0.114));
        rgb = mix(vec3(lum), rgb, u.c0.z);
        rgb += vec3(u.c0.w * 0.08, 0.0, -u.c0.w * 0.08);
        let edge = distance(v.local, vec2(0.5, 0.5)) * 1.414;
        rgb *= 1.0 - u.c1.x * smoothstep(0.3, 1.0, edge);
        rgb = clamp(rgb, vec3(0.0), vec3(1.0));
        let linear = u.out_size.z > 0.5;
        if (linear) {
            rgb = to_linear(rgb);
        }
        var alpha = c.a;
        // a cut-out (as Lightair's libobs shape mask): a rounded box around the crop window,
        // in the picture's pixels, its soft edge and its border the outermost pixels
        if (u.c2.x > 0.5) {
            let d = rounded_box(uv0 * dims - u.pivot.zw, u.dir_shadow.zw, u.c2.y);
            let edge = max(u.c2.z, 0.75);
            let inside = 1.0 - smoothstep(-edge, 0.0, d);
            if (u.c2.w > 0.0) {
                let ring = smoothstep(-u.c2.w - edge, -u.c2.w, d) * u.shadow_rect.a;
                var border = u.shadow_rect.rgb;
                if (linear) {
                    border = to_linear(border);
                }
                rgb = mix(rgb, border, ring);
                alpha = max(alpha, ring);
            }
            alpha = alpha * inside;
        }
        // where this pixel is in the layer's own (turned) frame, from its centre
        let turn = u.mode.w;
        let e = v.px - u.pivot.xy;
        let r = vec2(e.x * cos(turn) + e.y * sin(turn), -e.x * sin(turn) + e.y * cos(turn));
        // tilted: what grows past the picture's own box is cut off (the filter draws into it)
        let tilt = u.dir_shadow.xy;
        if (abs(tilt.x) + abs(tilt.y) > 0.0001) {
            if (abs(r.x) > u.rect.z * 0.5 + 0.5 || abs(r.y) > u.rect.w * 0.5 + 0.5) {
                alpha = 0.0;
            }
        }
        // kept inside its box (a picture covering it, the rest cut off)
        if (u.clip.x > 0.5) {
            let inside = clamp(u.clip.yz - abs(r) + vec2(0.5), vec2(0.0), vec2(1.0));
            alpha = alpha * inside.x * inside.y;
        }
        // a wipe: only the side of a line the layer is kept on (output pixels)
        let n = u.c1.zw;
        if (dot(n, n) > 0.0) {
            alpha = alpha * clamp(dot(v.px, n) - u.mask_info.w + 0.5, 0.0, 1.0);
        }
        return vec4(rgb * alpha, alpha) * u.mode.z;
    }
    // a sprite (premultiplied)
    if (u.out_size.z > 0.5 && colour.a > 0.0) {
        return vec4(to_linear(colour.rgb / colour.a) * colour.a, colour.a) * u.mode.z;
    }
    return colour * u.mode.z;
}
"#;

/// The pointer, where it is on the screen (fractions), as drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerDraw {
    pub x: f32,
    pub y: f32,
    /// `lightair_platform::input::Shape` as a number (0 arrow, 1 I-beam, 3 crosshair).
    pub shape: u8,
    pub opacity: f32,
    /// Degrees.
    pub tilt: f32,
}

/// One frame's content.
pub struct Frame<'a> {
    /// The screen's picture, RGBA, `screen_size`.
    pub screen: &'a [u8],
    pub screen_size: (u32, u32),
    pub view: View,
    pub pointer: Option<PointerDraw>,
    /// The pointer's size in result pixels at no zoom.
    pub pointer_px: f32,
    /// The pointer's look (0 pointer, 1 dot, 2 ring) and click effect (0 none, 1 circle, 2 ripple).
    pub pointer_style: i64,
    pub click_style: i64,
    /// Clicks to draw: where (fractions of the screen) and how far along (0..1).
    pub clicks: Vec<(f32, f32, f32)>,
    /// Areas hidden (x, y, w, h as fractions of the screen), up to four.
    pub masks: Vec<(f32, f32, f32, f32)>,
    /// Sharpening while zoomed in (0 none .. 1 most).
    pub sharpen: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Sprite {
    Arrow,
    IBeam,
    Cross,
    Dot,
    Ring,
    ClickRing,
    ClickDisc,
}

/// A picture in a frame of layers: RGBA (straight alpha), placed with its centre at `center`
/// (result pixels), drawn `draw` big, turned `rotation` degrees, at `opacity`.
/// A layer's pixels: RGBA bytes (uploaded), or (macOS, iOS) a decoder's IOSurface-backed
/// buffer, drawn over the same memory.
#[derive(Clone, Copy)]
pub enum Pixels<'a> {
    Rgba(&'a [u8]),
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Buffer(&'a crate::texture::PixelBuffer),
}

pub struct Layer<'a> {
    pub pixels: Pixels<'a>,
    pub size: (u32, u32),
    pub center: (f32, f32),
    pub draw: (f32, f32),
    pub rotation: f32,
    pub opacity: f32,
    /// Made pictures (text, shapes) come premultiplied, and take no filter.
    pub premultiplied: bool,
    pub filter: crate::sequence::Filter,
    /// Hidden areas: pixelated (fractions of the picture: x, y, w, h; at most 4).
    pub masks: &'a [(f32, f32, f32, f32)],
    /// The catalog's effects on it, and the clip's own time (seconds) for those that move.
    pub effects: &'a [crate::sequence::ClipEffect],
    pub time: f32,
    /// The part of the picture drawn: x, y, w, h (fractions; a negative size mirrors it).
    pub uv: (f32, f32, f32, f32),
    /// A cut-out of the picture (a scene object's shape).
    pub cut: Option<Cut>,
    /// 3D tilt in degrees, with perspective: around the horizontal axis (positive: the bottom
    /// recedes), then the vertical (positive: the left side recedes). What grows past the
    /// layer's box is cut off.
    pub tilt: (f32, f32),
    /// Only the part on one side of a line is drawn (a wipe): `(nx, ny, offset)`, kept where
    /// `nx * x + ny * y >= offset` in output pixels (`(nx, ny)` a unit vector).
    pub keep: Option<(f32, f32, f32)>,
    /// Only what's inside a box around the centre, turned with the layer, is drawn: its half
    /// size in output pixels (a picture filling its box, the overflow cut off after a tilt).
    pub bounds: Option<(f32, f32)>,
}

/// A cut-out of a layer's picture, as Lightair's libobs shape mask makes it: a rounded box in
/// the picture's own pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cut {
    /// The box's centre and half size, in the picture's pixels (its crop window).
    pub centre: (f32, f32),
    pub half: (f32, f32),
    /// Corner radius, soft edge and border width, in the picture's pixels.
    pub radius: f32,
    pub feather: f32,
    pub border: f32,
    /// The border's colour: straight RGBA, 0..1.
    pub border_color: [f32; 4],
}

/// Which texture a draw samples.
enum Tex {
    Blank,
    Screen,
    Sprite(Sprite),
    Slot(usize),
    /// A slot's picture through its effects.
    Fx(usize),
}

pub struct Compositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    target: wgpu::Texture,
    readback: wgpu::Buffer,
    size: (u32, u32),
    look: Look,
    blank: wgpu::Texture,
    screen: Option<(wgpu::Texture, (u32, u32))>,
    sprites: HashMap<Sprite, (wgpu::Texture, (f32, f32))>,
    /// Picture layers' textures, by position in the frame.
    slots: Vec<(wgpu::Texture, (u32, u32))>,
    /// macOS, iOS: the decoders' buffers drawn in the last frame (their memory is the slots'
    /// textures), and which slots hold such a texture.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    held: Vec<crate::texture::PixelBuffer>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    imported: std::collections::HashSet<usize>,
    /// Clip effects, made when first used, and each slot's picture through its effects.
    fx: Option<crate::fx::Fx>,
    fx_out: Vec<Option<wgpu::Texture>>,
    /// macOS: frames drawn into an IOSurface for the window (no read back).
    #[cfg(target_os = "macos")]
    surface: Option<crate::surface::Target>,
    /// Its frames are BGRA (an IOSurface can be drawn into).
    #[cfg(target_os = "macos")]
    bgra: bool,
    /// Windows: frames presented into a DirectComposition visual's swap chain (the window's
    /// player, under its UI), not read back.
    #[cfg(windows)]
    visual: Option<wgpu::Surface<'static>>,
    #[cfg(windows)]
    visual_config: Option<wgpu::SurfaceConfiguration>,
    #[cfg(windows)]
    bgra_out: bool,
    /// iOS, macOS: frames drawn into a wgpu surface on a `CAMetalLayer` the caller shows (a
    /// phone's player) and presented there, not read back; and the format frames are drawn in.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    layer: Option<wgpu::Surface<'static>>,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    layer_config: Option<wgpu::SurfaceConfiguration>,
    /// Picture layers blended in linear light into an sRGB target ([`Compositor::new_linear`]).
    linear: bool,
    /// BGRA frames asked for where the GPU can't draw BGRA: drawn RGBA, swapped on read-back.
    swap: bool,
    /// The last frame was for a layer or visual that gave no texture to draw into: it wasn't
    /// shown, and the caller should draw it again ([`Compositor::missed`]).
    missed: bool,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    out_format: wgpu::TextureFormat,
    /// Frames start transparent instead of black (an export that keeps transparency).
    transparent: bool,
}

/// Windows: set when a visual's swap chain was (re)made: DirectComposition shows it once the
/// window's composition is committed (the app does, and clears this).
#[cfg(windows)]
pub static VISUAL_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn rgb(c: u32) -> [f32; 4] {
    [((c >> 16) & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, (c & 0xFF) as f32 / 255.0, 1.0]
}

/// The largest box of aspect `aspect` (w/h) in (x, y, w, h), centred.
pub fn fit(aspect: f32, x: f32, y: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
    let (bw, bh) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
    (x + (w - bw) / 2.0, y + (h - bh) / 2.0, bw, bh)
}

/// The graphics device, made once (making one takes a while) and shared by every compositor.
/// The graphics APIs the app asked for ([`set_backends`]).
static BACKENDS: std::sync::OnceLock<wgpu::Backends> = std::sync::OnceLock::new();

/// Which graphics APIs lightair-render may use, asked before its first frame (false: too late,
/// the device is made). `WGPU_BACKEND` still wins, for testing. Without it: Direct3D 12 on
/// Windows, Vulkan then OpenGL ES on Android, wgpu's choice elsewhere.
pub fn set_backends(backends: wgpu::Backends) -> bool {
    BASE.get().is_none() && BACKENDS.set(backends).is_ok()
}

/// The instance and adapter everything is made with: the first of the wanted APIs that has an
/// adapter.
static BASE: std::sync::OnceLock<Result<(wgpu::Instance, wgpu::Adapter), String>> = std::sync::OnceLock::new();

fn base() -> Result<&'static (wgpu::Instance, wgpu::Adapter), String> {
    BASE.get_or_init(|| {
        let tries: Vec<wgpu::Backends> = match (wgpu::Backends::from_env(), BACKENDS.get()) {
            (Some(env), _) => vec![env],
            (None, Some(b)) => vec![*b],
            // Windows: Direct3D 12, which can present into a DirectComposition visual (the player)
            _ if cfg!(windows) && std::env::var_os("LIGHTAIR_EDITOR_VULKAN").is_none() => vec![wgpu::Backends::DX12],
            // Android: Vulkan where there is one (an emulator's OpenGL ES adapter would be picked
            // over its Vulkan otherwise), else OpenGL ES; a software Vulkan (an emulator's,
            // which falls over) only when there's no OpenGL ES either
            _ if cfg!(target_os = "android") => vec![wgpu::Backends::VULKAN, wgpu::Backends::GL],
            _ => vec![wgpu::Backends::all()],
        };
        let mut last = String::from("no graphics adapter");
        let mut software: Option<(wgpu::Instance, wgpu::Adapter)> = None;
        let count = tries.len();
        for (i, backends) in tries.into_iter().enumerate() {
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
            desc.backends = backends;
            // Android: no debug labels or validation layers (an emulator's Vulkan driver falls
            // over on them); WGPU_DEBUG and the like still turn them on
            if cfg!(target_os = "android") {
                desc.flags = wgpu::InstanceFlags::empty();
            }
            desc.flags = desc.flags.with_env();
            let instance = wgpu::Instance::new(desc);
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })) {
                Ok(adapter) => {
                    let info = adapter.get_info();
                    if info.device_type == wgpu::DeviceType::Cpu && i + 1 < count {
                        software.get_or_insert((instance, adapter));
                        continue;
                    }
                    tracing::info!(name = info.name, backend = ?info.backend, kind = ?info.device_type, "lightair-render's adapter");
                    return Ok((instance, adapter));
                }
                Err(e) => last = format!("no graphics adapter ({backends:?}): {e}"),
            }
        }
        software.ok_or(last)
    })
    .as_ref()
    .map_err(String::clone)
}

#[cfg(any(target_os = "ios", target_os = "macos", windows))]
fn instance() -> &'static wgpu::Instance {
    &base().expect("an instance is only asked for once a compositor exists").0
}

/// The adapter the shared device was made on.
static ADAPTER: std::sync::OnceLock<wgpu::Adapter> = std::sync::OnceLock::new();

fn gpu() -> Result<(wgpu::Device, wgpu::Queue), String> {
    static GPU: std::sync::OnceLock<Result<(wgpu::Device, wgpu::Queue), String>> = std::sync::OnceLock::new();
    GPU.get_or_init(|| {
        let adapter = base()?.1.clone();
        let _ = ADAPTER.set(adapter.clone());
        // what this adapter offers, not wgpu's defaults: those ask for more than some adapters have
        // (16 inter-stage shader variables against the iOS simulator's and some older GPUs' 15),
        // and then there's no device at all
        let desc = wgpu::DeviceDescriptor { required_limits: adapter.limits(), ..Default::default() };
        pollster::block_on(adapter.request_device(&desc)).map_err(|e| format!("no graphics device: {e}"))
    })
    .clone()
}

impl Compositor {
    /// A compositor making `size` frames with this look (RGBA).
    pub fn new(size: (u32, u32), look: Look) -> Result<Compositor, String> {
        Self::new_with(size, look, false)
    }

    /// A compositor whose frames are BGRA when `bgra` (as a window draws them: no swapping).
    pub fn new_with(size: (u32, u32), look: Look, bgra: bool) -> Result<Compositor, String> {
        Self::new_in(size, look, bgra, false)
    }

    /// A compositor that blends picture layers in linear light, as libobs does (a scene's
    /// see-through objects and soft edges come out as on the desktop). Its frames are still
    /// sRGB-encoded bytes. Reads frames back or presents into a layer; no IOSurface or visual.
    pub fn new_linear(size: (u32, u32), bgra: bool) -> Result<Compositor, String> {
        Self::new_in(size, Look::default(), bgra, true)
    }

    fn new_in(size: (u32, u32), look: Look, bgra: bool, linear: bool) -> Result<Compositor, String> {
        let (device, queue) = gpu()?;
        // BGRA where the GPU draws and reads it back; OpenGL ES (which says it can, then reads
        // BGRA back as zeros) and any that can't: RGBA, the bytes swapped as it's read back
        let wanted = if linear { wgpu::TextureFormat::Bgra8UnormSrgb } else { wgpu::TextureFormat::Bgra8Unorm };
        let swap = bgra
            && !ADAPTER.get().is_some_and(|a| {
                let usage = a.get_texture_format_features(wanted).allowed_usages;
                a.get_info().backend != wgpu::Backend::Gl
                    && usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC)
            });
        let out_format = match (bgra && !swap, linear) {
            (true, false) => wgpu::TextureFormat::Bgra8Unorm,
            (false, false) => wgpu::TextureFormat::Rgba8Unorm,
            (true, true) => wgpu::TextureFormat::Bgra8UnormSrgb,
            (false, true) => wgpu::TextureFormat::Rgba8UnormSrgb,
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-render"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lightair-render"),
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
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: out_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: out_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (padded_row(size.0) * size.1) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut c = Compositor {
            blank: Self::texture_of(&device, &queue, &[0, 0, 0, 0], (1, 1)),
            device,
            queue,
            pipeline,
            layout,
            sampler,
            target,
            readback,
            size,
            look,
            screen: None,
            sprites: HashMap::new(),
            slots: Vec::new(),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            held: Vec::new(),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            imported: Default::default(),
            fx: None,
            fx_out: Vec::new(),
            #[cfg(target_os = "macos")]
            surface: None,
            #[cfg(target_os = "macos")]
            bgra,
            #[cfg(windows)]
            visual: None,
            #[cfg(windows)]
            visual_config: None,
            #[cfg(windows)]
            bgra_out: bgra,
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            layer: None,
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            layer_config: None,
            missed: false,
            linear,
            swap,
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            out_format,
            transparent: false,
        };
        let pointer = rgb(0x0090FF);
        let col = [pointer[0], pointer[1], pointer[2]];
        let mk: [(Sprite, Bitmap); 7] = [
            (Sprite::Arrow, shapes::arrow()),
            (Sprite::IBeam, shapes::ibeam()),
            (Sprite::Cross, shapes::crosshair()),
            (Sprite::Dot, shapes::dot(col)),
            (Sprite::Ring, shapes::ring(col, 0.36, 0.08)),
            (Sprite::ClickRing, shapes::ring([1.0, 1.0, 1.0], 0.44, 0.05)),
            (Sprite::ClickDisc, shapes::disc(col)),
        ];
        for (s, b) in mk {
            let t = Self::texture_of(&c.device, &c.queue, &b.rgba, (RES as u32, RES as u32));
            c.sprites.insert(s, (t, b.hot));
        }
        Ok(c)
    }

    fn texture_of(device: &wgpu::Device, queue: &wgpu::Queue, rgba: &[u8], size: (u32, u32)) -> wgpu::Texture {
        let t = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Self::upload(queue, &t, rgba, size);
        t
    }

    fn upload(queue: &wgpu::Queue, t: &wgpu::Texture, rgba: &[u8], size: (u32, u32)) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: t,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * size.0), rows_per_image: Some(size.1) },
            wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        );
    }

    /// Where the screen sits in the result (x, y, w, h), for a screen of `aspect`.
    pub fn screen_box(&self, aspect: f32) -> (f32, f32, f32, f32) {
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let pad = h * self.look.padding;
        fit(aspect, pad, pad, w - 2.0 * pad, h - 2.0 * pad)
    }

    /// Composites one frame; RGBA, top row first.
    pub fn render(&mut self, f: &Frame) -> Vec<u8> {
        // the screen's texture, made again only when its size changes
        if self.screen.as_ref().is_none_or(|(_, s)| *s != f.screen_size) {
            let t = Self::texture_of(&self.device, &self.queue, f.screen, f.screen_size);
            self.screen = Some((t, f.screen_size));
        } else if let Some((t, s)) = &self.screen {
            Self::upload(&self.queue, t, f.screen, *s);
        }
        let (ow, oh) = (self.size.0 as f32, self.size.1 as f32);
        let aspect = f.screen_size.0 as f32 / f.screen_size.1.max(1) as f32;
        let bx = self.screen_box(aspect);
        let radius = bx.2.min(bx.3) * self.look.radius;
        let (colours, angle) = BACKDROPS[(self.look.backdrop.max(0) as usize).min(BACKDROPS.len() - 1)];
        let a = angle.to_radians();
        let base = |rect: [f32; 4]| {
            let mut u = [0f32; 64];
            u[0..4].copy_from_slice(&[ow, oh, 0.0, 0.0]);
            u[4..8].copy_from_slice(&rect);
            u[8..12].copy_from_slice(&[0.0, 0.0, 1.0, 1.0]);
            u
        };
        // draws: (uniforms, texture)
        let mut draws: Vec<([f32; 64], Tex)> = Vec::new();
        let mut back = base([0.0, 0.0, ow, oh]);
        back[12..16].copy_from_slice(&[0.0, radius, 1.0, 0.0]);
        back[20..24].copy_from_slice(&rgb(colours[0]));
        back[24..28].copy_from_slice(&rgb(colours[1]));
        back[28..32].copy_from_slice(&rgb(colours[2]));
        back[32..36].copy_from_slice(&[a.cos(), a.sin(), oh * 0.035, self.look.shadow]);
        back[36..40].copy_from_slice(&[bx.0, bx.1, bx.2, bx.3]);
        draws.push((back, Tex::Blank));
        let (vx, vy, vw, vh) = f.view.rect();
        let mut screen = base([bx.0, bx.1, bx.2, bx.3]);
        screen[8..12].copy_from_slice(&[vx, vy, vw, vh]);
        screen[12..16].copy_from_slice(&[1.0, radius, 1.0, 0.0]);
        let masks: Vec<_> = f.masks.iter().take(4).collect();
        // blocks about 1/60 of the screen's width
        screen[40..44].copy_from_slice(&[
            masks.len() as f32,
            (f.screen_size.0 as f32 / 60.0).max(4.0),
            f.sharpen.clamp(0.0, 1.0),
            0.0,
        ]);
        for (k, m) in masks.iter().enumerate() {
            screen[44 + k * 4..48 + k * 4].copy_from_slice(&[m.0, m.1, m.2, m.3]);
        }
        draws.push((screen, Tex::Screen));
        // screen fractions → result pixels, through the view
        let place = |x: f32, y: f32| (bx.0 + (x - vx) / vw * bx.2, bx.1 + (y - vy) / vh * bx.3);
        let inside = |p: (f32, f32)| p.0 >= bx.0 && p.0 <= bx.0 + bx.2 && p.1 >= bx.1 && p.1 <= bx.1 + bx.3;
        let size = f.pointer_px * f.view.scale;
        let sprite = |s: Sprite, at: (f32, f32), px: f32, hot: (f32, f32), angle: f32, opacity: f32| {
            let mut u = base([at.0 - hot.0 * px, at.1 - hot.1 * px, px, px]);
            u[12..16].copy_from_slice(&[2.0, 0.0, opacity, angle.to_radians()]);
            u[16..20].copy_from_slice(&[at.0, at.1, 0.0, 0.0]);
            (u, Tex::Sprite(s))
        };
        for &(x, y, k) in &f.clicks {
            let at = place(x, y);
            if !inside(at) || f.click_style == 0 {
                continue;
            }
            let (s, grow, fade) = if f.click_style == 1 {
                (Sprite::ClickDisc, 0.9 + 0.3 * k, 0.45)
            } else {
                (Sprite::ClickRing, 0.6 + 1.4 * k, 0.9)
            };
            draws.push(sprite(s, at, size * 1.6 * grow, (0.5, 0.5), 0.0, fade * (1.0 - k)));
        }
        if let Some(p) = f.pointer {
            let at = place(p.x, p.y);
            if inside(at) && p.opacity > 0.001 {
                let (s, tilt) = match (f.pointer_style, p.shape) {
                    (1, _) => (Sprite::Dot, 0.0),
                    (2, _) => (Sprite::Ring, 0.0),
                    (_, 1) => (Sprite::IBeam, 0.0),
                    (_, 3) => (Sprite::Cross, 0.0),
                    _ => (Sprite::Arrow, p.tilt),
                };
                let hot = self.sprites[&s].1;
                draws.push(sprite(s, at, size, hot, tilt, p.opacity));
            }
        }

        self.draw(draws)
    }

    /// A frame of picture layers on black, back to front.
    /// Frames start transparent (`true`) or black: where nothing covers the frame, it's see-
    /// through. Pixels come premultiplied either way (over black, the same picture).
    pub fn set_transparent(&mut self, transparent: bool) {
        self.transparent = transparent;
    }

    pub fn layers(&mut self, layers: &[Layer]) -> Vec<u8> {
        let (ow, oh) = (self.size.0 as f32, self.size.1 as f32);
        // the buffers drawn last time: the GPU is done with them now (each frame waits for it)
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        self.held.clear();
        for (i, l) in layers.iter().enumerate() {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let read_back: Vec<u8>;
            let rgba = match l.pixels {
                Pixels::Rgba(b) => b,
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                Pixels::Buffer(b) => {
                    // the decoder's own memory as the slot's texture, kept until it's drawn (a
                    // slot that goes back to bytes then gets a texture of its own again)
                    match b.texture(&self.device) {
                        Some(t) => {
                            let size = b.size();
                            if i < self.slots.len() {
                                self.slots[i] = (t, size);
                            } else {
                                self.slots.push((t, size));
                            }
                            self.held.push(b.clone());
                            self.imported.insert(i);
                            continue;
                        }
                        None => {
                            read_back = b.rgba().unwrap_or_else(|| vec![0; (l.size.0 * l.size.1 * 4) as usize]);
                            &read_back
                        }
                    }
                }
            };
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let imported = self.imported.remove(&i);
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            let imported = false;
            if !imported && self.slots.get(i).is_some_and(|(_, s)| *s == l.size) {
                Self::upload(&self.queue, &self.slots[i].0, rgba, l.size);
            } else {
                let t = Self::texture_of(&self.device, &self.queue, rgba, l.size);
                if i < self.slots.len() {
                    self.slots[i] = (t, l.size);
                } else {
                    self.slots.push((t, l.size));
                }
            }
        }
        // effects, on the GPU, before they're drawn
        self.fx_out.clear();
        for (i, l) in layers.iter().enumerate() {
            let out = if l.effects.iter().any(|e| e.on) {
                let fx = self.fx.get_or_insert_with(|| crate::fx::Fx::new(&self.device, &self.queue));
                fx.run(&self.slots[i].0, l.size, l.effects, l.time)
            } else {
                None
            };
            self.fx_out.push(out);
        }
        let draws = layers
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let mut u = [0f32; 64];
                u[0..4].copy_from_slice(&[ow, oh, if self.linear { 1.0 } else { 0.0 }, 0.0]);
                u[4..8].copy_from_slice(&[
                    l.center.0 - l.draw.0 / 2.0,
                    l.center.1 - l.draw.1 / 2.0,
                    l.draw.0,
                    l.draw.1,
                ]);
                u[8..12].copy_from_slice(&[l.uv.0, l.uv.1, l.uv.2, l.uv.3]);
                let mode = if l.premultiplied { 2.0 } else { 3.0 };
                u[12..16].copy_from_slice(&[mode, 0.0, l.opacity.clamp(0.0, 1.0), l.rotation.to_radians()]);
                u[16..20].copy_from_slice(&[l.center.0, l.center.1, 0.0, 0.0]);
                let f = l.filter;
                u[20..24].copy_from_slice(&[f.brightness, f.contrast, f.saturation, f.temperature]);
                u[24..28].copy_from_slice(&[f.vignette, f.blur, 0.0, 0.0]);
                if let Some(c) = l.cut {
                    u[18..20].copy_from_slice(&[c.centre.0, c.centre.1]);
                    u[28..32].copy_from_slice(&[1.0, c.radius, c.feather, c.border]);
                    u[34..36].copy_from_slice(&[c.half.0, c.half.1]);
                    u[36..40].copy_from_slice(&c.border_color);
                }
                u[32..34].copy_from_slice(&[l.tilt.0.to_radians(), l.tilt.1.to_radians()]);
                let masks: Vec<_> = l.masks.iter().take(4).collect();
                u[40..44].copy_from_slice(&[masks.len() as f32, (l.size.0 as f32 / 40.0).max(4.0), 0.0, 0.0]);
                for (k, m) in masks.iter().enumerate() {
                    u[44 + k * 4..48 + k * 4].copy_from_slice(&[m.0, m.1, m.2, m.3]);
                }
                if let Some((hw, hh)) = l.bounds {
                    u[60..64].copy_from_slice(&[1.0, hw, hh, 0.0]);
                }
                // (after the masks: their info shares the offset's vector)
                if let Some((nx, ny, offset)) = l.keep {
                    u[26..28].copy_from_slice(&[nx, ny]);
                    u[43] = offset;
                }
                (u, if self.fx_out[i].is_some() { Tex::Fx(i) } else { Tex::Slot(i) })
            })
            .collect();
        self.draw(draws)
    }

    /// macOS: frames go into an IOSurface from now on (see [`Compositor::surface_frame`]), not
    /// read back; for a BGRA compositor. False if it couldn't be made.
    #[cfg(target_os = "macos")]
    pub fn attach_surface(&mut self) -> bool {
        if !self.bgra || self.linear {
            return false;
        }
        self.surface = crate::surface::Target::new(&self.device, self.size);
        self.surface.is_some()
    }

    /// macOS: the last frame drawn into the IOSurface, ready for the window.
    #[cfg(target_os = "macos")]
    pub fn surface_frame(&self) -> Option<crate::surface::Frame> {
        self.surface.as_ref()?.frame()
    }

    /// iOS, macOS: draws frames into `layer` (a `CAMetalLayer*` the caller keeps alive and shows)
    /// and presents them there instead of reading them back; [`Compositor::layers`] then returns
    /// nothing. The layer's drawable is the compositor's size, in its format.
    ///
    /// # Safety
    /// `layer` must be a valid `CAMetalLayer` for as long as this compositor draws into it.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub unsafe fn attach_layer(&mut self, layer: *mut std::ffi::c_void) -> bool {
        if layer.is_null() {
            return false;
        }
        let surface =
            match unsafe { instance().create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer)) } {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%e, "the player's layer");
                    return false;
                }
            };
        let Some(adapter) = ADAPTER.get() else { return false };
        let caps = surface.get_capabilities(adapter);
        if !caps.formats.contains(&self.out_format) {
            tracing::warn!(format = ?self.out_format, "the player's layer can't take the frames' format");
            return false;
        }
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.out_format,
            width: self.size.0,
            height: self.size.1,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
                wgpu::CompositeAlphaMode::Opaque
            } else {
                caps.alpha_modes[0]
            },
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        if !configure(&surface, &self.device, &config) {
            return false;
        }
        self.layer = Some(surface);
        self.layer_config = Some(config);
        true
    }

    /// iOS, macOS: back to reading frames back.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub fn detach_layer(&mut self) {
        self.layer = None;
        self.layer_config = None;
    }

    /// Whether the last frame drawn for a layer (or Windows visual) couldn't be presented: the
    /// screen still shows the one before, so draw it again (after a moment, as with any frame
    /// the screen isn't ready for).
    pub fn missed(&self) -> bool {
        self.missed
    }

    /// iOS, macOS: whether frames are presented into a layer.
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub fn presents_into_layer(&self) -> bool {
        self.layer.is_some()
    }

    /// Windows: frames are presented into `visual` (an `IDCompositionVisual*`, kept alive by the
    /// caller while this lives) from now on, not read back; for a BGRA compositor. False if the
    /// surface couldn't be made (the frames are read back as before).
    #[cfg(windows)]
    pub fn attach_visual(&mut self, visual: *mut std::ffi::c_void) -> bool {
        if !self.bgra_out || self.linear || visual.is_null() {
            return false;
        }
        let surface =
            match unsafe { instance().create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(visual)) } {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%e, "the player's composition surface");
                    return false;
                }
            };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8Unorm,
            width: self.size.0,
            height: self.size.1,
            // the player paces itself; a frame never waits for the screen
            present_mode: wgpu::PresentMode::Immediate,
            desired_maximum_frame_latency: 1,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let Some(adapter) = ADAPTER.get() else { return false };
        let caps = surface.get_capabilities(adapter);
        let config = wgpu::SurfaceConfiguration {
            present_mode: if caps.present_modes.contains(&config.present_mode) {
                config.present_mode
            } else {
                wgpu::PresentMode::Fifo
            },
            alpha_mode: if caps.alpha_modes.contains(&config.alpha_mode) {
                config.alpha_mode
            } else {
                caps.alpha_modes[0]
            },
            ..config
        };
        if !configure(&surface, &self.device, &config) {
            return false;
        }
        tracing::info!(size = ?self.size, mode = ?config.present_mode, "the Editor's frames are presented under the UI");
        self.visual = Some(surface);
        self.visual_config = Some(config);
        VISUAL_CHANGED.store(true, std::sync::atomic::Ordering::Release);
        true
    }

    /// Windows: whether frames go to a visual.
    #[cfg(windows)]
    pub fn presents(&self) -> bool {
        self.visual.is_some()
    }

    /// Draws, then reads the frame back: RGBA (BGRA for a BGRA compositor), top row first;
    /// nothing when it draws into an IOSurface.
    fn draw(&mut self, draws: Vec<([f32; 64], Tex)>) -> Vec<u8> {
        // a layer's drawable, when frames are presented into one
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let drawable = match (&self.layer, &self.layer_config) {
            (Some(s), Some(c)) => acquire(s, &self.device, c),
            _ => None,
        };
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        {
            self.missed = self.layer.is_some() && drawable.is_none();
        }
        #[cfg(target_os = "macos")]
        let view = match (&drawable, &self.surface) {
            (Some(d), _) => d.texture.create_view(&Default::default()),
            (None, Some(s)) => s.texture.create_view(&Default::default()),
            (None, None) => self.target.create_view(&Default::default()),
        };
        #[cfg(target_os = "ios")]
        let view = match &drawable {
            Some(d) => d.texture.create_view(&Default::default()),
            None => self.target.create_view(&Default::default()),
        };
        #[cfg(windows)]
        let presented = match (&self.visual, &self.visual_config) {
            (Some(s), Some(c)) => acquire(s, &self.device, c),
            _ => None,
        };
        #[cfg(windows)]
        {
            self.missed = self.visual.is_some() && presented.is_none();
        }
        #[cfg(windows)]
        let view = match &presented {
            Some(frame) => frame.texture.create_view(&Default::default()),
            None => self.target.create_view(&Default::default()),
        };
        #[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
        let view = self.target.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let groups: Vec<wgpu::BindGroup> = draws
            .iter()
            .map(|(u, t)| {
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: (u.len() * 4) as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bytes: Vec<u8> = u.iter().flat_map(|v| v.to_le_bytes()).collect();
                self.queue.write_buffer(&buffer, 0, &bytes);
                let tex = match t {
                    Tex::Sprite(s) => &self.sprites[s].0,
                    Tex::Screen => self.screen.as_ref().map_or(&self.blank, |(t, _)| t),
                    Tex::Slot(i) => self.slots.get(*i).map_or(&self.blank, |(t, _)| t),
                    Tex::Fx(i) => self.fx_out.get(*i).and_then(Option::as_ref).unwrap_or(&self.blank),
                    Tex::Blank => &self.blank,
                };
                let tv = tex.create_view(&Default::default());
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&tv) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                })
            })
            .collect();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(if self.transparent {
                            wgpu::Color::TRANSPARENT
                        } else {
                            wgpu::Color::BLACK
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            for g in &groups {
                pass.set_bind_group(0, g, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        if let Some(d) = drawable {
            self.queue.submit([encoder.finish()]);
            self.queue.present(d);
            // a decoder's buffers drawn: done with before they go back to it
            if !self.held.is_empty() {
                let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
            }
            return Vec::new();
        }
        #[cfg(target_os = "macos")]
        if self.surface.is_some() {
            // the IOSurface is complete once the GPU is done
            self.queue.submit([encoder.finish()]);
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
            return Vec::new();
        }
        #[cfg(windows)]
        if let Some(frame) = presented {
            self.queue.submit([encoder.finish()]);
            self.queue.present(frame);
            return Vec::new();
        }
        let row = padded_row(self.size.0);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(self.size.1),
                },
            },
            wgpu::Extent3d { width: self.size.0, height: self.size.1, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let mut out = Vec::with_capacity((self.size.0 * self.size.1 * 4) as usize);
        {
            let Ok(data) = slice.get_mapped_range() else { return out };
            for y in 0..self.size.1 as usize {
                let start = y * row as usize;
                out.extend_from_slice(&data[start..start + self.size.0 as usize * 4]);
            }
            if self.swap {
                for px in out.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
            }
        }
        self.readback.unmap();
        out
    }
}

/// Configures `surface`, false when it can't be now (the GPU too busy to wait for, a layer
/// gone): an error caught here, not wgpu's panic.
#[cfg(any(target_os = "ios", target_os = "macos", windows))]
fn configure(surface: &wgpu::Surface<'static>, device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) -> bool {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    surface.configure(device, config);
    match pollster::block_on(scope.pop()) {
        None => true,
        Some(e) => {
            tracing::warn!(%e, "the player's surface couldn't be configured");
            false
        }
    }
}

/// A texture to draw a frame into from a layer's or visual's surface. A frame mustn't be lost
/// quietly: the screen would keep the one before (as when a clip's first effect is added, its
/// passes built while the layer waits). So it asks again after a timeout, and reconfigures and
/// asks again when the surface is outdated; `None` only when there's still none.
#[cfg(any(target_os = "ios", target_os = "macos", windows))]
fn acquire(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> Option<wgpu::SurfaceTexture> {
    use wgpu::CurrentSurfaceTexture as C;
    for _ in 0..3 {
        match surface.get_current_texture() {
            C::Success(t) => return Some(t),
            C::Suboptimal(t) => return Some(t),
            C::Timeout => continue,
            C::Outdated => {
                if !configure(surface, device, config) {
                    return None;
                }
            }
            other => {
                tracing::warn!(?other, "no texture to present the frame into");
                return None;
            }
        }
    }
    tracing::warn!("no texture to present the frame into after three tries");
    None
}

/// Rows in a buffer copy are padded to 256 bytes.
fn padded_row(width: u32) -> u32 {
    (width * 4).div_ceil(256) * 256
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    fn zoomed_frame() -> Frame<'static> {
        Frame {
            screen: &[],
            screen_size: (160, 100),
            view: View::WHOLE,
            pointer: None,
            pointer_px: 24.0,
            pointer_style: 0,
            click_style: 0,
            clicks: Vec::new(),
            masks: Vec::new(),
            sharpen: 0.0,
        }
    }

    #[test]
    fn frames_are_composited_the_same_every_time() {
        let Ok(mut c) = Compositor::new((640, 360), Look::default()) else {
            eprintln!("no GPU here; skipping");
            return;
        };
        // a mid-grey screen
        let screen = vec![128u8; 160 * 100 * 4];
        let frame = Frame {
            screen: &screen,
            screen_size: (160, 100),
            view: View::WHOLE,
            pointer: Some(PointerDraw { x: 0.5, y: 0.5, shape: 0, opacity: 1.0, tilt: 0.0 }),
            pointer_px: 24.0,
            pointer_style: 0,
            click_style: 2,
            clicks: vec![(0.25, 0.25, 0.3)],
            masks: Vec::new(),
            sharpen: 0.0,
        };
        let a = c.render(&frame);
        let b = c.render(&frame);
        assert_eq!(a, b, "deterministic");
        // the corner is the backdrop's first colour, the screen is grey, the pointer is drawn
        let corner = px(&a, 640, 1, 1);
        assert!((corner[0] as i32 - 0x1E).abs() < 12 && (corner[2] as i32 - 0x4B).abs() < 12, "{corner:?}");
        let bx = c.screen_box(1.6);
        let grey = px(&a, 640, (bx.0 + bx.2 * 0.8) as u32, (bx.1 + bx.3 * 0.8) as u32);
        assert!(grey[..3].iter().all(|&v| (v as i32 - 128).abs() < 3), "{grey:?}");
        let tip = (bx.0 + bx.2 * 0.5, bx.1 + bx.3 * 0.5);
        let body = px(&a, 640, (tip.0 + 4.0) as u32, (tip.1 + 9.0) as u32);
        assert!(body[0] > 230 && body[1] > 230, "a white pointer body: {body:?}");
        // zoomed twice, the grey fills the box and the pointer grows
        let zoomed =
            c.render(&Frame { view: View { scale: 2.0, x: 0.5, y: 0.5 }, sharpen: 0.6, clicks: Vec::new(), ..frame });
        assert_ne!(zoomed, a);
        // a hidden area: a gradient screen becomes flat blocks there, and stays itself elsewhere
        let ramp: Vec<u8> = (0..160 * 100)
            .flat_map(|i| {
                let x = (i % 160) as u8;
                [x, x, x, 255]
            })
            .collect();
        let plain = Frame { screen: &ramp, pointer: None, clicks: Vec::new(), masks: Vec::new(), ..zoomed_frame() };
        let hidden = Frame {
            screen: &ramp,
            pointer: None,
            clicks: Vec::new(),
            masks: vec![(0.0, 0.0, 0.5, 1.0)],
            ..zoomed_frame()
        };
        let (p, h) = (c.render(&plain), c.render(&hidden));
        let at = |img: &[u8], fx: f32| px(img, 640, (bx.0 + bx.2 * fx) as u32, (bx.1 + bx.3 * 0.5) as u32)[0];
        assert_eq!(at(&p, 0.8), at(&h, 0.8), "outside the area: unchanged");
        // texels 16.4 and 17.6 of 160: one block of four
        assert_ne!(at(&p, 0.1025), at(&p, 0.11), "a ramp moves pixel to pixel");
        assert_eq!(at(&h, 0.1025), at(&h, 0.11), "inside: one block");
    }
}
