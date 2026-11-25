//! The render thread: at each frame's due time on the program clock it draws the program scene
//! (or the switch in progress, or the privacy cover) on the GPU, then onto the preview and every
//! video encoder.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use lightair_model::{Canvas, Id, TransitionKind, Workspace};
use lightair_render::scene::SceneRenderer;

use crate::clock::{self, frame_ns};
use crate::encode::{Drawn, VideoEncoder};
use crate::gpu::Gpu;
use crate::gpu::window::{Kind, WindowSurface};
use crate::host::EncoderTarget;
use crate::hot::{Hot, Program};
use crate::pictures::{Cache, Frame};

/// A frame read back: RGBA and its size.
pub type Snapshot = (Vec<u8>, (u32, u32));

pub use crate::pictures::SourceFrame;

/// The program's last frame read back whole, with what it was drawn from (comparing a look
/// frame by frame with the desktop's: the feeds' frames say which source frame it shows).
#[derive(Debug, Clone)]
pub struct Captured {
    /// RGBA, `width` × `height`, 4 bytes a pixel, rows top to bottom with no padding.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Its program time.
    pub pts_ns: i64,
    /// The feeds' frames it drew, one per feed.
    pub sources: Vec<SourceFrame>,
}

/// A layer's still for the Looks screen's tiles: RGBA, `width` × `height`, rows top to bottom.
#[derive(Debug, Clone)]
pub struct Still {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What a still is asked for.
#[derive(Debug, Clone)]
pub struct StillRequest {
    pub layer: Id,
    /// Its side, in pixels (a square).
    pub size: u32,
    /// With the layer's looks (beauty, light, grade), or without.
    pub looks: bool,
    /// Beauty settings merged over the layer's own for this still (a Skin tile), with looks.
    pub beauty: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The source id a still's camera is drawn under: the looks' per-source state (its smoothed
/// pose, its targets) stays the program's own; the one slot goes when the project changes.
const STILL_SOURCE: Id = Id(uuid::Uuid::from_u128(0x5714_1100_0000_4000_8000_0000_0000_0001));

impl Captured {
    /// As a lossless PNG of its own size.
    pub fn png(&self) -> Option<Vec<u8>> {
        use image::ImageEncoder;
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&self.rgba, self.width, self.height, image::ExtendedColorType::Rgba8)
            .ok()?;
        Some(out)
    }
}

pub enum Msg {
    /// `handle`: a `CAMetalLayer*` or an `ANativeWindow*` (its reference given up).
    AttachPreview {
        handle: u64,
        size: (u32, u32),
    },
    ResizePreview {
        size: (u32, u32),
    },
    DetachPreview {
        done: Sender<()>,
    },
    AddEncoder(Arc<VideoEncoder>),
    RemoveEncoder(u32),
    /// Stop drawing (iOS in the background: no GPU work); `done` once the GPU is idle.
    Pause {
        done: Sender<()>,
    },
    Resume,
    /// iOS is taking the app to the background while it's live: the program is covered
    /// as by privacy, and each encoder's first covered keyframe is kept (`VideoEncoder::held`)
    /// for the stream to repeat. `false` when it's back.
    Away(bool),
    /// Forget a feed's converted frame (a camera switched).
    ForgetFeed(u32),
    /// The program's last frame read back (a remote controller's preview): the active format,
    /// or `canvas` if it's another one being drawn. RGBA and its size.
    Snapshot {
        canvas: Option<Id>,
        reply: Sender<Option<Snapshot>>,
    },
    /// The active format's last frame read back whole, with what it was drawn from.
    Capture {
        reply: Sender<Option<Captured>>,
    },
    /// A layer alone, cropped square around its face, for the Looks screen's tiles.
    Still {
        request: StillRequest,
        reply: Sender<Option<Still>>,
    },
    Quit,
}

enum Out {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Pool(crate::gpu::apple::PoolTarget),
    Window(WindowSurface),
    Texture(wgpu::Texture),
    None,
}

struct EncoderOut {
    encoder: Arc<VideoEncoder>,
    out: Out,
}

pub struct Renderer {
    hot: Arc<Hot>,
    gpu: Gpu,
    cache: Cache,
    scene: Option<(Id, (u32, u32), SceneRenderer)>,
    preview: Option<WindowSurface>,
    encoders: Vec<EncoderOut>,
    /// The plain privacy cards, by format: (canvas, workspace, scene).
    privacy: Vec<(Id, Arc<Workspace>, Id)>,
    /// The other formats an encoder wants, drawn from the same frames.
    formats: Vec<(Id, (u32, u32), SceneRenderer)>,
    paused: bool,
    /// Covered for the background, and whether the encoders were asked for its keyframe.
    away: Option<bool>,
    /// The camera alone, for a camera-clean recording: its renderer, and its one-object
    /// workspace (by camera source and format).
    clean: Option<(Id, (u32, u32), SceneRenderer)>,
    clean_ws: Option<(Id, Id, Arc<Workspace>, Id)>,
    /// When the project's thumbnail was last taken.
    thumb_at: Option<Instant>,
    /// The last program frame's time and the feeds' frames it drew.
    drawn: (i64, Vec<SourceFrame>),
    /// The preview's own picture while a layer's looks are bypassed there (the Looks screen's
    /// Compare): its renderer, and the workspace with that layer's looks off.
    bypass: Option<((u32, u32), SceneRenderer)>,
    bypass_ws: Option<(usize, Id, Arc<Workspace>)>,
}

/// How often an open project's thumbnail is taken from the program.
const THUMB_EVERY: Duration = Duration::from_secs(10);

pub fn spawn(hot: Arc<Hot>, rx: Receiver<Msg>) -> Result<std::thread::JoinHandle<()>, String> {
    let gpu = Gpu::new()?;
    std::thread::Builder::new()
        .name("lightair-render".into())
        .spawn(move || {
            let mut r = Renderer {
                hot,
                gpu,
                cache: Cache::default(),
                scene: None,
                preview: None,
                encoders: Vec::new(),
                privacy: Vec::new(),
                formats: Vec::new(),
                paused: false,
                away: None,
                clean: None,
                clean_ws: None,
                thumb_at: None,
                drawn: (0, Vec::new()),
                bypass: None,
                bypass_ws: None,
            };
            let hot = r.hot.clone();
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.run(rx))) {
                hot.fail(format!("The picture stopped: {}", crate::hot::panic_text(&e)));
            }
        })
        .map_err(|e| e.to_string())
}

