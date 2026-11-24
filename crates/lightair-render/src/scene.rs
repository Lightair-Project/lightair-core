//! A whole `lightair-model` scene drawn per frame: Lightair Live's Studio draws its scenes with
//! it, and the desktop checks it against libobs (golden frames). The objects as the desktop
//! draws them (lightair-live's docs/03 §1):
//!
//! - the hierarchy flattened ([`Scene::flatten`]): each picture at its world layout, a parent's
//!   visibility and opacity passed down;
//! - the box, fitted (fit, fill, stretch, original), cropped, turned, flipped, tilted in 3D with
//!   perspective, and cut out (rounded box, circle, pill, soft edge, border), as Lightair's
//!   libobs filters do;
//! - the source's effects: the catalog's shader effects (`fx.*`), and the colour filter
//!   (`color_filter_v2`: brightness, contrast, saturation, opacity). The others (keys, LUTs,
//!   sharpening, the Studio effects that need a model) aren't drawn: [`unsupported`] lists them
//!   so an app can mark those objects;
//! - transitions between two scenes: cut, fade, move (matching objects glide), slide, wipe, dip
//!   through black, and a stinger (its video from the caller).
//!
//! The pictures come from the caller ([`Pictures`]): a phone's cameras and media, a test's
//! images. A source it gives none for isn't drawn.

use lightair_model::{
    Direction, Fit, Id, Layout, Object, ShapeKind, Source, TransitionKind, TransitionSettings, Workspace,
};
use serde_json::{Map, Value};

use crate::compose::{Compositor, Cut, Layer, Pixels};
use crate::media::Picture;
use crate::sequence::{ClipEffect, Filter};

/// A source's picture now.
#[derive(Clone)]
pub struct Shot {
    pub picture: Picture,
    /// Its size in pixels.
    pub size: (u32, u32),
    /// Its colours already multiplied by its alpha (made pictures: text, shapes).
    pub premultiplied: bool,
}

impl Shot {
    pub fn rgba(rgba: Vec<u8>, size: (u32, u32)) -> Shot {
        Shot { picture: Picture::Rgba(rgba), size, premultiplied: false }
    }
}

/// Where the pictures come from.
pub trait Pictures {
    /// `source`'s picture now; `None`: it isn't drawn (nothing yet, or a kind this app can't
    /// show).
    fn picture(&mut self, source: &Source) -> Option<Shot>;

    /// The workspace's stinger (`path`) at `at_ms` into it, for a stinger transition; `None`:
    /// the switch is a cut at its cut point.
    fn stinger(&mut self, path: &str, at_ms: u32) -> Option<Shot> {
        let _ = (path, at_ms);
        None
    }
}

/// Something in a scene the renderer doesn't draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub object: Id,
    pub source: Id,
    /// The effect's kind (`chroma_key_filter_v2`, `studio.bg_remove`, …).
    pub effect: String,
}

/// The effects of the objects in `scene` that [`SceneRenderer`] leaves out (audio effects aren't
/// counted: they aren't pictures).
pub fn unsupported(workspace: &Workspace, scene: Id) -> Vec<Unsupported> {
    let Some(scene) = workspace.scene(scene) else { return vec![] };
    let mut out = vec![];
    for o in &scene.objects {
        let Some(source) = o.source.and_then(|s| workspace.sources.get(&s)) else { continue };
        for e in source.effects.iter().filter(|e| e.enabled) {
            if !drawn(e) && !e.kind.starts_with("audio.") {
                out.push(Unsupported { object: o.id, source: source.id, effect: e.kind.clone() });
            }
        }
    }
    out
}

/// Whether the renderer draws an effect: the shader effects, the colour filter, and a LUT
/// from a `.cube` file (libobs's `clut_filter`; its `.png` strips aren't read).
fn drawn(e: &lightair_model::Effect) -> bool {
    e.kind.starts_with("fx.") || e.kind == "color_filter_v2" || cube_of(e).is_some()
}

