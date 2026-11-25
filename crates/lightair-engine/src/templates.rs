//! New projects, scenes and layers as Studio makes them: the desktop's own source kinds, so a
//! phone's project opens on the desktop.

use std::sync::Arc;

use lightair_model::{Canvas, Fit, Id, Layout, Object, Scene, Source, Track, Workspace};
use serde_json::{Value, json};

pub const CAMERA: &str = lightair_model::kinds::CAMERA;
/// The phone's screen: the desktop's screen kind, so projects move both ways.
pub const SCREEN: &str = lightair_model::kinds::SCREEN;
pub const MIC: &str = lightair_model::kinds::MIC;
pub const IMAGE: &str = "image_source";
pub const MEDIA: &str = "ffmpeg_source";
pub const TEXT: &str = "text_ft2_source_v2";
pub const COLOUR: &str = "color_source_v3";
pub const TIMER: &str = "lightair_timer";

/// A format: Vertical (phones' default) or Landscape, at the device's default size and rate.
pub fn canvas(vertical: bool, short: u32, fps: u32) -> Canvas {
    let long = short * 16 / 9;
    let (w, h) = if vertical { (short, long) } else { (long, short) };
    Canvas {
        id: Id::new(),
        name: if vertical { "Vertical".into() } else { "Landscape".into() },
        width: w,
        height: h,
        fps_num: fps,
        fps_den: 1,
    }
}

/// A new project: Vertical and Landscape formats, one scene with the back camera filling it,
/// and the microphone.
pub fn project(name: &str, short: u32, fps: u32) -> Workspace {
    let mut ws = Workspace::new(name);
    ws.canvases = vec![canvas(true, short, fps), canvas(false, short, fps)];
    let camera = source("Camera", CAMERA, json!({ "facing": "back" }));
    let mic = source("Microphone", MIC, json!({}));
    let mut track = Track::new("Microphone", mic.id);
    track.named = true;
    ws.tracks = vec![Arc::new(track)];
    let mut scene = Scene::new("Scene 1");
    scene.objects = vec![object_filling(&ws, &camera, Fit::Fill)];
    ws.program = Some(scene.id);
    ws.scenes = vec![Arc::new(scene)];
    ws.sources.insert(camera.id, Arc::new(camera));
    ws.sources.insert(mic.id, Arc::new(mic));
    ws
}

pub fn source(name: &str, kind: &str, settings: Value) -> Source {
    Source { id: Id::new(), name: name.into(), kind: kind.into(), settings, effects: vec![] }
}

/// An object showing `source` over each whole format.
pub fn object_filling(ws: &Workspace, source: &Source, fit: Fit) -> Object {
    let layouts = ws.canvases.iter().map(|c| {
        let mut l = Layout::boxed(0.0, 0.0, c.width as f32, c.height as f32);
        l.fit = fit;
        (c.id, l)
    });
    Object::new(&source.name, source.id, layouts)
}

/// An object showing `source` centred in each format, `frac` of its short side wide, of aspect
/// `aspect` (w/h).
pub fn object_centred(ws: &Workspace, source: &Source, frac: f32, aspect: f32) -> Object {
    let layouts = ws.canvases.iter().map(|c| {
        let short = c.width.min(c.height) as f32;
        let w = short * frac;
        let h = w / aspect.max(0.01);
        let mut l = Layout::boxed((c.width as f32 - w) / 2.0, (c.height as f32 - h) / 2.0, w, h);
        l.fit = Fit::Stretch;
        (c.id, l)
    });
    Object::new(&source.name, source.id, layouts)
}

/// An object showing `source` at `w`×`h` canvas pixels (smaller if that's more than 90% of a
/// canvas), centred.
pub fn object_sized(ws: &Workspace, source: &Source, w: f32, h: f32) -> Object {
    let layouts = ws.canvases.iter().map(|c| {
        let (cw, ch) = (c.width as f32, c.height as f32);
        let k = (0.9 * cw / w.max(1.0)).min(0.9 * ch / h.max(1.0)).min(1.0);
        let (w, h) = (w * k, h * k);
        let mut l = Layout::boxed((cw - w) / 2.0, (ch - h) / 2.0, w, h);
        l.fit = Fit::Stretch;
        (c.id, l)
    });
    Object::new(&source.name, source.id, layouts)
}

/// A text source's settings, as the desktop's text object stores them (libobs `text_ft2`:
/// colours are 0xAABBGGRR).
#[allow(clippy::too_many_arguments)]
pub fn text_settings(
    text: &str,
    family: Option<&str>,
    size: f32,
    rgb: u32,
    bold: bool,
    align: u8,
    outline: Option<u32>,
    background: Option<u32>,
) -> Value {
    let abgr = |rgb: u32| 0xFF00_0000 | ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF);
    let align = ["left", "center", "right"][align.min(2) as usize];
    let mut s = json!({
        "text": text,
        "font": { "face": family.unwrap_or("Inter"), "size": size.round() as i64, "flags": if bold { 1 } else { 0 } },
        "color1": abgr(rgb),
        "color2": abgr(rgb),
        "align": align,
    });
    if let Some(o) = outline {
        s["outline"] = json!(true);
        s["outline_color"] = json!(abgr(o));
    }
    if let Some(b) = background {
        s["background"] = json!(abgr(b));
    }
    s
}

/// A colour source's settings (0xAARRGGBB in, libobs's 0xAABBGGRR stored).
pub fn colour_settings(argb: u32, width: u32, height: u32) -> Value {
    let a = argb >> 24;
    let rgb = argb & 0xFF_FFFF;
    let abgr = (a << 24) | ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF);
    json!({ "color": abgr, "width": width, "height": height })
}
