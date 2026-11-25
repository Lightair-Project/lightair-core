//! Projects' thumbnails (the projects list): `thumbnail.jpg` in each project's folder,
//! from the live program while the project is open, and rendered once when a project is made or
//! imported (cameras aren't there then: what else the scene shows).

use std::path::Path;
use std::sync::Arc;

use lightair_render::scene::SceneRenderer;

use crate::gpu::Gpu;
use crate::hot::{Hot, Program};

pub const FILE: &str = "thumbnail.jpg";

/// The longest side a thumbnail has.
const LONG: u32 = 480;

/// Scales an RGBA frame to a thumbnail and writes it (a temporary file, then renamed).
pub fn save(rgba: Vec<u8>, size: (u32, u32), path: &Path) -> std::io::Result<()> {
    let img = image::RgbaImage::from_raw(size.0, size.1, rgba)
        .ok_or_else(|| std::io::Error::other("a frame of the wrong size"))?;
    let k = LONG as f32 / size.0.max(size.1).max(1) as f32;
    let (w, h) = (((size.0 as f32 * k) as u32).max(1), ((size.1 as f32 * k) as u32).max(1));
    let small = image::imageops::thumbnail(&img, w, h);
    let rgb = image::DynamicImage::ImageRgba8(small).to_rgb8();
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80)
        .encode_image(&rgb)
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("jpg.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Renders the project in `dir` (its program scene in its first format) and saves its
/// thumbnail. Needs the GPU; slow-ish the first time (shaders): call it off the main thread.
pub fn render(dir: &Path) -> Result<(), String> {
    let ws = Arc::new(lightair_store::portable::load_current(dir).map_err(|e| e.to_string())?);
    let canvas = ws.canvases.first().cloned().ok_or("no format")?;
    let scene =
        ws.program.filter(|p| ws.scene(*p).is_some()).or_else(|| ws.scenes.first().map(|s| s.id)).ok_or("no scene")?;
    let k = LONG as f32 / canvas.width.max(canvas.height).max(1) as f32;
    let size = (((canvas.width as f32 * k) as u32).max(2), ((canvas.height as f32 * k) as u32).max(2));
    let gpu = Gpu::new()?;
    let program = Program {
        workspace: ws.clone(),
        folder: dir.to_path_buf(),
        scene,
        canvas: canvas.id,
        switch: None,
        privacy_scene: None,
        feeds: Default::default(),
        on_air_ns: 0,
        utc_offset_min: 0,
        frozen: Vec::new(),
    };
    let hot = Hot::new(program.clone(), 0);
    let mut cache = crate::pictures::Cache::default();
    let mut r = SceneRenderer::new(size)?;
    r.set_folder(Some(dir.to_path_buf()));
    // images load on threads of their own: draw again until they're in (3 s at most)
    let started = std::time::Instant::now();
    let rgba = loop {
        let mut pictures = crate::pictures::Frame {
            gpu: &gpu,
            cache: &mut cache,
            hot: &hot,
            program: &program,
            host_ns: 0,
            pts_ns: 0,
            used: Vec::new(),
        };
        let rgba = r.frame(&ws, scene, canvas.id, &mut pictures, 0.0);
        if !cache.loading() || started.elapsed() > std::time::Duration::from_secs(3) {
            break rgba;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    save(rgba, size, &dir.join(FILE)).map_err(|e| e.to_string())
}

/// Renders a project's thumbnail on a thread of its own (a new or imported project).
pub fn render_soon(dir: &Path) {
    let dir = dir.to_path_buf();
    let _ = std::thread::Builder::new().name("lightair-thumbnail".into()).spawn(move || {
        if let Err(e) = render(&dir) {
            tracing::warn!(%e, "a project's thumbnail");
        }
    });
}