/// A `clut_filter`'s `.cube` file.
fn cube_of(e: &lightair_model::Effect) -> Option<&str> {
    let path = e.settings.get("image_path")?.as_str()?;
    (e.kind == "clut_filter" && path.to_ascii_lowercase().ends_with(".cube")).then_some(path)
}

/// One picture to draw, owned (the pictures live until the frame is drawn).
struct Draw {
    shot: Shot,
    center: (f32, f32),
    draw: (f32, f32),
    rotation: f32,
    opacity: f32,
    uv: (f32, f32, f32, f32),
    filter: Filter,
    effects: Vec<ClipEffect>,
    cut: Option<Cut>,
    tilt: (f32, f32),
    keep: Option<(f32, f32, f32)>,
    /// Half the box a covering picture is cut to (output pixels).
    bounds: Option<(f32, f32)>,
    /// What a Move transition matches it by.
    object: Object,
    layout: Layout,
}

pub struct SceneRenderer {
    compositor: Compositor,
    size: (u32, u32),
    /// Where relative file paths in effects are (the workspace's folder).
    folder: Option<std::path::PathBuf>,
}

impl SceneRenderer {
    /// Frames `size` pixels (a canvas's size, or smaller for a preview: the canvas is scaled to
    /// the width).
    pub fn new(size: (u32, u32)) -> Result<SceneRenderer, String> {
        Ok(SceneRenderer { compositor: Compositor::new_linear(size, false)?, size, folder: None })
    }

    /// The workspace's folder: where relative file paths in effects (a LUT's) are found, as a
    /// portable copy (`project.export`) names its `assets/…`.
    pub fn set_folder(&mut self, folder: Option<std::path::PathBuf>) {
        self.folder = folder;
    }

