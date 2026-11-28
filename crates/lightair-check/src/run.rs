//! The headless run of a package's content (design §4.4): what can be drawn is drawn with
//! `lightair-render` on whatever GPU adapter the machine has (Metal on a Mac, lavapipe on the Linux
//! runner), what can be played is probed, and recipes are walked and dry-run against a fake
//! studio that records the methods they call. Each kind says plainly when it can't be run here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lightair_model::{Command, Id, Layout, Object, Scene, SceneAsset, Source, Stinger, TransitionSettings, Workspace};
use lightair_packages::{Kind, Limits, Package};
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, SceneRenderer, Shot};
use serde_json::{Value, json};

use crate::report::{Check, Output, Preview};

/// Frames drawn for checking and previews.
const SIZE: (u32, u32) = (640, 360);
/// A preview's size limit.
const PREVIEW_MAX: usize = 100 * 1024;
/// At most this many previews.
const PREVIEWS: usize = 4;
/// A frame may take this many times a reference frame (a plain scene on the same adapter), and
/// never more than [`FRAME_MAX`].
const FRAME_FACTOR: f64 = 40.0;
const FRAME_MAX: Duration = Duration::from_secs(2);

/// The run. `previews` also gets the previews as files.
pub fn run(path: &Path, deep: bool, previews: Option<&Path>) -> Output {
    let mut out = Output { observed: Some(Default::default()), ..Output::default() };
    let mut c = Check::new(if deep { "run-deep" } else { "run" });
    let package = match Package::open_with(path, Limits::every_tier()) {
        Ok(p) => p,
        Err(e) => {
            c.fail(e.to_string(), None);
            out.checks.push(c);
            return out;
        }
    };
    let dir = match extracted(path, &package) {
        Ok(d) => d,
        Err(e) => {
            c.fail(format!("couldn't unpack the package to run it: {e}"), None);
            out.checks.push(c);
            return out;
        }
    };

    let mut gpu = Gpu::default();
    for content in &package.manifest.contents {
        let folder = dir.path.join(&content.path);
        let rel = content.path.as_str();
        match content.kind {
            Kind::Template => template(&mut c, &mut gpu, &folder, rel, &mut out.previews),
            Kind::Transition => transition(&mut c, &mut gpu, &folder, rel, &mut out.previews),
            Kind::ShaderEffect | Kind::ShaderTransition | Kind::ShaderSource => shaders(&mut c, &mut gpu, &folder, rel),
            Kind::Media | Kind::Sound => media(&mut c, &folder, rel),
            Kind::Lut => luts(&mut c, &folder, rel),
            Kind::Recipe => recipe(&mut c, &folder, rel, deep),
            Kind::Widget => c.note("widgets are run in headless Chromium by the widget run (run-linux)", Some(rel)),
            Kind::Look => c.note("looks need a face and the Effect Stack's models: not drawn headless yet", Some(rel)),
            Kind::Avatar => c.note(
                "avatars aren't drawn headless yet (their runtimes are the desktop's): only read by the static checks",
                Some(rel),
            ),
            Kind::Helper => c.note("helpers are started by the helper run on macOS and Windows", Some(rel)),
            Kind::Extension => crate::script_run::script(&mut c, &folder, rel, &package.manifest),
            Kind::WasmEffect | Kind::WasmSource => c.note("WebAssembly has no runtime to run in yet", Some(rel)),
            Kind::EffectPreset | Kind::Theme | Kind::Font | Kind::Other(_) => {}
        }
    }
    if let Some(adapter) = &gpu.adapter {
        c.note(format!("drawn on {adapter}"), None);
    }
    out.previews.truncate(PREVIEWS);
    if let Some(dir) = previews {
        let _ = std::fs::create_dir_all(dir);
        for p in &out.previews {
            use base64::Engine as _;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&p.webp_base64) {
                let _ = std::fs::write(dir.join(format!("{}.webp", slug(&p.name))), bytes);
            }
        }
    }
    out.checks.push(c.finish("everything that can run here ran without a problem"));
    out
}