fn fps_of(c: &Canvas) -> u32 {
    (c.fps_num / c.fps_den.max(1)).clamp(1, 120)
}

impl Renderer {
    fn run(&mut self, rx: Receiver<Msg>) {
        let mut next: Option<(u32, u64)> = None; // (fps, frame number)
        loop {
            // what's been asked, every time round: a renderer that's always late still pauses,
            // lets go of a surface and quits when told
            loop {
                match rx.try_recv() {
                    Ok(m) => {
                        if !self.handle(m) {
                            return;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
            // what to draw, and when
            let program = self.hot.program();
            let Some(canvas) = program.workspace.canvas(program.canvas).cloned() else {
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(m) => {
                        if !self.handle(m) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                continue;
            };
            let fps = fps_of(&canvas);
            let n = match next {
                Some((f, n)) if f == fps => n,
                // a new rate: the next frame after now
                _ => (self.hot.clock.now_ns().max(0) as i128 * fps as i128 / 1_000_000_000) as u64 + 1,
            };
            let pts = frame_ns(n, fps);
            let due = self.hot.clock.host_at((pts as i128 * clock::RATE as i128 / 1_000_000_000) as u64);
            let now = clock::host_now_ns();
            let wait =
                if self.paused { Duration::from_millis(500) } else { Duration::from_nanos((due - now).max(0) as u64) };
            if self.paused || wait > Duration::ZERO {
                match rx.recv_timeout(wait.min(Duration::from_millis(50))) {
                    Ok(m) => {
                        if !self.handle(m) {
                            return;
                        }
                        while let Ok(m) = rx.try_recv() {
                            if !self.handle(m) {
                                return;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                next = Some((fps, n));
                continue;
            }
            // late by more than a frame: skip to the frame due now (the timeline never slips)
            let period = 1_000_000_000 / fps as i64;
            let mut n = n;
            if now - due > period {
                let behind = ((now - due) / period) as u64;
                n += behind;
                self.hot.stats.dropped.fetch_add(behind, std::sync::atomic::Ordering::Relaxed);
            }
            let started = Instant::now();
            self.frame(&program, &canvas, n, fps);
            self.hot
                .stats
                .render_us
                .fetch_add(started.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            self.hot.stats.frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            next = Some((fps, n + 1));
        }
    }

    /// False to quit.
    fn handle(&mut self, m: Msg) -> bool {
        match m {
            Msg::AttachPreview { handle, size } => {
                self.preview = unsafe { WindowSurface::new(&self.gpu, handle, size, Kind::Preview) };
            }
            Msg::ResizePreview { size } => {
                if let Some(p) = &mut self.preview {
                    p.resize(&self.gpu, size);
                }
            }
            Msg::DetachPreview { done } => {
                self.preview = None;
                self.gpu.wait();
                let _ = done.send(());
            }
            Msg::AddEncoder(e) => {
                let out = match e.target {
                    #[cfg(any(target_os = "macos", target_os = "ios"))]
                    EncoderTarget::PixelBufferPool(raw) => {
                        unsafe { crate::gpu::apple::PoolTarget::from_raw(raw) }.map_or(Out::None, Out::Pool)
                    }
                    EncoderTarget::NativeWindow(raw) => {
                        let size = (e.config.width, e.config.height);
                        unsafe { WindowSurface::new(&self.gpu, raw, size, Kind::Encoder) }
                            .map_or(Out::None, Out::Window)
                    }
                    EncoderTarget::Texture => Out::Texture(
                        self.gpu.target((e.config.width, e.config.height), wgpu::TextureFormat::Bgra8Unorm),
                    ),
                    #[allow(unreachable_patterns)]
                    _ => Out::None,
                };
                if matches!(out, Out::None) {
                    tracing::warn!(encoder = e.id, "no target to draw the encoder's frames into");
                }
                self.encoders.push(EncoderOut { encoder: e, out });
            }
            Msg::RemoveEncoder(id) => {
                self.encoders.retain(|e| e.encoder.id != id);
                self.gpu.wait();
            }
            Msg::Pause { done } => {
                self.paused = true;
                self.gpu.wait();
                let _ = done.send(());
            }
            Msg::Resume => self.paused = false,
            Msg::Away(on) => self.away = on.then_some(false),
            Msg::ForgetFeed(f) => self.cache.forget_feed(f),
            Msg::Snapshot { canvas, reply } => {
                let other = canvas.and_then(|c| self.formats.iter().find(|f| f.0 == c));
                let target = match other {
                    Some((_, size, r)) => Some((r.target().clone(), *size)),
                    None => self.scene.as_ref().map(|(_, size, r)| (r.target().clone(), *size)),
                };
                let _ = reply
                    .send(target.and_then(|(t, size)| lightair_render::compose::read_texture(&t).map(|px| (px, size))));
            }
            Msg::Capture { reply } => {
                let captured = self.scene.as_ref().and_then(|(_, (w, h), r)| {
                    let rgba = lightair_render::compose::read_texture(r.target())?;
                    Some(Captured { rgba, width: *w, height: *h, pts_ns: self.drawn.0, sources: self.drawn.1.clone() })
                });
                let _ = reply.send(captured);
            }
            Msg::Still { request, reply } => {
                let _ = reply.send(self.still(&request));
            }
            Msg::Quit => {
                self.thumbnail();
                self.encoders.clear();
                self.preview = None;
                self.gpu.wait();
                return false;
            }
        }
        true
    }

    fn renderer(&mut self, program: &Program, canvas: &Canvas) -> Option<&mut SceneRenderer> {
        let size = (canvas.width.max(2), canvas.height.max(2));
        let fresh = !matches!(&self.scene, Some((id, s, _)) if *id == canvas.id && *s == size);
        if fresh {
            let mut r = SceneRenderer::new(size).ok()?;
            r.keep_on_gpu(true);
            self.scene = Some((canvas.id, size, r));
        }
        let (_, _, r) = self.scene.as_mut()?;
        r.set_folder(Some(program.folder.clone()));
        Some(r)
    }

    fn frame(&mut self, program: &Program, canvas: &Canvas, n: u64, fps: u32) {
        let pts = frame_ns(n, fps);
        let due = self.hot.clock.host_at((pts as i128 * clock::RATE as i128 / 1_000_000_000) as u64);
        let time_s = pts as f32 / 1e9;
        // the privacy cover, by program time (the sound is cut from the same sample): the
        // project's own scene, else a plain card
        let covered =
            self.away.is_some() || self.hot.privacy_at((pts as i128 * clock::RATE as i128 / 1_000_000_000) as u64);
        if self.away == Some(false) {
            // this frame is the first covered one: each encoder makes it a keyframe, and keeps it
            self.away = Some(true);
            for e in &self.encoders {
                *e.encoder.held.lock() = crate::encode::Held { catching: true, from_ns: pts, packet: None };
                e.encoder.keyframe_wanted.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let privacy = covered.then(|| self.cover(program, canvas));
        let hot = self.hot.clone();
        // a feed that stopped sending (the app in the background, a call, another app took the
        // camera) shows its last frame dimmed (the desktop's device placeholder, LY-27)
        let dimmed = dim_paused(program, &hot, due);
        let ws = dimmed.as_ref().unwrap_or(&program.workspace);
        // split borrows: the renderer, the cache and the GPU are separate fields
        let Some(_) = self.renderer(program, canvas) else {
            return;
        };
        let (_, _, r) = self.scene.as_mut().unwrap();
        let mut pictures = Frame {
            gpu: &self.gpu,
            cache: &mut self.cache,
            hot: &hot,
            program,
            host_ns: due,
            pts_ns: pts,
            used: Vec::new(),
        };
        draw(r, ws, program, &privacy, canvas.id, &mut pictures, pts, time_s);
        self.drawn = (pts, std::mem::take(&mut pictures.used));
        let target = r.target().clone();
        let others = self.other_formats(program, canvas, ws, covered, due, pts, time_s);
        let clean = self.clean_frame(program, canvas, privacy.clone(), due, pts, time_s);
        let bypassed = self.bypass_frame(program, canvas, ws, &privacy, due, pts, time_s);
        self.cache.end_frame();
        self.cache.keep_sources(&program.workspace);
        self.present(&target, bypassed.as_ref(), canvas.id, &others, clean.as_ref(), pts, n, fps);
        if self.thumb_at.is_none_or(|t| t.elapsed() >= THUMB_EVERY) {
            self.thumbnail();
        }
    }

    /// A layer alone, a square around the face its looks see (the picture's middle without one),
    /// `size` on a side, with its looks, without, or with other beauty settings.
    fn still(&mut self, req: &StillRequest) -> Option<Still> {
        let program = self.hot.program();
        let source = program
            .workspace
            .scenes
            .iter()
            .flat_map(|s| s.objects.iter())
            .find(|o| o.id == req.layer)
            .and_then(|o| o.source)
            .and_then(|s| program.workspace.sources.get(&s))?
            .clone();
        let feed = *program.feeds.get(&source.id)?;
        let now = clock::host_now_ns();
        let (pw, ph, face) = {
            let f = self.hot.feed(feed)?;
            let crate::feeds::Feed::Video(v) = &*f else {
                return None;
            };
            let (picture, _, _) = v.at(now)?;
            let (pw, ph) = picture.upright_size();
            let seen = *v.looks_seen.lock();
            let face = seen.filter(|s| s.present && now - s.at_ns < 1_000_000_000).and_then(|s| s.face_box);
            (pw as f32, ph as f32, face)
        };
        if pw < 2.0 || ph < 2.0 {
            return None;
        }
        // the square, in the picture's pixels: 1.6 times the face's larger side around its
        // middle, else the largest square in the middle; kept inside the picture
        let (cx, cy, side) = match face {
            Some([x, y, w, h]) => ((x + w / 2.0) * pw, (y + h / 2.0) * ph, (w * pw).max(h * ph) * 1.6),
            None => (pw / 2.0, ph / 2.0, pw.min(ph)),
        };
        let side = side.clamp(16.0, pw.min(ph));
        let left = (cx - side / 2.0).clamp(0.0, pw - side);
        let top = (cy - side / 2.0).clamp(0.0, ph - side);
        let size = req.size.clamp(16, 1024);
        let k = size as f32 / side;
        // the camera under the still's own id, its looks as asked
        let mut src = (*source).clone();
        src.id = STILL_SOURCE;
        for e in &mut src.effects {
            if crate::effects::LOOKS.contains(&e.kind.as_str()) && !req.looks {
                e.enabled = false;
            }
        }
        if let (true, Some(extra)) = (req.looks, &req.beauty) {
            match src.effects.iter_mut().find(|e| e.kind == "studio.beauty") {
                Some(e) => {
                    e.enabled = true;
                    let mut m = e.settings.as_object().cloned().unwrap_or_default();
                    m.extend(extra.clone());
                    e.settings = serde_json::Value::Object(m);
                }
                None => src.effects.push(lightair_model::Effect::new(
                    "Beauty",
                    "studio.beauty",
                    serde_json::Value::Object(extra.clone()),
                )),
            }
        }
        let canvas =
            Canvas { id: STILL_SOURCE, name: "Still".into(), width: size, height: size, fps_num: 30, fps_den: 1 };
        let mut ws = Workspace::new("Still");
        ws.canvases = vec![canvas.clone()];
        let layout = lightair_model::Layout::boxed(-left * k, -top * k, pw * k, ph * k);
        let object = lightair_model::Object::new("Still", STILL_SOURCE, [(canvas.id, layout)]);
        let mut scene = lightair_model::Scene::new("Still");
        scene.objects = vec![object];
        let scene_id = scene.id;
        ws.scenes = vec![Arc::new(scene)];
        ws.sources.insert(STILL_SOURCE, Arc::new(src));
        let mut still_program = (*program).clone();
        still_program.feeds.insert(STILL_SOURCE, feed);
        let mut r = SceneRenderer::new((size, size)).ok()?;
        r.keep_on_gpu(true);
        r.set_folder(Some(program.folder.clone()));
        let hot = self.hot.clone();
        let mut pictures = Frame {
            gpu: &self.gpu,
            cache: &mut self.cache,
            hot: &hot,
            program: &still_program,
            host_ns: now,
            pts_ns: self.drawn.0,
            used: Vec::new(),
        };
        r.frame(&ws, scene_id, canvas.id, &mut pictures, self.drawn.0 as f32 / 1e9);
        let rgba = lightair_render::compose::read_texture(r.target())?;
        Some(Still { rgba, width: size, height: size })
    }

    /// The program again with one layer's looks off, for the preview alone, while the Looks
    /// screen's Compare is held (`Hot::preview_bypass`); the encoders keep the looked picture.
    #[allow(clippy::too_many_arguments)]
    fn bypass_frame(
        &mut self,
        program: &Program,
        canvas: &Canvas,
        ws: &Arc<Workspace>,
        privacy: &Option<(Arc<Workspace>, Id)>,
        due: i64,
        pts: i64,
        time_s: f32,
    ) -> Option<wgpu::Texture> {
        let layer = *self.hot.preview_bypass.lock();
        let Some(layer) = layer.filter(|_| self.preview.is_some() && privacy.is_none()) else {
            self.bypass = None;
            self.bypass_ws = None;
            return None;
        };
        let at = Arc::as_ptr(ws) as usize;
        if !matches!(&self.bypass_ws, Some((p, l, _)) if *p == at && *l == layer) {
            let mut w = (**ws).clone();
            let source = w.scenes.iter().flat_map(|s| s.objects.iter()).find(|o| o.id == layer).and_then(|o| o.source);
            if let Some(src) = source.and_then(|s| w.sources.get_mut(&s)) {
                for e in &mut Arc::make_mut(src).effects {
                    if crate::effects::LOOKS.contains(&e.kind.as_str()) {
                        e.enabled = false;
                    }
                }
            }
            self.bypass_ws = Some((at, layer, Arc::new(w)));
        }
        let bypass_ws = self.bypass_ws.as_ref()?.2.clone();
        let size = (canvas.width.max(2), canvas.height.max(2));
        if !matches!(&self.bypass, Some((s, _)) if *s == size) {
            let mut r = SceneRenderer::new(size).ok()?;
            r.keep_on_gpu(true);
            self.bypass = Some((size, r));
        }
        let (_, r) = self.bypass.as_mut()?;
        r.set_folder(Some(program.folder.clone()));
        let hot = self.hot.clone();
        let mut pictures = Frame {
            gpu: &self.gpu,
            cache: &mut self.cache,
            hot: &hot,
            program,
            host_ns: due,
            pts_ns: pts,
            used: Vec::new(),
        };
        draw(r, &bypass_ws, program, privacy, canvas.id, &mut pictures, pts, time_s);
        Some(r.target().clone())
    }

    /// The privacy cover in `canvas`: the project's privacy scene, else a plain card.
    fn cover(&mut self, program: &Program, canvas: &Canvas) -> (Arc<Workspace>, Id) {
        if let Some(s) = program.privacy_scene {
            return (program.workspace.clone(), s);
        }
        if let Some((_, ws, scene)) = self.privacy.iter().find(|p| p.0 == canvas.id) {
            return (ws.clone(), *scene);
        }
        let (ws, scene) = privacy_card(canvas);
        let ws = Arc::new(ws);
        self.privacy.push((canvas.id, ws.clone(), scene));
        (ws, scene)
    }

    /// The program in each other format an open encoder wants, from the same frames:
    /// (format, its texture).
    #[allow(clippy::too_many_arguments)]
    fn other_formats(
        &mut self,
        program: &Program,
        active: &Canvas,
        ws: &Workspace,
        covered: bool,
        due: i64,
        pts: i64,
        time_s: f32,
    ) -> Vec<(Id, wgpu::Texture)> {
        let mut wanted: Vec<Id> = self
            .encoders
            .iter()
            .filter(|e| !e.encoder.clean && !e.encoder.is_closed())
            .filter_map(|e| e.encoder.format)
            .filter(|f| *f != active.id)
            .collect();
        wanted.sort();
        wanted.dedup();
        self.formats.retain(|f| wanted.contains(&f.0));
        let hot = self.hot.clone();
        let mut out = Vec::new();
        for id in wanted {
            let Some(canvas) = program.workspace.canvas(id).cloned() else {
                continue;
            };
            let size = (canvas.width.max(2), canvas.height.max(2));
            if !self.formats.iter().any(|f| f.0 == id && f.1 == size) {
                let Ok(mut r) = SceneRenderer::new(size) else {
                    continue;
                };
                r.keep_on_gpu(true);
                self.formats.retain(|f| f.0 != id);
                self.formats.push((id, size, r));
            }
            let privacy = covered.then(|| self.cover(program, &canvas));
            let Some((_, _, r)) = self.formats.iter_mut().find(|f| f.0 == id) else {
                continue;
            };
            r.set_folder(Some(program.folder.clone()));
            let mut pictures = Frame {
                gpu: &self.gpu,
                cache: &mut self.cache,
                hot: &hot,
                program,
                host_ns: due,
                pts_ns: pts,
                used: Vec::new(),
            };
            draw(r, ws, program, &privacy, id, &mut pictures, pts, time_s);
            out.push((id, r.target().clone()));
        }
        out
    }

    /// The program's last frame as the project's thumbnail (read back, then scaled and written
    /// on a thread of its own).
    fn thumbnail(&mut self) {
        let Some((_, size, r)) = &self.scene else {
            return;
        };
        self.thumb_at = Some(Instant::now());
        let Some(rgba) = lightair_render::compose::read_texture(r.target()) else {
            return;
        };
        let path = self.hot.program().folder.join(crate::thumb::FILE);
        let size = *size;
        let _ = std::thread::Builder::new().name("lightair-thumbnail".into()).spawn(move || {
            if let Err(e) = crate::thumb::save(rgba, size, &path) {
                tracing::warn!(%e, "the project's thumbnail");
            }
        });
    }

    /// The camera alone, as the format shows it filled (or the privacy cover while that's on),
    /// when a camera-clean encoder wants it.
    fn clean_frame(
        &mut self,
        program: &Program,
        canvas: &Canvas,
        covered: Option<(Arc<Workspace>, Id)>,
        due: i64,
        pts: i64,
        time_s: f32,
    ) -> Option<wgpu::Texture> {
        if !self.encoders.iter().any(|e| e.encoder.clean && !e.encoder.is_closed()) {
            self.clean = None;
            return None;
        }
        let size = (canvas.width.max(2), canvas.height.max(2));
        if !matches!(&self.clean, Some((id, s, _)) if *id == canvas.id && *s == size) {
            let mut r = SceneRenderer::new(size).ok()?;
            r.keep_on_gpu(true);
            self.clean = Some((canvas.id, size, r));
        }
        // the program scene's camera
        let scene = program.workspace.scene(program.scene)?;
        let camera = scene
            .objects
            .iter()
            .filter_map(|o| program.workspace.sources.get(&o.source?))
            .find(|s| s.kind == crate::templates::CAMERA && program.feeds.contains_key(&s.id))
            .cloned();
        let (ws, scene) = if let Some(cover) = covered {
            cover
        } else if let Some(cam) = camera {
            let fresh = !matches!(&self.clean_ws, Some((s, c, _, _)) if *s == cam.id && *c == canvas.id);
            if fresh {
                let mut ws = Workspace::new("Camera");
                ws.canvases = vec![canvas.clone()];
                let o = crate::templates::object_filling(&ws, &cam, lightair_model::Fit::Fill);
                let mut sc = lightair_model::Scene::new("Camera");
                sc.objects = vec![o];
                let scene = sc.id;
                ws.scenes = vec![Arc::new(sc)];
                ws.sources.insert(cam.id, cam.clone());
                self.clean_ws = Some((cam.id, canvas.id, Arc::new(ws), scene));
            }
            let (_, _, ws, scene) = self.clean_ws.as_ref()?;
            (ws.clone(), *scene)
        } else {
            // no camera in this scene: black frames keep the file's timing
            let fresh = !matches!(&self.clean_ws, Some((s, c, _, _)) if *s == Id::default() && *c == canvas.id);
            if fresh {
                let mut ws = Workspace::new("Camera");
                ws.canvases = vec![canvas.clone()];
                let sc = lightair_model::Scene::new("Camera");
                let scene = sc.id;
                ws.scenes = vec![Arc::new(sc)];
                self.clean_ws = Some((Id::default(), canvas.id, Arc::new(ws), scene));
            }
            let (_, _, ws, scene) = self.clean_ws.as_ref()?;
            (ws.clone(), *scene)
        };
        let hot = self.hot.clone();
        let (_, _, r) = self.clean.as_mut()?;
        let mut pictures = Frame {
            gpu: &self.gpu,
            cache: &mut self.cache,
            hot: &hot,
            program,
            host_ns: due,
            pts_ns: pts,
            used: Vec::new(),
        };
        r.frame(&ws, scene, canvas.id, &mut pictures, time_s);
        Some(r.target().clone())
    }

    #[allow(clippy::too_many_arguments)]
    fn present(
        &mut self,
        program: &wgpu::Texture,
        preview_only: Option<&wgpu::Texture>,
        active: Id,
        others: &[(Id, wgpu::Texture)],
        clean: Option<&wgpu::Texture>,
        pts: i64,
        n: u64,
        fps: u32,
    ) {
        let mut encoder = self.gpu.device().create_command_encoder(&Default::default());
        let mut presents = Vec::new();
        let mut preview_tex = None;
        // the thermal governor's steps: the preview and the encoders every nth frame
        let every = |a: &std::sync::atomic::AtomicU32| {
            n.is_multiple_of(a.load(std::sync::atomic::Ordering::Relaxed).max(1) as u64)
        };
        let (draw_preview, draw_encoders) = (every(&self.hot.preview_every), every(&self.hot.encode_every));
        if draw_preview
            && let Some(p) = &self.preview
            && let Some(t) = p.texture(&self.gpu)
        {
            let view = t.texture.create_view(&Default::default());
            let format = p.format();
            preview_tex = Some((t, view, format));
        }
        if let Some((_, view, format)) = &preview_tex {
            self.gpu.blit(&mut encoder, preview_only.unwrap_or(program), view, *format);
        }
        // each encoder's buffer or surface
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let mut buffers: Vec<(usize, objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>)> = Vec::new();
        let mut readbacks: Vec<(usize, wgpu::Texture)> = Vec::new();
        let encoders = std::mem::take(&mut self.encoders);
        for (i, e) in encoders.iter().enumerate() {
            if e.encoder.is_closed() || !draw_encoders {
                continue;
            }
            // an encode at a lower frame rate (a destination's own, 30 of 60) takes every nth
            let nth = (fps / e.encoder.config.fps.max(1)).max(1) as u64;
            if !n.is_multiple_of(nth) {
                continue;
            }
            let program = match (e.encoder.clean, clean, e.encoder.format) {
                (true, Some(c), _) => c,
                (true, None, _) => continue,
                // another format
                (false, _, Some(f)) if f != active => match others.iter().find(|o| o.0 == f) {
                    Some((_, t)) => t,
                    None => continue,
                },
                (false, _, _) => program,
            };
            match &e.out {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                Out::Pool(pool) => match pool.next(self.gpu.device()) {
                    Some((buffer, texture)) => {
                        let view = texture.create_view(&Default::default());
                        self.gpu.blit(&mut encoder, program, &view, wgpu::TextureFormat::Bgra8Unorm);
                        buffers.push((i, buffer));
                    }
                    None => {
                        e.encoder.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                Out::Window(w) => {
                    if let Some(t) = w.texture(&self.gpu) {
                        let view = t.texture.create_view(&Default::default());
                        self.gpu.blit(&mut encoder, program, &view, w.format());
                        presents.push((i, t));
                    }
                }
                Out::Texture(t) => {
                    let view = t.create_view(&Default::default());
                    self.gpu.blit(&mut encoder, program, &view, t.format());
                    readbacks.push((i, t.clone()));
                }
                Out::None => {}
            }
        }
        self.gpu.queue().submit([encoder.finish()]);
        if let Some((t, _, _)) = preview_tex {
            self.gpu.queue().present(t);
        }
        for (i, t) in presents {
            self.gpu.queue().present(t);
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            encoders[i].encoder.presented(clock::monotonic_now_ns(), pts);
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let _ = i;
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let pulled = !buffers.is_empty();
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let pulled = false;
        if pulled || !readbacks.is_empty() {
            // the encoder may read the buffer only once it's drawn
            self.gpu.wait();
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        for (i, b) in buffers {
            let buffer = crate::gpu::apple::into_raw(b);
            encoders[i].encoder.queue(Drawn { buffer, pts_ns: pts, keyframe: false, rgba: None });
        }
        for (i, t) in readbacks {
            let rgba = lightair_render::compose::read_texture(&t);
            encoders[i].encoder.queue(Drawn { buffer: 0, pts_ns: pts, keyframe: false, rgba });
        }
        self.encoders = encoders;
    }
}

/// One format's program: the privacy cover, the switch in progress, or the program scene.
#[allow(clippy::too_many_arguments)]
fn draw(
    r: &mut SceneRenderer,
    ws: &Workspace,
    program: &Program,
    privacy: &Option<(Arc<Workspace>, Id)>,
    canvas: Id,
    pictures: &mut Frame,
    pts: i64,
    time_s: f32,
) {
    match (privacy, &program.switch) {
        (Some((ws, scene)), _) => {
            r.frame(ws, *scene, canvas, pictures, time_s);
        }
        (None, Some(sw)) if pts < sw.start_ns + sw.settings.duration_ms as i64 * 1_000_000 => {
            let p = (pts - sw.start_ns).max(0) as f32 / (sw.settings.duration_ms.max(1) as f32 * 1e6);
            let mut settings = sw.settings;
            if settings.kind == TransitionKind::Stinger {
                settings.kind = TransitionKind::Fade;
            }
            r.transition(ws, sw.from, program.scene, canvas, &settings, p, pictures, time_s);
        }
        _ => {
            r.frame(ws, program.scene, canvas, pictures, time_s);
        }
    }
}

/// How long a feed may send nothing before its picture is drawn as paused.
pub const PAUSED_NS: i64 = 500_000_000;

/// The program's workspace with every source whose feed has gone quiet dimmed (a colour filter
/// the renderer draws); `None` when every feed is sending.
/// Whether the session's program would be drawn with paused feeds now (tests).
#[cfg(test)]
pub fn dim_paused_for_test(session: &crate::session::Session, now_ns: i64) -> Option<Arc<Workspace>> {
    let program = session.hot().program.read().clone();
    dim_paused(&program, session.hot(), now_ns)
}

fn dim_paused(program: &Program, hot: &Hot, now_ns: i64) -> Option<Arc<Workspace>> {
    let quiet: Vec<Id> = program
        .feeds
        .iter()
        .filter(|(_, f)| !program.frozen.contains(f))
        // a still screen sends no frames (ScreenCaptureKit, ReplayKit): its last one stands, and
        // the host says when sharing stopped
        .filter(|(s, _)| program.workspace.sources.get(*s).is_none_or(|s| s.kind != crate::templates::SCREEN))
        .filter(|(_, f)| {
            hot.feed(**f).is_some_and(|f| {
                f.video().is_some_and(|v| {
                    let last = v.arrived_ns.load(std::sync::atomic::Ordering::Acquire);
                    last != i64::MIN && now_ns - last > PAUSED_NS
                })
            })
        })
        .map(|(s, _)| *s)
        .collect();
    if quiet.is_empty() {
        return None;
    }
    let mut ws = (*program.workspace).clone();
    for id in quiet {
        if let Some(s) = ws.sources.get(&id) {
            let mut s = (**s).clone();
            s.effects.push(lightair_model::Effect::new(
                "Paused",
                "color_filter_v2",
                serde_json::json!({ "brightness": -0.35, "saturation": -0.7 }),
            ));
            ws.sources.insert(id, Arc::new(s));
        }
    }
    Some(Arc::new(ws))
}

/// The plain privacy cover: dark, with "Be right back".
fn privacy_card(canvas: &Canvas) -> (Workspace, Id) {
    use lightair_model::{Fit, Layout, Object, Scene};
    let mut ws = Workspace::new("Privacy");
    ws.canvases = vec![canvas.clone()];
    let bg = crate::templates::source(
        "Cover",
        crate::templates::COLOUR,
        crate::templates::colour_settings(0xFF10_1014, 16, 16),
    );
    let size = (canvas.width.min(canvas.height) as f32 / 12.0).max(24.0);
    let text = crate::templates::source(
        "Be right back",
        crate::templates::TEXT,
        crate::templates::text_settings("Be right back", None, size, 0xFFFFFF, true, 1, None, None),
    );
    let mut full = Layout::boxed(0.0, 0.0, canvas.width as f32, canvas.height as f32);
    full.fit = Fit::Stretch;
    let mut middle = Layout::boxed(0.0, canvas.height as f32 * 0.4, canvas.width as f32, canvas.height as f32 * 0.2);
    middle.fit = Fit::Fit;
    let mut scene = Scene::new("Privacy");
    scene.objects =
        vec![Object::new("Cover", bg.id, [(canvas.id, full)]), Object::new("Text", text.id, [(canvas.id, middle)])];
    let id = scene.id;
    ws.sources.insert(bg.id, Arc::new(bg));
    ws.sources.insert(text.id, Arc::new(text));
    ws.scenes = vec![Arc::new(scene)];
    (ws, id)
}