    /// The compositor, to present into a layer or a surface instead of reading frames back.
    pub fn compositor(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Frames stay on the GPU ([`SceneRenderer::target`]); [`SceneRenderer::frame`] and
    /// [`SceneRenderer::transition`] then return nothing (see [`Compositor::keep_on_gpu`]).
    pub fn keep_on_gpu(&mut self, keep: bool) {
        self.compositor.keep_on_gpu(keep);
    }

    /// The texture the frames are drawn into ([`Compositor::target`]).
    pub fn target(&self) -> &wgpu::Texture {
        self.compositor.target()
    }

    /// `scene` in `canvas`, now (`time_s`: the clock moving effects run on). RGBA, top row
    /// first; nothing when presented into a layer.
    pub fn frame(
        &mut self,
        workspace: &Workspace,
        scene: Id,
        canvas: Id,
        pictures: &mut dyn Pictures,
        time_s: f32,
    ) -> Vec<u8> {
        let draws = self.draws(workspace, scene, canvas, pictures);
        self.draw(&draws, time_s)
    }

    /// The switch from `from` to `to` in `canvas`, `progress` (0..1) through `transition`.
    #[allow(clippy::too_many_arguments)]
    pub fn transition(
        &mut self,
        workspace: &Workspace,
        from: Id,
        to: Id,
        canvas: Id,
        transition: &TransitionSettings,
        progress: f32,
        pictures: &mut dyn Pictures,
        time_s: f32,
    ) -> Vec<u8> {
        let p = progress.clamp(0.0, 1.0);
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let mut a = self.draws(workspace, from, canvas, pictures);
        let mut b = self.draws(workspace, to, canvas, pictures);
        let draws = match transition.kind {
            TransitionKind::Cut => b,
            TransitionKind::Fade => {
                for d in &mut b {
                    d.opacity *= p;
                }
                a.extend(b);
                a
            }
            TransitionKind::Dip => {
                // through black, the scene switching halfway
                let (mut out, dark) = if p < 0.5 { (a, p * 2.0) } else { (b, 2.0 - p * 2.0) };
                out.push(self.colour(0x000000, dark));
                out
            }
            TransitionKind::Slide => {
                let e = ease_in_out(p);
                let (dx, dy) = match transition.direction {
                    Direction::Left => (-w, 0.0),
                    Direction::Right => (w, 0.0),
                    Direction::Up => (0.0, -h),
                    Direction::Down => (0.0, h),
                };
                for d in &mut a {
                    d.center = (d.center.0 + dx * e, d.center.1 + dy * e);
                }
                for d in &mut b {
                    d.center = (d.center.0 - dx * (1.0 - e), d.center.1 - dy * (1.0 - e));
                }
                a.extend(b);
                a
            }
            TransitionKind::Wipe => {
                // the new scene across the old one, coming in from the side `direction` points
                // away from (Left: from the right, its edge moving left)
                let keep = match transition.direction {
                    Direction::Left => (1.0, 0.0, w * (1.0 - p)),
                    Direction::Right => (-1.0, 0.0, -w * p),
                    Direction::Up => (0.0, 1.0, h * (1.0 - p)),
                    Direction::Down => (0.0, -1.0, -h * p),
                };
                for d in &mut b {
                    d.keep = Some(keep);
                }
                a.extend(b);
                a
            }
            TransitionKind::Move => moved(a, b, ease_in_out(p)),
            TransitionKind::Stinger => {
                let Some(stinger) = workspace.stinger.clone() else {
                    return self.draw(&if p < 0.5 { a } else { b }, time_s);
                };
                let at = (p * transition.duration_ms as f32) as u32;
                let mut out = if at < stinger.cut_ms { a } else { b };
                if let Some(shot) = pictures.stinger(&stinger.path, at) {
                    out.push(self.cover(shot));
                }
                out
            }
            TransitionKind::Scene => {
                // the transition scene over the switch, the scenes swapping halfway
                let over = workspace.scene(to).and_then(|s| s.transition_scene).or(workspace.show.transition_scene);
                let mut out = if p < 0.5 { a } else { b };
                if let Some(over) = over.filter(|o| *o != from && *o != to) {
                    out.extend(self.draws(workspace, over, canvas, pictures));
                }
                out
            }
        };
        self.draw(&draws, time_s)
    }

    fn draws(&self, workspace: &Workspace, scene: Id, canvas: Id, pictures: &mut dyn Pictures) -> Vec<Draw> {
        let Some(scene) = workspace.scene(scene) else { return vec![] };
        let Some(c) = workspace.canvases.iter().find(|c| c.id == canvas) else { return vec![] };
        let k = self.size.0 as f32 / c.width.max(1) as f32;
        scene
            .flatten()
            .objects
            .into_iter()
            .filter_map(|o| {
                let layout = *o.layouts.get(&canvas)?;
                if !layout.visible || layout.opacity <= 0.0 {
                    return None;
                }
                let source = workspace.sources.get(&o.source?)?;
                let shot = pictures.picture(source)?;
                Some(placed(o.clone(), layout, source, shot, k, self.folder.as_deref()))
            })
            .collect()
    }

    /// A colour over the whole frame.
    fn colour(&self, rgb: u32, opacity: f32) -> Draw {
        let px = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255];
        self.cover(Shot::rgba(px.repeat(4), (2, 2))).with_opacity(opacity)
    }

    /// `shot` stretched over the whole frame.
    fn cover(&self, shot: Shot) -> Draw {
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        Draw {
            shot,
            center: (w / 2.0, h / 2.0),
            draw: (w, h),
            rotation: 0.0,
            opacity: 1.0,
            uv: (0.0, 0.0, 1.0, 1.0),
            filter: Filter::default(),
            effects: vec![],
            cut: None,
            tilt: (0.0, 0.0),
            keep: None,
            bounds: None,
            object: Object::new("", Id::new(), []),
            layout: Layout::boxed(0.0, 0.0, w, h),
        }
    }