/// The package unpacked into a temporary folder (removed when dropped), or the folder itself.
struct Extracted {
    path: PathBuf,
    temp: bool,
}

impl Drop for Extracted {
    fn drop(&mut self) {
        if self.temp {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn extracted(path: &Path, package: &Package) -> Result<Extracted, String> {
    if path.is_dir() {
        return Ok(Extracted { path: path.to_path_buf(), temp: false });
    }
    let dir = std::env::temp_dir().join(format!("lightair-check-run-{}-{}", std::process::id(), Id::new()));
    package.extract_to(&dir).map_err(|e| e.to_string())?;
    Ok(Extracted { path: dir, temp: true })
}

/// The GPU, made once: a renderer for frames and the time a plain frame takes.
#[derive(Default)]
struct Gpu {
    renderer: Option<Result<SceneRenderer, String>>,
    reference: Option<Duration>,
    adapter: Option<String>,
}

impl Gpu {
    fn renderer(&mut self) -> Result<&mut SceneRenderer, String> {
        if self.renderer.is_none() {
            let made = SceneRenderer::new(SIZE);
            if made.is_ok() {
                self.adapter = lightair_render::shared_gpu().ok().map(|g| {
                    let i = g.adapter.get_info();
                    format!("{} ({:?})", i.name, i.backend)
                });
            }
            self.renderer = Some(made);
        }
        match self.renderer.as_mut() {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(e.clone()),
            None => Err("no renderer".into()),
        }
    }

    /// How long a plain scene (one colour) takes here, measured once.
    fn reference(&mut self) -> Option<Duration> {
        if self.reference.is_none() {
            let (ws, scene, canvas) = sample_workspace(0x3A4150);
            let r = self.renderer().ok()?;
            let mut pics = CheckPictures::new(PathBuf::new());
            r.frame(&ws, scene, canvas, &mut pics, 0.0);
            let t = Instant::now();
            for i in 0..3 {
                r.frame(&ws, scene, canvas, &mut pics, i as f32 * 0.1);
            }
            self.reference = Some(t.elapsed() / 3);
        }
        self.reference
    }

    fn budget(&mut self) -> Duration {
        self.reference().map_or(FRAME_MAX, |r| r.mul_f64(FRAME_FACTOR).clamp(Duration::from_millis(50), FRAME_MAX))
    }
}

/// A workspace with one scene holding a full-canvas colour.
fn sample_workspace(rgb: u32) -> (Workspace, Id, Id) {
    let mut ws = Workspace::new("check");
    let scene = sample_scene(&mut ws, "Sample", rgb);
    let canvas = ws.canvases[0].id;
    (ws, scene, canvas)
}

fn sample_scene(ws: &mut Workspace, name: &str, rgb: u32) -> Id {
    let abgr = 0xFF00_0000 | ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF);
    let (w, h) = (ws.canvases[0].width, ws.canvases[0].height);
    // a plain colour, or (rgb 0) the test pattern a camera shows
    let source = if rgb == 0 {
        Source::new(name, lightair_model::kinds::CAMERA, json!({}))
    } else {
        Source::new(name, COLOUR, json!({"color": abgr, "width": w, "height": h}))
    };
    let sid = source.id;
    let scene = Scene::new(name.to_string());
    let id = scene.id;
    let canvas = ws.canvases[0].id;
    let _ = Command::AddSource { source }.apply(ws);
    let _ = Command::AddScene { scene, index: None }.apply(ws);
    let object = Object::new(name, sid, [(canvas, Layout::boxed(0.0, 0.0, w as f32, h as f32))]);
    let _ = Command::AddObject { scene: id, object, index: None }.apply(ws);
    id
}

const IMAGE: &str = "image_source";
const MEDIA: &str = "ffmpeg_source";
const TEXT: &str = "text_ft2_source_v2";
const COLOUR: &str = "color_source_v3";
const TIMER: &str = "lightair_timer";

/// Pictures for a check: images decoded, text and colours made, cameras and screens as a test
/// pattern (a template's device roles), a video's first frame through FFmpeg. Web pages and the
/// rest aren't drawn; what wasn't is kept to say so.
struct CheckPictures {
    base: PathBuf,
    skipped: BTreeSet<String>,
    broken: BTreeSet<String>,
}

impl CheckPictures {
    fn new(base: PathBuf) -> CheckPictures {
        CheckPictures { base, skipped: BTreeSet::new(), broken: BTreeSet::new() }
    }

    fn file(&self, s: &Value, key: &str) -> Option<PathBuf> {
        let f = s[key].as_str().filter(|f| !f.is_empty())?;
        let p = PathBuf::from(f);
        Some(if p.is_absolute() { p } else { self.base.join(p) })
    }
}

impl Pictures for CheckPictures {
    fn picture(&mut self, source: &Source) -> Option<Shot> {
        let s = &source.settings;
        match source.kind.as_str() {
            IMAGE => {
                let path = self.file(s, "file")?;
                match image::open(&path) {
                    Ok(img) => {
                        let img =
                            if img.width() > 4096 || img.height() > 4096 { img.thumbnail(4096, 4096) } else { img };
                        let rgba = img.to_rgba8();
                        let size = rgba.dimensions();
                        Some(Shot::rgba(rgba.into_raw(), size))
                    }
                    Err(e) => {
                        self.broken.insert(format!(
                            "{}: {e}",
                            path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
                        ));
                        None
                    }
                }
            }
            TEXT => {
                let style = text_style(s, s["text"].as_str().unwrap_or("").to_string());
                let r = lightair_render::generate::text(&style);
                Some(Shot { picture: Picture::Rgba(r.rgba), size: r.size, premultiplied: true })
            }
            TIMER => {
                let style = text_style(s, "12:34".into());
                let r = lightair_render::generate::text(&style);
                Some(Shot { picture: Picture::Rgba(r.rgba), size: r.size, premultiplied: true })
            }
            COLOUR => {
                let abgr = s["color"].as_u64().unwrap_or(0xFFFF_FFFF) as u32;
                let w = s["width"].as_u64().unwrap_or(1920).clamp(1, 4096) as u32;
                let h = s["height"].as_u64().unwrap_or(1080).clamp(1, 4096) as u32;
                let px = [(abgr & 0xFF) as u8, (abgr >> 8 & 0xFF) as u8, (abgr >> 16 & 0xFF) as u8, (abgr >> 24) as u8];
                Some(Shot::rgba(px.repeat((w * h) as usize), (w, h)))
            }
            k if k == lightair_model::kinds::CAMERA
                || k == lightair_model::kinds::SCREEN
                || k == "lightair_camera_capture" =>
            {
                Some(test_pattern(if k == lightair_model::kinds::SCREEN { (1920, 1080) } else { (1280, 720) }))
            }
            MEDIA => {
                let path = self.file(s, "local_file")?;
                match lightair_render::media::frame_at(&path, 0, (640, 360)) {
                    Ok(rgba) => Some(Shot::rgba(rgba, (640, 360))),
                    Err(e) => {
                        self.broken.insert(format!(
                            "{}: {e}",
                            path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
                        ));
                        None
                    }
                }
            }
            other => {
                self.skipped.insert(other.to_string());
                None
            }
        }
    }

    fn stinger(&mut self, path: &str, at_ms: u32) -> Option<Shot> {
        let p = if Path::new(path).is_absolute() { PathBuf::from(path) } else { self.base.join(path) };
        lightair_render::media::frame_at(&p, at_ms as u64, (640, 360)).ok().map(|rgba| Shot::rgba(rgba, (640, 360)))
    }
}

/// Bars and a gradient: what a camera or screen role shows in a check.
fn test_pattern((w, h): (u32, u32)) -> Shot {
    let bars: [[u8; 3]; 7] =
        [[192, 192, 192], [192, 192, 0], [0, 192, 192], [0, 192, 0], [192, 0, 192], [192, 0, 0], [0, 0, 192]];
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let px = if y < h * 2 / 3 {
                bars[(x * 7 / w) as usize]
            } else {
                let v = (x * 255 / w.max(1)) as u8;
                [v, v, v]
            };
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    Shot::rgba(rgba, (w, h))
}

/// A text source's settings (libobs `text_ft2`, as the desktop writes them) as a text style.
fn text_style(s: &Value, text: String) -> lightair_render::sequence::TextStyle {
    let rgb_of = |abgr: u64| {
        let v = abgr as u32;
        ((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF)
    };
    let font = &s["font"];
    let family = font["face"].as_str().filter(|f| !f.is_empty()).map(str::to_string);
    let bold = font["flags"].as_i64().unwrap_or(0) & 1 == 1;
    lightair_render::sequence::TextStyle {
        text,
        size: font["size"].as_f64().unwrap_or(72.0) as f32,
        color: rgb_of(s["color1"].as_u64().unwrap_or(0xFFFF_FFFF)),
        bold,
        align: match s["align"].as_str() {
            Some("left") => 0,
            Some("right") => 2,
            _ => 1,
        },
        background: s["background"].as_u64().map(|c| (rgb_of(c), ((c >> 24) & 0xFF) as f32 / 255.0)),
        outline: s["outline"]
            .as_bool()
            .filter(|o| *o)
            .map(|_| rgb_of(s["outline_color"].as_u64().unwrap_or(0xFF00_0000))),
        width: s["width"].as_f64().unwrap_or(4096.0) as f32,
        font: family.map(|f| lightair_fonts::FontRef::new(f, if bold { 700 } else { 400 })),
    }
}

/// Relative file names in a template made absolute (inside its folder), as the desktop does.
fn absolutise(value: &mut Value, base: &Path) {
    match value {
        Value::String(s) => {
            let rel = Path::new(s.as_str());
            if !s.is_empty()
                && !s.contains("://")
                && rel.is_relative()
                && rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
                && base.join(rel).is_file()
            {
                *s = base.join(rel).to_string_lossy().into_owned();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| absolutise(v, base)),
        Value::Object(m) => m.values_mut().for_each(|v| absolutise(v, base)),
        _ => {}
    }
}

/// What a frame looks like: drawn at all, more than one colour.
fn judge(c: &mut Check, rgba: &[u8], what: &str, rel: &str) -> bool {
    if rgba.is_empty() {
        c.fail(format!("{what}: no frame came back from the renderer"), Some(rel));
        return false;
    }
    let (pixels, _) = rgba.as_chunks::<4>();
    let drawn = pixels.iter().any(|p| p[3] > 0);
    if !drawn {
        c.fail(format!("{what}: the frame is empty (nothing drawn)"), Some(rel));
        return false;
    }
    let first = pixels[0];
    if pixels.iter().step_by(7).all(|p| *p == first) {
        c.note(format!("{what}: the frame is a single colour"), Some(rel));
    }
    true
}

fn template(c: &mut Check, gpu: &mut Gpu, folder: &Path, rel: &str, previews: &mut Vec<Preview>) {
    let file = folder.join("scene.json");
    let Ok(bytes) = std::fs::read(&file) else {
        return c.fail("a template needs a scene.json", Some(rel));
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return c.fail("scene.json isn't valid JSON", Some(rel));
    };
    absolutise(&mut value, folder);
    let asset: SceneAsset = match serde_json::from_value(value) {
        Ok(a) => a,
        Err(e) => return c.fail(format!("scene.json isn't a scene: {e}"), Some(rel)),
    };
    let mut ws = Workspace::new("check");
    let (commands, scene) = asset.add(&ws, None);
    for cmd in &commands {
        if let Err(e) = cmd.apply(&mut ws) {
            return c.fail(format!("the scene couldn't be added: {e:?}"), Some(rel));
        }
    }
    let canvas = ws.canvases[0].id;
    let unsupported = lightair_render::scene::unsupported(&ws, scene);
    let budget = gpu.budget();
    let r = match gpu.renderer() {
        Ok(r) => r,
        Err(e) => return c.note(format!("no GPU here, so the template wasn't drawn: {e}"), Some(rel)),
    };
    let mut pics = CheckPictures::new(folder.to_path_buf());
    let mut slowest = Duration::ZERO;
    let mut frame = Vec::new();
    // the first frame uploads pictures and shapes text: timed are the ones after it
    for i in 0..5 {
        let t = Instant::now();
        frame = r.frame(&ws, scene, canvas, &mut pics, i as f32 * 0.5);
        if i > 0 {
            slowest = slowest.max(t.elapsed());
        }
    }
    let name = format!("{} ({rel})", asset.name);
    if judge(c, &frame, &format!("the template “{}”", asset.name), rel) {
        push_preview(previews, &name, &frame, SIZE);
    }
    if slowest > budget {
        c.fail(
            format!(
                "a frame of “{}” took {} ms, over the {} ms this machine allows",
                asset.name,
                slowest.as_millis(),
                budget.as_millis()
            ),
            Some(rel),
        );
    }
    for b in &pics.broken {
        c.fail(format!("a picture couldn't be read: {b}"), Some(rel));
    }
    if !pics.skipped.is_empty() {
        let kinds: Vec<&str> = pics.skipped.iter().map(String::as_str).collect();
        c.note(
            format!("not drawn here (web pages run in the widget run; the rest need the app): {}", kinds.join(", ")),
            Some(rel),
        );
    }
    if !unsupported.is_empty() {
        c.note(format!("{} effect(s) the renderer doesn't draw yet", unsupported.len()), Some(rel));
    }
}

fn transition(c: &mut Check, gpu: &mut Gpu, folder: &Path, rel: &str, previews: &mut Vec<Preview>) {
    let Some(file) = ["transition.json", "settings.json"].iter().map(|n| folder.join(n)).find(|p| p.is_file()) else {
        return c.note("no transition.json: only its media were read by the static checks", Some(rel));
    };
    let Ok(value) = std::fs::read(&file)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
    else {
        return c.fail("transition.json isn't valid JSON", Some(rel));
    };
    let settings: TransitionSettings =
        match serde_json::from_value(value.get("transition").cloned().unwrap_or(value.clone())) {
            Ok(s) => s,
            Err(_) => {
                return c.note(
                    "transition.json isn't a transition this renderer reads (a stinger's media are probed instead)",
                    Some(rel),
                );
            }
        };
    let mut ws = Workspace::new("check");
    let a = sample_scene(&mut ws, "A", 0x3A4150);
    let b = sample_scene(&mut ws, "B", 0);
    let canvas = ws.canvases[0].id;
    if let Some(stinger) = value["stinger"].as_str().or(value["file"].as_str()) {
        let cut_ms = value["cut_ms"].as_u64().unwrap_or(settings.duration_ms as u64 / 2) as u32;
        ws.stinger = Some(Stinger { path: folder.join(stinger).to_string_lossy().into_owned(), cut_ms });
    }
    let r = match gpu.renderer() {
        Ok(r) => r,
        Err(e) => return c.note(format!("no GPU here, so the transition wasn't drawn: {e}"), Some(rel)),
    };
    let mut pics = CheckPictures::new(folder.to_path_buf());
    let mut mid = Vec::new();
    for p in [0.25f32, 0.5, 0.75] {
        let f = r.transition(&ws, a, b, canvas, &settings, p, &mut pics, p);
        if p == 0.5 {
            mid = f;
        }
    }
    if judge(c, &mid, "the transition halfway", rel) {
        push_preview(previews, &format!("Transition ({rel})"), &mid, SIZE);
    }
}

fn shaders(c: &mut Check, gpu: &mut Gpu, folder: &Path, rel: &str) {
    let files = files_with(folder, &["wgsl"]);
    if files.is_empty() {
        return c.fail("a shader package needs a .wgsl file", Some(rel));
    }
    if gpu.renderer().is_err() {
        return c.note("no GPU here, so the shaders weren't compiled", Some(rel));
    }
    let Ok(shared) = lightair_render::shared_gpu() else {
        return c.note("no GPU here, so the shaders weren't compiled", Some(rel));
    };
    for f in &files {
        let name = f.strip_prefix(folder).unwrap_or(f).to_string_lossy().into_owned();
        let Ok(code) = std::fs::read_to_string(f) else {
            c.fail("not readable as text", Some(&name));
            continue;
        };
        let t = Instant::now();
        let scope = shared.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _module = shared.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("package shader"),
            source: wgpu::ShaderSource::Wgsl(code.into()),
        });
        let error = pollster::block_on(scope.pop());
        if let Some(e) = error {
            let first = e.to_string().lines().take(6).collect::<Vec<_>>().join(" ");
            c.fail(format!("the shader doesn't compile: {first}"), Some(&name));
        } else if t.elapsed() > Duration::from_secs(5) {
            c.note(format!("the shader took {} ms to compile", t.elapsed().as_millis()), Some(&name));
        }
    }
    c.note("shaders compile here; running them on frames waits for stage C's interface", Some(rel));
}

fn media(c: &mut Check, folder: &Path, rel: &str) {
    let files =
        files_with(folder, &["mp4", "mov", "m4v", "webm", "mkv", "mp3", "m4a", "aac", "wav", "ogg", "opus", "flac"]);
    if files.is_empty() {
        return;
    }
    if lightair_render::media::ffmpeg().is_err() {
        return c.note("FFmpeg isn't here, so video and sound weren't probed", Some(rel));
    }
    for f in files {
        let name = f.strip_prefix(folder).unwrap_or(&f).to_string_lossy().into_owned();
        match lightair_render::media::probe(&f) {
            Ok(_) => {}
            Err(e) => c.fail(format!("FFmpeg can't read it: {e}"), Some(&name)),
        }
    }
}

fn luts(c: &mut Check, folder: &Path, rel: &str) {
    let files = files_with(folder, &["cube"]);
    if files.is_empty() {
        c.note("no .cube file to read", Some(rel));
    }
    for f in files {
        let name = f.strip_prefix(folder).unwrap_or(&f).to_string_lossy().into_owned();
        match std::fs::read_to_string(&f)
            .map_err(|e| e.to_string())
            .and_then(|t| lightair_render::fx::parse_cube(&t).map(|_| ()))
        {
            Ok(()) => {}
            Err(e) => c.fail(format!("not a LUT this renderer reads: {e}"), Some(&name)),
        }
    }
}

/// A recipe: its steps walked (what it calls, where its web requests go, whether it runs
/// scripts), then dry-run against a fake studio when it has no web requests or scripts.
fn recipe(c: &mut Check, folder: &Path, rel: &str, deep: bool) {
    let file = folder.join("automation.json");
    let Ok(bytes) = std::fs::read(&file) else {
        return c.fail("a recipe needs an automation.json", Some(rel));
    };
    let automation: lightair_automation::Automation = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return c.fail(format!("automation.json isn't an automation: {e}"), Some(rel)),
    };
    let steps = serde_json::to_value(&automation.steps).unwrap_or(Value::Null);
    let mut methods = BTreeSet::new();
    let mut hosts = BTreeSet::new();
    let mut scripts = 0;
    let mut waits = 0.0;
    walk(&steps, &mut methods, &mut hosts, &mut scripts, &mut waits);
    if !methods.is_empty() {
        c.note(format!("it calls {}", methods.iter().cloned().collect::<Vec<_>>().join(", ")), Some(rel));
    }
    if !hosts.is_empty() {
        c.note(format!("it makes web requests to {}", hosts.iter().cloned().collect::<Vec<_>>().join(", ")), Some(rel));
    }
    if scripts > 0 {
        c.note(format!("it runs {scripts} script step(s)"), Some(rel));
    }
    if !hosts.is_empty() || scripts > 0 {
        return c.note(
            "not dry-run: it has web requests or scripts (people see every step before switching it on)",
            Some(rel),
        );
    }
    if waits > 30.0 && !deep {
        return c.note(format!("not dry-run: it waits {waits:.0} s in all"), Some(rel));
    }
    let called: Arc<parking_lot::Mutex<Vec<String>>> = Arc::default();
    let rec = called.clone();
    let host: lightair_automation::Host = Arc::new(move |method: &str, _params: Value| {
        rec.lock().push(method.to_string());
        Ok(json!({}))
    });
    let dir = std::env::temp_dir().join(format!("lightair-check-recipe-{}-{}", std::process::id(), Id::new()));
    let runner = lightair_automation::Automations::start(lightair_live::Hub::new(), dir.clone(), host);
    let mut a = automation.clone();
    a.enabled = true;
    let key = a.id.clone();
    runner.set(a);
    let ran = runner.run(&key, None, &[]);
    let until = Instant::now() + Duration::from_secs_f64(waits + 10.0);
    while Instant::now() < until && runner.history().iter().all(|r| !r.done) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&dir);
    match ran {
        Err(e) => c.fail(format!("the dry run didn't start: {e}"), Some(rel)),
        Ok(()) => {
            let calls = called.lock().clone();
            let unknown: Vec<&String> = calls.iter().filter(|m| !methods.contains(*m)).collect();
            if !unknown.is_empty() {
                c.fail(
                    format!(
                        "the dry run called methods its steps don't show: {}",
                        unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                    Some(rel),
                );
            }
            if calls.is_empty() && !methods.is_empty() {
                c.note("the dry run didn't reach its steps in time", Some(rel));
            } else if !calls.is_empty() {
                c.note(format!("dry run: called {}", calls.join(", ")), Some(rel));
            }
        }
    }
}

fn walk(v: &Value, methods: &mut BTreeSet<String>, hosts: &mut BTreeSet<String>, scripts: &mut usize, waits: &mut f64) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| walk(x, methods, hosts, scripts, waits)),
        Value::Object(m) => {
            match m.get("do").and_then(Value::as_str) {
                Some("call") => {
                    if let Some(method) = m.get("method").and_then(Value::as_str) {
                        methods.insert(method.to_string());
                    }
                }
                Some("chat") => {
                    methods.insert("chat.send".into());
                }
                Some("speak") => {
                    methods.insert("voice.say".into());
                }
                Some("sound") => {
                    methods.insert("widgets.sound".into());
                }
                Some("http") => {
                    let url = m.get("url").and_then(Value::as_str).unwrap_or("");
                    let host = url
                        .split("://")
                        .nth(1)
                        .and_then(|r| r.split(['/', '?', '#']).next())
                        .unwrap_or("an address built at run time");
                    hosts.insert(host.to_string());
                }
                Some("script") => *scripts += 1,
                Some("wait") => *waits += m.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
                _ => {}
            }
            m.values().for_each(|x| walk(x, methods, hosts, scripts, waits));
        }
        _ => {}
    }
}

fn files_with(folder: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![folder.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// A frame as a WebP preview, made smaller until it's under the limit.
fn push_preview(previews: &mut Vec<Preview>, name: &str, rgba: &[u8], size: (u32, u32)) {
    if previews.len() >= PREVIEWS {
        return;
    }
    let Some(img) = image::RgbaImage::from_raw(size.0, size.1, rgba.to_vec()) else { return };
    let mut img = image::DynamicImage::ImageRgba8(img);
    for _ in 0..4 {
        let mut bytes = Vec::new();
        let ok = img.write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)).is_ok();
        if ok && bytes.len() <= PREVIEW_MAX {
            use base64::Engine as _;
            previews.push(Preview {
                name: name.to_string(),
                webp_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
            return;
        }
        img = img.thumbnail(img.width() * 3 / 4, img.height() * 3 / 4);
    }
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