    fn draw(&mut self, draws: &[Draw], time_s: f32) -> Vec<u8> {
        let layers: Vec<Layer> = draws
            .iter()
            .map(|d| Layer {
                pixels: match &d.shot.picture {
                    Picture::Rgba(b) => Pixels::Rgba(b),
                    #[cfg(any(target_os = "macos", target_os = "ios"))]
                    Picture::Buffer(b) => Pixels::Buffer(b),
                    Picture::Texture(t) => Pixels::Texture(t),
                },
                size: d.shot.size,
                center: d.center,
                draw: d.draw,
                rotation: d.rotation,
                opacity: d.opacity,
                premultiplied: d.shot.premultiplied,
                filter: d.filter,
                masks: &[],
                effects: &d.effects,
                time: time_s,
                uv: d.uv,
                cut: d.cut,
                tilt: d.tilt,
                keep: d.keep,
                bounds: d.bounds,
            })
            .collect();
        self.compositor.layers(&layers)
    }
}

impl Draw {
    fn with_opacity(mut self, opacity: f32) -> Draw {
        self.opacity = opacity;
        self
    }
}

/// One object's picture where its layout puts it, `k` output pixels per canvas pixel.
fn placed(object: Object, l: Layout, source: &Source, shot: Shot, k: f32, folder: Option<&std::path::Path>) -> Draw {
    let (w, h) = (shot.size.0.max(1) as f32, shot.size.1.max(1) as f32);
    let c = l.crop;
    let (cl, ct) = ((c.left as f32).min(w - 1.0), (c.top as f32).min(h - 1.0));
    let vw = (w - c.left as f32 - c.right as f32).max(1.0);
    let vh = (h - c.top as f32 - c.bottom as f32).max(1.0);
    // the crop window (fractions)
    let mut uv = (cl / w, ct / h, vw / w, vh / h);
    let (bw, bh) = (l.width.abs(), l.height.abs());
    let draw = match l.fit {
        Fit::Fit => {
            let s = (bw / vw).min(bh / vh);
            (vw * s, vh * s)
        }
        Fit::Fill => {
            // covers the box, the overflow cut off (after a tilt: as libobs crops to the bounds)
            let s = (bw / vw).max(bh / vh);
            (vw * s, vh * s)
        }
        Fit::Stretch => (bw, bh),
        Fit::Original => (vw, vh),
    };
    if l.flip_h {
        uv = (uv.0 + uv.2, uv.1, -uv.2, uv.3);
    }
    if l.flip_v {
        uv = (uv.0, uv.1 + uv.3, uv.2, -uv.3);
    }
    let cut = l.shape.map(|s| {
        let short = vw.min(vh) / 2.0;
        let (half, radius) = match s.kind {
            ShapeKind::Circle => ((short, short), short),
            ShapeKind::Pill => ((vw / 2.0, vh / 2.0), short),
            ShapeKind::Rounded => ((vw / 2.0, vh / 2.0), s.radius.clamp(0.0, 1.0) * short),
        };
        // 0xAABBGGRR
        let b = s.border_color;
        let ch = |shift: u32| ((b >> shift) & 255) as f32 / 255.0;
        Cut {
            centre: (cl + vw / 2.0, ct + vh / 2.0),
            half,
            radius,
            feather: s.feather,
            border: s.border,
            border_color: [ch(0), ch(8), ch(16), ch(24)],
        }
    });
    let (filter, opacity, effects) = effects_of(source, folder);
    Draw {
        shot,
        center: ((l.x + l.width / 2.0) * k, (l.y + l.height / 2.0) * k),
        draw: (draw.0 * k, draw.1 * k),
        rotation: l.rotation,
        opacity: l.opacity.clamp(0.0, 1.0) * opacity,
        uv,
        filter,
        effects,
        cut,
        tilt: (l.tilt_x, l.tilt_y),
        keep: None,
        bounds: (l.fit == Fit::Fill).then_some((bw * k / 2.0, bh * k / 2.0)),
        object,
        layout: l,
    }
}

/// A source's effects as the compositor draws them: the colour filter, its opacity, and the
/// shader effects (each with the catalog's defaults under its own settings).
fn effects_of(source: &Source, folder: Option<&std::path::Path>) -> (Filter, f32, Vec<ClipEffect>) {
    let mut filter = Filter::default();
    let mut opacity = 1.0;
    let mut effects = vec![];
    for e in source.effects.iter().filter(|e| e.enabled) {
        let num = |k: &str, d: f64| e.settings.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
        if e.kind == "color_filter_v2" {
            // libobs's colour correction: brightness added, contrast and saturation around 1
            filter.brightness = (filter.brightness + num("brightness", 0.0)).clamp(-1.0, 1.0);
            filter.contrast *= (num("contrast", 0.0) + 1.0).max(0.0);
            filter.saturation *= (num("saturation", 0.0) + 1.0).max(0.0);
            opacity *= num("opacity", 1.0).clamp(0.0, 1.0);
        } else if e.kind.starts_with("fx.") {
            let mut settings: Map<String, Value> =
                lightair_model::catalog::def(&e.kind).map(|d| d.defaults()).unwrap_or_default();
            if let Value::Object(own) = &e.settings {
                settings.extend(own.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            effects.push(ClipEffect::new(e.kind.clone(), settings));
        } else if let Some(cube) = cube_of(e) {
            // libobs's LUT filter: lightair-render's own (a relative path: in the workspace's
            // folder, as a portable copy names its assets)
            let path = std::path::Path::new(cube);
            let file = match folder {
                Some(f) if path.is_relative() => f.join(path),
                _ => path.to_path_buf(),
            };
            let mut settings = Map::new();
            settings.insert("file".into(), Value::from(file.to_string_lossy().into_owned()));
            settings.insert("amount".into(), Value::from(num("clut_amount", 1.0).clamp(0.0, 1.0) as f64));
            effects.push(ClipEffect::new("fx.lut", settings));
        }
    }
    (filter, opacity, effects)
}

/// Whether two pictures are the same thing in two scenes (one of Move's rules).
type Match = dyn Fn(&Draw, &Draw) -> bool;

/// The Move transition at `e` (eased): objects in both scenes (by link, then source, then
/// name) glide from A's layout to B's; the rest of A fades out, the rest of B fades in.
fn moved(a: Vec<Draw>, b: Vec<Draw>, e: f32) -> Vec<Draw> {
    let mut taken = vec![false; a.len()];
    let mut partner = vec![None; b.len()];
    let rules: [&Match; 3] = [
        &|x, y| x.object.link.is_some() && x.object.link == y.object.link,
        &|x, y| x.object.source.is_some() && x.object.source == y.object.source,
        &|x, y| !x.object.name.is_empty() && x.object.name == y.object.name,
    ];
    for rule in rules {
        for (j, y) in b.iter().enumerate() {
            if partner[j].is_some() {
                continue;
            }
            if let Some(i) = (0..a.len()).find(|&i| !taken[i] && rule(&a[i], y)) {
                taken[i] = true;
                partner[j] = Some(i);
            }
        }
    }
    let lerp = |x: f32, y: f32| x + (y - x) * e;
    let mut out: Vec<Draw> = vec![];
    let mut a: Vec<Option<Draw>> = a.into_iter().map(Some).collect();
    // A's own, fading out, behind
    for (i, d) in a.iter_mut().enumerate() {
        if !taken[i]
            && let Some(mut d) = d.take()
        {
            d.opacity *= 1.0 - e;
            out.push(d);
        }
    }
    for (j, mut d) in b.into_iter().enumerate() {
        match partner[j].and_then(|i| a[i].take()) {
            Some(from) => {
                // B's picture, gliding from where A had it (its size by A's box to B's)
                let (fl, tl) = (from.layout, d.layout);
                let scale =
                    (lerp(fl.width, tl.width) / tl.width.max(1e-3), lerp(fl.height, tl.height) / tl.height.max(1e-3));
                d.center = (lerp(from.center.0, d.center.0), lerp(from.center.1, d.center.1));
                d.draw = (d.draw.0 * scale.0, d.draw.1 * scale.1);
                d.rotation = lerp(fl.rotation, tl.rotation);
                d.tilt = (lerp(fl.tilt_x, tl.tilt_x), lerp(fl.tilt_y, tl.tilt_y));
                d.opacity = lerp(from.opacity, d.opacity);
            }
            None => d.opacity *= e,
        }
        out.push(d);
    }
    out
}

/// The desktop's Move easing (cubic in and out).
fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}
