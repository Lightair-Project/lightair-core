//! An open Studio project: the control thread that owns the document and asks the host for
//! captures and encoders, and the calls the interface makes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lightair_audio as la;
use lightair_model::{
    Canvas, Command, Document, Effect, Fit, Id, Layout, Object, Scene, Shape,
    ShapeKind as ModelShape, Source, Track, TransitionKind, TransitionSettings, Workspace,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};

use crate::audio::{self, Routing};
use crate::encode::{AudioEncoder, VideoEncoder};
use crate::feeds::{AudioFeed, Feed, VideoFeed};
use crate::gesture::{self, Delta, Feedback, LayerFrame, View};
use crate::host::*;
use crate::hot::{self, Hot, Program, Switch};
use crate::outputs::stream_size;
use crate::project::Projects;
use crate::render;
use crate::templates;

// ── What the interface asks for ─────────────────────────────────────────────────────────────

/// The alerts waiting for the alert box (the capped queue the broadcast extension uses too).
pub type AlertQueue = crate::alerts::Queue;

#[derive(Debug, Clone, PartialEq)]
pub struct TextSpec {
    pub text: String,
    pub font_family: Option<String>,
    pub size: f32,
    /// 0xRRGGBB.
    pub color: u32,
    pub bold: bool,
    /// 0 left, 1 centre, 2 right.
    pub align: u8,
    pub outline: Option<u32>,
    pub background: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Rectangle,
    Ellipse,
    Line,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShapeSpec {
    pub kind: ShapeKind,
    /// 0xAARRGGBB.
    pub fill: u32,
    pub stroke: Option<u32>,
    pub stroke_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerMode {
    CountUp,
    CountDown,
    Clock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimerSpec {
    pub mode: TimerMode,
    pub seconds: u32,
    pub text: TextSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NewLayer {
    Camera {
        facing: Facing,
    },
    Image {
        path: String,
    },
    Video {
        path: String,
        looped: bool,
        with_sound: bool,
    },
    Gif {
        path: String,
    },
    Text {
        text: TextSpec,
    },
    Shape {
        shape: ShapeSpec,
    },
    Timer {
        timer: TimerSpec,
    },
    Colour {
        argb: u32,
    },
    /// A widget of the desktop's native set ("chat-box", "alert-box", "event-list", "goal",
    /// "label", "counter").
    Widget {
        kind: String,
    },
    /// The phone's screen; its sound on its own track when `with_sound`. One per
    /// project: a second one shows the same screen.
    Screen {
        with_sound: bool,
    },
    /// An external camera (a USB-C camera or capture card the app found), by the
    /// platform's id for it, and its name.
    ExternalCamera {
        device: String,
        name: String,
    },
    /// An imported custom widget: a `.lwidget` (zip) package or a folder of its files
    /// (StreamElements' HTML, CSS, JS and fields). Drawn by the host's web view.
    ImportedWidget {
        path: String,
    },
}

/// The screen's feeds, for the host to push its frames and sound into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenFeeds {
    /// The screen's source.
    pub source: Id,
    pub video: u32,
    pub audio: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutPatch {
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub rotation: Option<f32>,
    pub opacity: Option<f32>,
    pub corner_radius: Option<f32>,
    pub crop: [Option<u32>; 4],
    pub flip_h: Option<bool>,
    pub flip_v: Option<bool>,
}

/// Something that carries the show while the app is away (iOS 17–26's broadcast extension):
/// the session pauses it while the app is in front and lets it take over when the app leaves.
pub trait AwayExtension: Send {
    /// Set while the app is in front (the extension waits), cleared when it takes over.
    fn paused(&self) -> Arc<AtomicBool>;
}

/// What the interface draws (`StudioSnapshot` in the facade).
#[derive(Clone)]
pub struct State {
    pub version: u64,
    pub workspace: Arc<Workspace>,
    pub project_id: String,
    pub folder: PathBuf,
    pub canvas: Id,
    pub program: Id,
    pub inputs: Vec<AudioInput>,
    pub active_input: Option<String>,
    pub recording: RecordingState,
    pub live: LiveState,
    pub privacy: bool,
    pub undo: Option<String>,
    pub redo: Option<String>,
    /// While the project has a screen layer.
    pub screen: Option<ScreenFeeds>,
    /// The program's web layers (imported widgets, pages), for the host's web views.
    pub web: Vec<crate::web::WebFeed>,
}

impl State {
    pub fn scene(&self) -> Option<&Arc<Scene>> {
        self.workspace.scene(self.program)
    }
    pub fn canvas(&self) -> Option<&Canvas> {
        self.workspace.canvas(self.canvas)
    }
}

/// A scene named "Be right back" or "BRB" is the project's privacy cover.
pub fn is_privacy_scene(s: &Scene) -> bool {
    let n = s.name.trim().to_ascii_lowercase();
    n == "be right back" || n == "brb"
}

/// Why a layer is drawn as a placeholder on the phone, if it is.
pub fn unsupported(ws: &Workspace, o: &Object) -> Option<String> {
    let s = ws.sources.get(&o.source?)?;
    if !crate::pictures::drawn(&s.kind) {
        return Some(format!("Not shown on phones yet ({})", s.kind));
    }
    s.effects
        .iter()
        .find(|e| {
            e.enabled
                && (!crate::effects::drawn(&e.kind) && !e.kind.starts_with("audio.")
                    // a LUT phones draw only from a .cube file
                    || e.kind == "clut_filter" && !is_cube(&e.settings))
        })
        .map(|e| format!("Effect not shown on phones: {}", e.name))
}

/// A LUT effect's file is a .cube (the kind phones draw).
fn is_cube(settings: &Value) -> bool {
    settings["image_path"]
        .as_str()
        .is_some_and(|p| p.to_ascii_lowercase().ends_with(".cube"))
}

/// A .cube LUT that can be read: a 3D table (`LUT_3D_SIZE` 2–256) with all its rows.
pub fn check_cube(path: &Path) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|_| "The file couldn't be read.".to_string())?;
    let mut size = None;
    let mut rows = 0usize;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(n) = line.strip_prefix("LUT_3D_SIZE") {
            size = n.trim().parse::<usize>().ok();
            continue;
        }
        if line.starts_with("LUT_1D_SIZE") {
            return Err("That's a 1D LUT; phones take 3D .cube LUTs.".into());
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() == 3 && words.iter().all(|w| w.parse::<f32>().is_ok()) {
            rows += 1;
        }
    }
    let n = size
        .filter(|n| (2..=256).contains(n))
        .ok_or("That isn't a 3D .cube LUT (no LUT_3D_SIZE).")?;
    if rows != n * n * n {
        return Err(format!(
            "The LUT is incomplete ({rows} of {} entries).",
            n * n * n
        ));
    }
    Ok(())
}

/// Copies a file into a project's `assets/` under a unique name; its path relative to the
/// project.
fn copy_asset(folder: &Path, from: &Path) -> Option<String> {
    let name = from.file_name()?.to_string_lossy().to_string();
    let rel = format!(
        "assets/{}-{}",
        &uuid::Uuid::new_v4().to_string()[..8],
        lightair_model::file_safe(&name)
    );
    let to = folder.join(&rel);
    std::fs::create_dir_all(to.parent()?).ok()?;
    std::fs::copy(from, &to).ok()?;
    Some(rel)
}

/// A command made on the control thread from the document as it is then.
type MakeCommand = Box<dyn FnOnce(&Workspace) -> Option<Command> + Send>;

enum Msg {
    Apply(Command),
    Undo,
    Redo,
    Refresh,
    SwitchScene(Id),
    SetFormat(Id),
    AddFormat(bool),
    AddSizedFormat {
        width: u32,
        height: u32,
        reply: Sender<Option<Id>>,
    },
    AddLayer(NewLayer),
    Privacy,
    StartRecording {
        camera_clean: bool,
    },
    StopRecording,
    SelectMic(String),
    FeedState(u32, FeedState),
    Inputs(Vec<AudioInput>, Option<String>),
    RecordingFinished(u32, RecordingResult),
    EncoderFailed(u32, String),
    /// Into the background: a recording stops (the render thread is paused by the session).
    Background,
    Foreground,
    GoLive(Vec<crate::outputs::Destination>, Start),
    /// iOS 17–26: the app's destinations handed to the broadcast extension while the
    /// app is away, and taken back.
    HandOver,
    TakeBack,
    /// A destination stopped while live, started again, or skipped.
    StopDestination(String),
    /// The destinations' order and one destination's new settings.
    Reorder(Vec<String>),
    Update(crate::outputs::Destination),
    StartDestination(String),
    SkipDestination(String),
    /// The destinations with their servers and keys found (off the control thread), what
    /// couldn't be (id, why), and whether to record.
    Resolved(
        Vec<crate::outputs::Destination>,
        Vec<(String, String)>,
        Vec<String>,
        Start,
    ),
    /// Something to say that came from another thread (a chat message not sent).
    Said(String),
    /// A command made from the document as it is when it's applied (not the interface's
    /// snapshot, which may be a step behind): settings merged, effects moved.
    Edit(MakeCommand),
    AlertSettings(crate::alerts::Settings),
    TestAlert(String),
    EndLive,
    Device(DeviceState),
    Close(Sender<()>),
}

struct Gesture {
    scene: Id,
    object: Id,
    canvas: Id,
    start: Layout,
    now: Layout,
    others: Vec<Layout>,
    last_snap: Option<f32>,
    turning: bool,
}

pub struct Session {
    pub handle: u64,
    hot: Arc<Hot>,
    tx: Sender<Msg>,
    render: Sender<render::Msg>,
    state: Arc<RwLock<Arc<State>>>,
    view: Mutex<View>,
    gesture: Mutex<Option<Gesture>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    chat: Arc<crate::chat::Chat>,
    quit_mix: Arc<AtomicBool>,
    closed: AtomicBool,
    device: crate::device::DeviceInfo,
    /// The folder shared with Lightair Editor (recordings, clips).
    shared: PathBuf,
    /// The app's own folder (remote control's keys and paired devices).
    app_dir: PathBuf,
    /// Remote control of this Studio from a paired desktop or phone, while on.
    remote: Mutex<Option<crate::remote::Remote>>,
    /// Remote control's TLS port (4456; tests: any).
    pub remote_port: std::sync::atomic::AtomicU32,
    /// The destinations a controller's `stream.start` goes live on (the app's, kept in memory).
    remote_destinations: Mutex<Vec<crate::outputs::Destination>>,
    /// iOS 27+: the app keeps capturing the screen in the background (the
    /// `screen-capture` mode), so drawing and encoding go on there.
    background_capture: AtomicBool,
    /// The app is in the background (`lifecycle`), and whether the program was covered and its
    /// drawing paused when it left: screen capture that starts or ends while the app is away
    /// changes which it should be.
    in_background: AtomicBool,
    away_paused: AtomicBool,
    /// What carries the show while the app is away, when the app attached one (iOS 17–26's
    /// broadcast extension).
    extension: Mutex<Option<Box<dyn AwayExtension>>>,
    /// Image layers' picture sizes, read from their files' headers (`source_size`).
    image_sizes: Mutex<HashMap<PathBuf, (u32, u32)>>,
}

impl Projects {
    /// Opens project `id` (one session at a time: the caller closes the last first).
    pub fn open_session(
        &self,
        id: &str,
        host: Arc<dyn Host>,
        events: Arc<dyn Events>,
    ) -> Result<Arc<Session>, crate::project::ProjectError> {
        let dir = self.dir(id)?;
        let opened =
            lightair_store::Package::open(&dir, || templates::project("My studio", 1080, 30))?;
        let doc = Document::new(opened.workspace);
        let ws = doc.workspace().clone();
        let program = ws
            .program
            .or_else(|| ws.scenes.first().map(|s| s.id))
            .unwrap_or_default();
        let canvas = ws.canvases.first().map(|c| c.id).unwrap_or_default();
        let now = crate::clock::host_now_ns();
        let hot = Arc::new(Hot::new(
            Program {
                workspace: ws.clone(),
                folder: dir.clone(),
                scene: program,
                canvas,
                switch: None,
                privacy_scene: None,
                feeds: HashMap::new(),
                on_air_ns: 0,
                utc_offset_min: self.device.utc_offset_min,
                frozen: Vec::new(),
            },
            now,
        ));
        let handle = hot::register(hot.clone());
        let state = Arc::new(RwLock::new(Arc::new(State {
            version: 1,
            workspace: ws.clone(),
            project_id: id.to_string(),
            folder: dir.clone(),
            canvas,
            program,
            inputs: Vec::new(),
            active_input: None,
            recording: RecordingState::Idle,
            live: LiveState::Off,
            privacy: false,
            undo: None,
            redo: None,
            screen: None,
            web: Vec::new(),
        })));
        let (render_tx, render_rx) = mpsc::channel();
        let render_thread = match render::spawn(hot.clone(), render_rx) {
            Ok(t) => t,
            Err(e) => {
                hot::unregister(handle);
                return Err(crate::project::ProjectError::Store(format!(
                    "no graphics: {e}"
                )));
            }
        };
        let (engine, renderer) = la::Engine::new(la::GraphSpec::default(), crate::clock::RATE);
        let routing = Arc::new(Mutex::new(Routing::default()));
        let quit_mix = Arc::new(AtomicBool::new(false));
        let mix_thread = audio::spawn(hot.clone(), renderer, routing.clone(), quit_mix.clone())
            .map_err(|e| crate::project::ProjectError::Store(e.to_string()))?;
        let (tx, rx) = mpsc::channel();
        let chat = Arc::new(crate::chat::Chat::default());
        let control = Control {
            doc,
            package: opened.package,
            host,
            events,
            hot: hot.clone(),
            render: render_tx.clone(),
            state: state.clone(),
            folder: dir,
            shared: self.shared.clone(),
            device: self.device.clone(),
            canvas,
            version: 1,
            audio: engine,
            routing,
            cameras: Vec::new(),
            retired: Vec::new(),
            live: None,
            phone: DeviceState::default(),
            chat: chat.clone(),
            chat_at: Instant::now(),
            away: false,
            resolving: false,
            handed: Vec::new(),
            take_back_at: None,
            dropped_at_start: 0,
            broadcasts: Vec::new(),
            me: tx.clone(),
            held_at: Instant::now(),
            alert_settings: Default::default(),
            alert_queue: Default::default(),
            alert_until: None,
            alerts_feed: 0,
            governor: crate::governor::Governor::new(Instant::now()),
            governor_message: None,
            health_at: Instant::now(),
            pending: None,
            hold: None,
            mic: None,
            media: HashMap::new(),
            screen: None,
            web: HashMap::new(),
            next_id: 1,
            inputs: Vec::new(),
            active_input: None,
            recording: None,
            meters: HashMap::new(),
            issues: HashMap::new(),
            stats_at: Instant::now(),
            meters_at: Instant::now(),
            frames_seen: 0,
            dropped_seen: 0,
            coalescing: None,
        };
        // a GPU error (out of memory, a lost device) doesn't panic the render thread: it's said
        let gpu_hot = Arc::downgrade(&hot);
        if let Ok(g) = lightair_render::shared_gpu() {
            g.device
                .on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                    if let Some(h) = gpu_hot.upgrade() {
                        h.fail(format!("The graphics stopped: {e}"));
                    }
                }));
        }
        let control_events = control.events.clone();
        let control_thread = std::thread::Builder::new()
            .name("lightair-studio".into())
            .spawn(move || {
                if let Err(e) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| control.run(rx)))
                {
                    let reason = format!("Studio stopped: {}", hot::panic_text(&e));
                    control_events.event(Event::Failed { reason });
                }
            })
            .map_err(|e| crate::project::ProjectError::Store(e.to_string()))?;
        let session = Arc::new(Session {
            handle,
            hot,
            tx,
            render: render_tx,
            state,
            view: Mutex::new(View {
                view: (1.0, 1.0),
                canvas: (1.0, 1.0),
            }),
            gesture: Mutex::new(None),
            threads: Mutex::new(vec![render_thread, mix_thread, control_thread]),
            chat,
            quit_mix,
            closed: AtomicBool::new(false),
            device: self.device.clone(),
            shared: self.shared.clone(),
            app_dir: self.app_dir(),
            remote: Mutex::new(None),
            remote_port: std::sync::atomic::AtomicU32::new(lightair_remote::SECURE_PORT as u32),
            remote_destinations: Mutex::new(Vec::new()),
            background_capture: AtomicBool::new(false),
            in_background: AtomicBool::new(false),
            image_sizes: Mutex::new(HashMap::new()),
            away_paused: AtomicBool::new(false),
            extension: Mutex::new(None),
        });
        let _ = session.tx.send(Msg::Refresh);
        Ok(session)
    }
}

impl Session {
    pub fn state(&self) -> Arc<State> {
        self.state.read().clone()
    }

    /// The program as a JPEG (a remote controller's preview): `canvas` a format other
    /// than the active one if an encoder draws it, at most `max_width` wide. (jpeg, width, height)
    pub fn program_jpeg(&self, max_width: u32, canvas: Option<Id>) -> Option<(Vec<u8>, u32, u32)> {
        let (tx, rx) = mpsc::channel();
        self.render
            .send(render::Msg::Snapshot { canvas, reply: tx })
            .ok()?;
        let (rgba, (w, h)) = rx.recv_timeout(Duration::from_millis(500)).ok()??;
        let img = image::RgbaImage::from_raw(w, h, rgba)?;
        let img = if w > max_width.max(16) {
            let nh = (h as u64 * max_width.max(16) as u64 / w.max(1) as u64).max(1) as u32;
            image::imageops::resize(
                &img,
                max_width.max(16),
                nh,
                image::imageops::FilterType::Triangle,
            )
        } else {
            img
        };
        let (w, h) = img.dimensions();
        let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70)
            .encode_image(&rgb)
            .ok()?;
        Some((out, w, h))
    }

    /// The program as a PNG at most `max_width` wide, lossless (for checking a look against
    /// the desktop's frames). (png, width, height)
    pub fn program_png(&self, max_width: u32) -> Option<(Vec<u8>, u32, u32)> {
        let (tx, rx) = mpsc::channel();
        self.render
            .send(render::Msg::Snapshot {
                canvas: None,
                reply: tx,
            })
            .ok()?;
        let (rgba, (w, h)) = rx.recv_timeout(Duration::from_millis(1000)).ok()??;
        let img = image::RgbaImage::from_raw(w, h, rgba)?;
        let img = if w > max_width.max(16) {
            let nh = (h as u64 * max_width.max(16) as u64 / w.max(1) as u64).max(1) as u32;
            image::imageops::resize(
                &img,
                max_width.max(16),
                nh,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            img
        };
        let (w, h) = img.dimensions();
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .ok()?;
        Some((out.into_inner(), w, h))
    }

    /// The active format's last frame read back whole (its own size, never scaled), with the
    /// feeds' frames it drew: the time each was pushed with. `None` before the first frame.
    pub fn program_frame(&self) -> Option<render::Captured> {
        let (tx, rx) = mpsc::channel();
        self.render.send(render::Msg::Capture { reply: tx }).ok()?;
        rx.recv_timeout(Duration::from_millis(1000)).ok()?
    }

    /// The face the looks see on a camera layer (the Looks screen's status chip and framing).
    pub fn face_status(&self, layer: Id) -> crate::looks::FaceStatus {
        use crate::looks::{FaceState, FaceStatus};
        let lighter = self.hot.governor_step.load(Ordering::Relaxed) >= crate::looks::LIGHTER_STEP;
        let stopped = FaceStatus {
            state: FaceState::Stopped,
            face_box: None,
            lighter,
        };
        let st = self.state();
        let Some(source) = st
            .scene()
            .and_then(|s| s.objects.iter().find(|o| o.id == layer))
            .and_then(|o| o.source)
        else {
            return stopped;
        };
        let Some(feed) = self.hot.program().feeds.get(&source).copied() else {
            return stopped;
        };
        let Some(f) = self.hot.feed(feed) else {
            return stopped;
        };
        let crate::feeds::Feed::Video(v) = &*f else {
            return stopped;
        };
        let Some(seen) = *v.looks_seen.lock() else {
            return stopped;
        };
        // the looks stopped drawing this camera (switched off, or the layer left the program)
        if crate::clock::host_now_ns() - seen.at_ns > 1_000_000_000 {
            return stopped;
        }
        let state = if !seen.analysed {
            FaceState::Warming
        } else if seen.present {
            FaceState::Found
        } else {
            FaceState::None
        };
        FaceStatus {
            state,
            face_box: seen.face_box.filter(|_| seen.present),
            lighter,
        }
    }

    /// A camera layer alone, cropped square around the face its looks see (the picture's middle
    /// without one), `size` pixels on a side: with its looks, without them, or with `beauty`
    /// settings merged over its own (the Looks screen's tiles). `None` without a picture.
    pub fn capture_layer_still(
        &self,
        layer: Id,
        size: u32,
        looks: bool,
        beauty: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Option<render::Still> {
        let (tx, rx) = mpsc::channel();
        self.render
            .send(render::Msg::Still {
                request: render::StillRequest {
                    layer,
                    size,
                    looks,
                    beauty,
                },
                reply: tx,
            })
            .ok()?;
        rx.recv_timeout(Duration::from_millis(1500)).ok()?
    }

    /// Draws `layer` without its looks on the preview alone (`on`), or as usual; the program,
    /// the streams and recordings keep the looks (the Looks screen's Compare, on air too).
    /// Turning it off for another layer than the bypassed one does nothing.
    pub fn set_preview_bypass_looks(&self, layer: Id, on: bool) {
        let mut b = self.hot.preview_bypass.lock();
        if on {
            *b = Some(layer);
        } else if *b == Some(layer) {
            *b = None;
        }
    }

    /// Which looks this phone can draw now, by its class and heat (the Looks screen's parts).
    pub fn look_support(&self) -> crate::looks::LookSupport {
        crate::looks::LookSupport::of(
            self.device.class(),
            self.hot.governor_step.load(Ordering::Relaxed),
        )
    }

    pub fn hot(&self) -> &Arc<Hot> {
        &self.hot
    }

    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
    }

    fn apply(&self, c: Command) {
        self.send(Msg::Apply(c));
    }

    /// Stops everything; returns once the threads have.
    /// Stops everything; returns once the threads have. A recording is finished first (its
    /// file and the index entry), for up to 5 s, while the host's pulls still reach the session.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.detach_extension();
        *self.remote.lock() = None;
        self.chat.set_channels(&[]);
        let (done, wait) = mpsc::channel();
        let start = Instant::now();
        self.send(Msg::Close(done));
        // the control thread's answer: up to 8 s while it's going round (it may be finishing a
        // recording), but not past 1 s of silence from it (stuck in a host call: it can't answer)
        while let Err(mpsc::RecvTimeoutError::Timeout) =
            wait.recv_timeout(Duration::from_millis(50))
        {
            let beat = self.hot.control_beat_ns.load(Ordering::Acquire);
            let silent = crate::clock::host_now_ns().saturating_sub(beat) > 1_000_000_000;
            if start.elapsed() >= Duration::from_secs(8) || silent {
                break;
            }
        }
        let _ = self.render.send(render::Msg::Quit);
        self.quit_mix.store(true, Ordering::Release);
        self.hot.wake_mixer();
        // joined if they end within 3 s of the close (1 s after a recording finished late); a
        // thread stuck in a host call (a camera or an encoder that never returns) is left to end
        // on its own, so closing never hangs the app
        let threads: Vec<JoinHandle<()>> = self.threads.lock().drain(..).collect();
        let until = (start + Duration::from_secs(3)).max(Instant::now() + Duration::from_secs(1));
        for t in threads {
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                tracing::warn!(
                    thread = t.thread().name().unwrap_or("?"),
                    "a Studio thread didn't end on close; left to end on its own"
                );
            }
        }
        hot::unregister(self.handle);
    }

    /// Foreground (`true`) or background. Straight to the render thread (whatever the control
    /// thread is doing): in the background it returns once the GPU has stopped (iOS ends an
    /// app that draws there). A recording is stopped too.
    ///
    /// While live, the program is first covered ("Be right back", or the project's
    /// privacy scene), each stream's encoder makes that a keyframe (up to a second for it), and
    /// the streams repeat it twice a second until the app is back; the sound goes on.
    pub fn lifecycle(&self, foreground: bool) {
        // a Compare held as the app left never sees its release: the preview comes back with
        // the looks on
        if !foreground {
            *self.hot.preview_bypass.lock() = None;
        }
        self.in_background.store(!foreground, Ordering::Release);
        if foreground {
            self.away_paused.store(false, Ordering::Release);
        }
        // iOS 27+ with the screen captured in the background: the program goes on as it is
        // (no cover, nothing paused, a recording carries on)
        if !foreground
            && self.background_capture.load(Ordering::Acquire)
            && self.state().screen.is_some()
        {
            return;
        }
        // with an extension attached (iOS 17–26's broadcast extension): it takes over while the
        // app is away (the reading stops, so does the heartbeat), and the app's own destinations
        // are handed to it (two senders can't share a stream key); taken back when the app returns
        let attached = self.extension.lock().as_ref().map(|a| a.paused());
        if let Some(paused) = attached {
            paused.store(!foreground, Ordering::Release);
            if foreground {
                let _ = self.render.send(render::Msg::Resume);
                self.send(Msg::TakeBack);
                self.send(Msg::Foreground);
                return;
            }
            self.send(Msg::HandOver);
            let (done, wait) = mpsc::channel();
            if self.render.send(render::Msg::Pause { done }).is_ok() {
                let _ = wait.recv_timeout(Duration::from_secs(2));
            }
            self.send(Msg::Background);
            return;
        }
        if foreground {
            let _ = self.render.send(render::Msg::Resume);
            let _ = self.render.send(render::Msg::Away(false));
            self.send(Msg::Foreground);
            return;
        }
        self.go_away();
    }

    /// The app is away and nothing keeps it drawing: while live the program is covered and each
    /// stream's encoder makes that a keyframe (up to a second for it), then drawing stops (iOS
    /// ends an app that draws in the background) and a recording is stopped.
    fn go_away(&self) {
        self.away_paused.store(true, Ordering::Release);
        let live: Vec<Arc<VideoEncoder>> = {
            let outputs = self.hot.outputs.read();
            outputs
                .iter()
                .filter_map(|o| self.hot.video_encoder(o.video_encoder()))
                .collect()
        };
        if !live.is_empty() && self.render.send(render::Msg::Away(true)).is_ok() {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(1)
                && !live.iter().all(|e| e.held.lock().packet.is_some())
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let (done, wait) = mpsc::channel();
        if self.render.send(render::Msg::Pause { done }).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        }
        self.send(Msg::Background);
    }

    // ── Preview ───────────────────────────────────────────────────────────────────────────

    pub fn attach_preview(&self, handle: u64, width: u32, height: u32, scale: f32) {
        self.set_view(width, height, scale);
        let _ = self.render.send(render::Msg::AttachPreview {
            handle,
            size: (width, height),
        });
    }

    pub fn resize_preview(&self, width: u32, height: u32, scale: f32) {
        self.set_view(width, height, scale);
        let _ = self.render.send(render::Msg::ResizePreview {
            size: (width, height),
        });
    }

    pub fn detach_preview(&self) {
        let (done, wait) = mpsc::channel();
        if self
            .render
            .send(render::Msg::DetachPreview { done })
            .is_ok()
        {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        }
    }

    fn set_view(&self, width: u32, height: u32, scale: f32) {
        let s = scale.max(0.1);
        self.view.lock().view = (width as f32 / s, height as f32 / s);
    }

    fn view(&self) -> View {
        let st = self.state();
        let mut v = *self.view.lock();
        if let Some(c) = st.canvas() {
            v.canvas = (c.width as f32, c.height as f32);
        }
        v
    }

    // ── Formats and scenes ────────────────────────────────────────────────────────────────

    pub fn set_format(&self, canvas: Id) {
        self.send(Msg::SetFormat(canvas));
    }

    pub fn add_format(&self, vertical: bool) {
        self.send(Msg::AddFormat(vertical));
    }

    /// A format of exactly `width` × `height` (the project's frame rate), every layer laid out
    /// in it as in the first format, scaled, and made active (not while recording); one of that
    /// size already there is the one used. Its id, or `None` for a size that can't be encoded
    /// (odd, under 16 or over 4096 on a side) or a session that's closing.
    pub fn add_sized_format(&self, width: u32, height: u32) -> Option<Id> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Msg::AddSizedFormat {
                width,
                height,
                reply: tx,
            })
            .ok()?;
        rx.recv_timeout(Duration::from_secs(5)).ok()?
    }

    pub fn switch_scene(&self, scene: Id) {
        self.send(Msg::SwitchScene(scene));
    }

    pub fn add_scene(&self, name: &str, copy_of: Option<Id>) {
        let st = self.state();
        let mut scene = match copy_of.and_then(|c| st.workspace.scene(c)) {
            Some(s) => {
                let mut s = (**s).clone();
                s.id = Id::new();
                // new object ids, parents moved along
                let ids: HashMap<Id, Id> = s.objects.iter().map(|o| (o.id, Id::new())).collect();
                for o in &mut s.objects {
                    o.id = ids[&o.id];
                    o.parent = o.parent.and_then(|p| ids.get(&p).copied());
                }
                s
            }
            None => Scene::new(""),
        };
        scene.name = if name.trim().is_empty() {
            format!("Scene {}", st.workspace.scenes.len() + 1)
        } else {
            name.trim().into()
        };
        self.apply(Command::AddScene { scene, index: None });
    }

    pub fn rename_scene(&self, scene: Id, name: &str) {
        self.apply(Command::RenameScene {
            scene,
            name: name.trim().into(),
        });
    }

    pub fn remove_scene(&self, scene: Id) {
        self.apply(Command::RemoveScene { scene });
    }

    pub fn move_scene(&self, scene: Id, to: usize) {
        self.apply(Command::MoveScene { scene, to });
    }

    pub fn set_transition(&self, scene: Id, kind: TransitionKind, duration_ms: u32) {
        let transition = TransitionSettings {
            kind,
            duration_ms,
            ..Default::default()
        };
        self.apply(Command::SetSceneTransition {
            scene,
            transition: Some(transition),
        });
    }

    // ── Layers ────────────────────────────────────────────────────────────────────────────

    pub fn add_layer(&self, layer: NewLayer) {
        self.send(Msg::AddLayer(layer));
    }

    fn object(&self, layer: Id) -> Option<(Arc<State>, Id, Object)> {
        let st = self.state();
        let scene = st.scene()?;
        let o = scene.object(layer)?.clone();
        let sid = scene.id;
        Some((st, sid, o))
    }

    pub fn remove_layer(&self, layer: Id) {
        if let Some((_, scene, _)) = self.object(layer) {
            self.apply(Command::RemoveObject {
                scene,
                object: layer,
            });
        }
    }

    pub fn duplicate_layer(&self, layer: Id) {
        if let Some((st, scene, mut o)) = self.object(layer) {
            let index = st
                .scene()
                .and_then(|s| s.object_index(layer))
                .map(|i| i + 1);
            o.id = Id::new();
            o.name = format!("{} copy", o.name);
            self.apply(Command::AddObject {
                scene,
                object: o,
                index,
            });
        }
    }

    pub fn set_layer_visible(&self, layer: Id, visible: bool) {
        if let Some((st, scene, _)) = self.object(layer) {
            let commands = st
                .workspace
                .canvases
                .iter()
                .map(|c| Command::SetObjectVisible {
                    scene,
                    object: layer,
                    canvas: c.id,
                    visible,
                })
                .collect();
            let label = if visible { "Show layer" } else { "Hide layer" };
            self.apply(Command::Batch {
                label: label.into(),
                commands,
            });
        }
    }

    pub fn set_layer_locked(&self, layer: Id, locked: bool) {
        if let Some((_, scene, _)) = self.object(layer) {
            self.apply(Command::SetObjectLocked {
                scene,
                object: layer,
                locked,
            });
        }
    }

    pub fn move_layer(&self, layer: Id, to: usize) {
        if let Some((_, scene, _)) = self.object(layer) {
            self.apply(Command::MoveObject {
                scene,
                object: layer,
                to,
            });
        }
    }

    pub fn rename_layer(&self, layer: Id, name: &str) {
        if let Some((_, scene, _)) = self.object(layer) {
            self.apply(Command::RenameObject {
                scene,
                object: layer,
                name: name.trim().into(),
            });
        }
    }

    pub fn set_layout(&self, layer: Id, p: &LayoutPatch) {
        let Some((st, scene, o)) = self.object(layer) else {
            return;
        };
        let Some(mut l) = o.layouts.get(&st.canvas).copied() else {
            return;
        };
        if let Some(v) = p.x {
            l.x = v;
        }
        if let Some(v) = p.y {
            l.y = v;
        }
        if let Some(v) = p.width {
            l.width = v.max(1.0);
        }
        if let Some(v) = p.height {
            l.height = v.max(1.0);
        }
        if let Some(v) = p.rotation {
            l.rotation = v.rem_euclid(360.0);
        }
        if let Some(v) = p.opacity {
            l.opacity = v.clamp(0.0, 1.0);
        }
        if let Some(r) = p.corner_radius {
            let mut shape = l.shape.unwrap_or(Shape::new(ModelShape::Rounded));
            if shape.kind == ModelShape::Rounded {
                shape.radius = r.clamp(0.0, 1.0);
            }
            l.shape = (r > 0.0 || shape.kind != ModelShape::Rounded || shape.border > 0.0)
                .then_some(shape);
        }
        let c = &mut l.crop;
        for (slot, v) in [&mut c.left, &mut c.top, &mut c.right, &mut c.bottom]
            .into_iter()
            .zip(p.crop)
        {
            if let Some(v) = v {
                *slot = v;
            }
        }
        if let Some(v) = p.flip_h {
            l.flip_h = v;
        }
        if let Some(v) = p.flip_v {
            l.flip_v = v;
        }
        self.apply(Command::SetLayout {
            scene,
            object: layer,
            canvas: st.canvas,
            layout: l,
        });
    }

    fn source_of(&self, layer: Id) -> Option<(Arc<State>, Arc<Source>)> {
        let (st, _, o) = self.object(layer)?;
        let s = st.workspace.sources.get(&o.source?)?.clone();
        Some((st, s))
    }

    fn set_settings(&self, layer: Id, patch: Value) {
        self.send(Msg::Edit(Box::new(move |ws| {
            let s = source_in(ws, layer)?;
            let mut settings = s.settings.clone();
            merge(&mut settings, patch);
            Some(Command::SetSourceSettings {
                source: s.id,
                settings,
            })
        })));
    }

    /// A text or timer's settings change, and the boxes showing it follow its drawn size.
    fn set_text_settings(&self, layer: Id, patch: Value) {
        self.send(Msg::Edit(Box::new(move |ws| {
            let s = source_in(ws, layer)?;
            let mut after = (*s).clone();
            merge(&mut after.settings, patch);
            let mut commands = follow_text(ws, &s, &after);
            commands.insert(
                0,
                Command::SetSourceSettings {
                    source: s.id,
                    settings: after.settings,
                },
            );
            Some(Command::Batch {
                label: "Change text".into(),
                commands,
            })
        })));
    }

    pub fn set_text(&self, layer: Id, t: &TextSpec) {
        self.set_text_settings(layer, text_settings(t));
    }

    pub fn set_timer(&self, layer: Id, t: &TimerSpec) {
        let mut s = text_settings(&t.text);
        merge(&mut s, timer_settings(t));
        self.set_text_settings(layer, s);
    }

    /// A widget's settings changed (JSON: only what differs from its defaults is kept).
    pub fn set_widget(&self, layer: Id, fields: Value) {
        self.send(Msg::Edit(Box::new(move |ws| {
            let s = source_in(ws, layer)?;
            let mut settings = s.settings.clone();
            let w = settings.get_mut("widget")?;
            let mut f = w.get("fields").cloned().unwrap_or_else(|| json!({}));
            merge(&mut f, fields);
            w["fields"] = f;
            Some(Command::SetSourceSettings {
                source: s.id,
                settings,
            })
        })));
    }

    /// Adds an effect of `kind` to a layer's source, on top (its defaults, then `settings`: a
    /// preset's).
    pub fn add_effect(&self, layer: Id, kind: &str, settings: Option<Value>) {
        let Some((_, s)) = self.source_of(layer) else {
            return;
        };
        let Some(def) = lightair_model::catalog::def(kind) else {
            return;
        };
        let mut v = Value::Object(def.defaults());
        if let Some(p) = settings {
            merge(&mut v, p);
        }
        self.apply(Command::AddSourceEffect {
            source: s.id,
            effect: Effect::new(def.name, kind, v),
            index: None,
        });
    }

    /// An effect's settings changed (only the keys given).
    pub fn set_effect(&self, layer: Id, effect: Id, settings: Value) {
        self.send(Msg::Edit(Box::new(move |ws| {
            let s = source_in(ws, layer)?;
            let mut v = s.effects.iter().find(|e| e.id == effect)?.settings.clone();
            merge(&mut v, settings);
            Some(Command::SetSourceEffectSettings {
                source: s.id,
                effect,
                settings: v,
            })
        })));
    }

    /// A file for an effect's file control (a LUT picked in Files): checked, copied into
    /// the project's assets and set on the effect. Returns the path it's kept at (relative to the
    /// project), or why it can't be used, in plain words.
    pub fn set_effect_file(
        &self,
        layer: Id,
        effect: Id,
        key: &str,
        path: &Path,
    ) -> Result<String, String> {
        use lightair_model::catalog::{self, Control};
        let (_, s) = self.source_of(layer).ok_or("That layer is gone.")?;
        let e = s
            .effects
            .iter()
            .find(|e| e.id == effect)
            .ok_or("That effect is gone.")?;
        let def = catalog::def(&e.kind).ok_or("That effect isn't known.")?;
        let extensions = def
            .controls
            .iter()
            .find_map(|c| match c {
                Control::File {
                    key: k, extensions, ..
                } if *k == key => Some(*extensions),
                _ => None,
            })
            .ok_or("That effect takes no file.")?;
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // phones draw LUTs from .cube files only (lightair-render's)
        let usable: Vec<&str> = extensions
            .iter()
            .copied()
            .filter(|x| e.kind != "clut_filter" || *x == "cube")
            .collect();
        if !usable.contains(&ext.as_str()) {
            return Err(format!("Pick a .{} file.", usable.join(" or .")));
        }
        if ext == "cube" {
            check_cube(path)?;
        }
        let folder = self.hot.program().folder.clone();
        let rel =
            copy_asset(&folder, path).ok_or("The file couldn't be copied into the project.")?;
        self.set_effect(layer, effect, json!({ key: rel }));
        Ok(rel)
    }

    /// A widget's button (a settings field of type "button": a subathon timer's Start, Pause,
    /// +5 minutes, Reset) was pressed.
    pub fn widget_button(&self, layer: Id, name: &str) {
        let Some((_, s)) = self.source_of(layer) else {
            return;
        };
        let Some(i) = crate::widgets::instance(&s.settings) else {
            return;
        };
        if let Some(server) = crate::web::server(&self.shared) {
            server.button(&i.id, name);
        }
    }

    pub fn set_effect_enabled(&self, layer: Id, effect: Id, enabled: bool) {
        if let Some((_, s)) = self.source_of(layer) {
            self.apply(Command::SetSourceEffectEnabled {
                source: s.id,
                effect,
                enabled,
            });
        }
    }

    pub fn remove_effect(&self, layer: Id, effect: Id) {
        if let Some((_, s)) = self.source_of(layer) {
            self.apply(Command::RemoveSourceEffect {
                source: s.id,
                effect,
            });
        }
    }

    /// Moves an effect to `to` in the layer's list (0: first applied); one undo step.
    pub fn move_effect(&self, layer: Id, effect: Id, to: usize) {
        self.send(Msg::Edit(Box::new(move |ws| {
            let s = source_in(ws, layer)?;
            let e = s.effects.iter().find(|e| e.id == effect)?.clone();
            let commands = vec![
                Command::RemoveSourceEffect {
                    source: s.id,
                    effect,
                },
                Command::AddSourceEffect {
                    source: s.id,
                    effect: e,
                    index: Some(to.min(s.effects.len() - 1)),
                },
            ];
            Some(Command::Batch {
                label: "Move effect".into(),
                commands,
            })
        })));
    }

    /// A colour layer's colour (0xAARRGGBB).
    pub fn set_colour(&self, layer: Id, argb: u32) {
        let colour = templates::colour_settings(argb, 0, 0)["color"].clone();
        self.set_settings(layer, json!({ "color": colour }));
    }

    pub fn set_camera(&self, layer: Id, facing: Facing) {
        self.set_settings(layer, json!({ "facing": facing_name(facing) }));
    }

    pub fn set_shape(&self, layer: Id, shape: &ShapeSpec) {
        let Some((st, scene, o)) = self.object(layer) else {
            return;
        };
        let Some(source) = o.source else { return };
        let Some(s) = st.workspace.sources.get(&source) else {
            return;
        };
        let mut settings = s.settings.clone();
        let (w, h) = (
            s.settings["width"].as_u64().unwrap_or(512) as u32,
            s.settings["height"].as_u64().unwrap_or(512) as u32,
        );
        merge(&mut settings, templates::colour_settings(shape.fill, w, h));
        merge(&mut settings, json!({ "shape": shape_name(shape.kind) }));
        let mut commands = vec![Command::SetSourceSettings { source, settings }];
        for (canvas, l) in &o.layouts {
            let mut l = *l;
            l.shape = shape_of(shape);
            commands.push(Command::SetLayout {
                scene,
                object: o.id,
                canvas: *canvas,
                layout: l,
            });
        }
        self.apply(Command::Batch {
            label: "Change shape".into(),
            commands,
        });
    }

    // ── Canvas editing ────────────────────────────────────────────────────────────────────

    pub fn hit_test(&self, x: f32, y: f32) -> Option<Id> {
        let st = self.state();
        gesture::hit(st.scene()?, st.canvas, &self.view(), x, y)
    }

    pub fn layer_frames(&self) -> Vec<LayerFrame> {
        let st = self.state();
        let view = self.view();
        let Some(scene) = st.scene() else {
            return vec![];
        };
        scene
            .objects
            .iter()
            .filter_map(|o| Some(view.frame(o.id, o.layouts.get(&st.canvas)?)))
            .collect()
    }

    pub fn begin_gesture(&self, layer: Id) {
        let Some((st, scene, o)) = self.object(layer) else {
            return;
        };
        let Some(start) = o.layouts.get(&st.canvas).copied() else {
            return;
        };
        let others = st
            .scene()
            .map(|s| gesture::others(s, st.canvas, layer))
            .unwrap_or_default();
        *self.gesture.lock() = Some(Gesture {
            scene,
            object: layer,
            canvas: st.canvas,
            start,
            now: start,
            others,
            last_snap: None,
            turning: false,
        });
    }

    /// The layer moves at once (drawn from the next frame); one undo step when it ends.
    pub fn update_gesture(&self, d: Delta) -> Option<Feedback> {
        let view = self.view();
        let mut g = self.gesture.lock();
        let g = g.as_mut()?;
        let (l, guides, snapped) = gesture::apply(&g.start, &g.others, &view, &d);
        g.now = l;
        g.turning |= d.rotation != 0.0;
        let new_snap = snapped.is_some() && snapped != g.last_snap;
        g.last_snap = snapped;
        // drawn now, before the document hears of it
        let p = self.hot.program();
        let mut ws = (*p.workspace).clone();
        if let Some(i) = ws.scene_index(g.scene) {
            let mut s = (*ws.scenes[i]).clone();
            if let Some(o) = s.objects.iter_mut().find(|o| o.id == g.object) {
                o.layouts.insert(g.canvas, l);
            }
            ws.scenes[i] = Arc::new(s);
        }
        let mut np = (*p).clone();
        np.workspace = Arc::new(ws);
        self.hot.set_program(np);
        Some(Feedback {
            frame: view.frame(g.object, &l),
            guides,
            snapped_rotation: new_snap,
            label: gesture::label(&l, g.turning),
        })
    }

    pub fn end_gesture(&self) {
        if let Some(g) = self.gesture.lock().take()
            && g.now != g.start
        {
            self.apply(Command::SetLayout {
                scene: g.scene,
                object: g.object,
                canvas: g.canvas,
                layout: g.now,
            });
        }
    }

    pub fn cancel_gesture(&self) {
        self.gesture.lock().take();
        self.send(Msg::Refresh);
    }

    pub fn undo(&self) {
        self.send(Msg::Undo);
    }

    pub fn redo(&self) {
        self.send(Msg::Redo);
    }

    // ── Audio ─────────────────────────────────────────────────────────────────────────────

    pub fn select_microphone(&self, input: &str) {
        self.send(Msg::SelectMic(input.into()));
    }

    pub fn set_track_gain(&self, track: Id, db: f32) {
        self.apply(Command::SetTrackVolume {
            track,
            volume_db: db.clamp(-100.0, 20.0),
        });
    }

    pub fn set_track_muted(&self, track: Id, muted: bool) {
        self.apply(Command::SetTrackMuted { track, muted });
    }

    /// Voice Clarity (`audio.voice`) or noise suppression (`audio.noise`) on, at `amount`, or off.
    pub fn set_track_effect(&self, track: Id, kind: &str, key: &str, amount: Option<f32>) {
        let st = self.state();
        let Some(t) = st.workspace.track(track) else {
            return;
        };
        let have = t.effects.iter().find(|e| e.kind == kind);
        let c = match (have, amount) {
            (None, None) => return,
            (Some(e), None) => Command::RemoveEffect {
                track,
                effect: e.id,
            },
            (Some(e), Some(a)) => {
                let mut settings = e.settings.clone();
                merge(&mut settings, json!({ key: a.clamp(0.0, 1.0) }));
                Command::SetEffectSettings {
                    track,
                    effect: e.id,
                    settings,
                }
            }
            (None, Some(a)) => {
                let name = if kind == "audio.voice" {
                    "Voice Clarity"
                } else {
                    "Noise suppression"
                };
                Command::AddEffect {
                    track,
                    effect: Effect::new(name, kind, json!({ key: a.clamp(0.0, 1.0) })),
                    index: None,
                }
            }
        };
        self.apply(c);
    }

    // ── Recording, privacy ────────────────────────────────────────────────────────────────

    pub fn start_recording(&self, camera_clean: bool) {
        self.send(Msg::StartRecording { camera_clean });
    }

    pub fn stop_recording(&self) {
        self.send(Msg::StopRecording);
    }

    /// Live on `destinations` (the Test destination streams nowhere), recording
    /// too if asked. How it goes comes as `Event::Live` and `Event::Health`.
    pub fn go_live(&self, destinations: Vec<crate::outputs::Destination>, record: bool) {
        self.send(Msg::GoLive(
            destinations,
            Start {
                record,
                ordered: false,
                adding: false,
            },
        ));
    }

    pub fn end_live(&self) {
        self.send(Msg::EndLive);
    }

    /// Live on `destinations` one at a time, in order, each on air before the next starts; one
    /// that doesn't start waits for `skip_destination` (continue without it) or
    /// `end_live` (stop all).
    pub fn go_live_ordered(&self, destinations: Vec<crate::outputs::Destination>, record: bool) {
        self.send(Msg::GoLive(
            destinations,
            Start {
                record,
                ordered: true,
                adding: false,
            },
        ));
    }

    /// Another destination on the stream that's on: its encode shared if its settings
    /// match one that's on, else opened (if the phone can run one more).
    pub fn add_destination(&self, destination: crate::outputs::Destination) {
        self.send(Msg::GoLive(
            vec![destination],
            Start {
                record: false,
                ordered: false,
                adding: true,
            },
        ));
    }

    /// Stops one destination while the rest go on; `start_destination` starts it again.
    pub fn stop_destination(&self, id: &str) {
        self.send(Msg::StopDestination(id.into()));
    }

    pub fn start_destination(&self, id: &str) {
        self.send(Msg::StartDestination(id.into()));
    }

    /// The destinations' order while live: the health list's and the start queue's.
    /// Ids not given keep their places after the given ones.
    pub fn reorder_destinations(&self, ids: Vec<String>) {
        self.send(Msg::Reorder(ids));
    }

    /// New settings for a destination that's on ("Match settings", or its frame rate):
    /// it moves to the encode its settings now share, or a new one; the rest go on.
    pub fn update_destination(&self, destination: crate::outputs::Destination) {
        self.send(Msg::Update(destination));
    }

    /// Ordered start: go on without the destination that didn't start.
    pub fn skip_destination(&self, id: &str) {
        self.send(Msg::SkipDestination(id.into()));
    }

    /// Remote control from a paired desktop or another phone: on (the server, advertised
    /// as `name`) or off. A pairing link for a controller's QR code when on; `addresses` are the
    /// phone's own on the network.
    pub fn remote_control(
        self: &Arc<Self>,
        on: bool,
        name: &str,
        addresses: &[String],
    ) -> Result<Option<String>, String> {
        let mut r = self.remote.lock();
        if !on {
            *r = None;
            return Ok(None);
        }
        if r.is_none() {
            let port = self.remote_port.load(Ordering::Relaxed) as u16;
            *r = Some(crate::remote::Remote::start(
                self,
                &self.app_dir,
                name,
                port,
            )?);
        }
        Ok(r.as_ref().map(|r| r.offer(addresses)))
    }

    pub fn remote_status(&self) -> crate::remote::RemoteStatus {
        self.remote
            .lock()
            .as_ref()
            .map_or_else(Default::default, |r| r.status())
    }

    /// The person's answer to a device asking to pair (`remote_status().pending`).
    pub fn remote_answer(&self, accept: bool) {
        if let Some(r) = self.remote.lock().as_ref() {
            r.answer(accept);
        }
    }

    pub fn remote_forget(&self, name: &str) {
        if let Some(r) = self.remote.lock().as_ref() {
            r.forget(name);
        }
    }

    /// What a controller's `stream.start` goes live on: the app's destinations as they are now.
    pub fn set_remote_destinations(&self, destinations: Vec<crate::outputs::Destination>) {
        *self.remote_destinations.lock() = destinations;
    }

    pub fn remote_destinations(&self) -> Vec<crate::outputs::Destination> {
        self.remote_destinations.lock().clone()
    }

    /// iOS 27+: whether the app keeps capturing the screen in the background
    /// (`UIBackgroundModes` `screen-capture`, an `SCStream` running). While on and the project
    /// has a screen layer, `lifecycle(Background)` leaves everything running: the program keeps
    /// being drawn and encoded, with its live overlays. The host rebuilds a VideoToolbox session
    /// the system invalidated (-12903) under the same encoder id.
    ///
    /// The capture can start or end while the app is already away: the picker's "entire screen"
    /// sends the app to the background before the stream has started, so the program was covered
    /// ("Be right back") and its drawing paused, and must now go on; and sharing stopped from the
    /// system's control while away leaves nothing to keep drawing for, so it's covered then.
    pub fn set_background_capture(&self, on: bool) {
        let was = self.background_capture.swap(on, Ordering::AcqRel);
        if was == on || !self.in_background.load(Ordering::Acquire) {
            return;
        }
        if on {
            if self.state().screen.is_some() && self.away_paused.swap(false, Ordering::AcqRel) {
                let _ = self.render.send(render::Msg::Resume);
                let _ = self.render.send(render::Msg::Away(false));
                self.send(Msg::Foreground);
            }
        } else if !self.away_paused.load(Ordering::Acquire) && self.extension.lock().is_none() {
            self.go_away();
        }
    }

    /// Attaches what carries the show while the app is away (iOS 17–26's broadcast extension,
    /// which the app builds), from now until `detach_extension` (or close).
    pub fn attach_extension(&self, extension: Box<dyn AwayExtension>) {
        *self.extension.lock() = Some(extension);
    }

    pub fn detach_extension(&self) {
        let a = self.extension.lock().take();
        drop(a);
    }

    /// The session's state as its threads share it (for an extension reading the show).
    pub fn shared_state(&self) -> Arc<RwLock<Arc<State>>> {
        self.state.clone()
    }

    /// The device's offset from UTC, in minutes.
    pub fn utc_offset_min(&self) -> i32 {
        self.device.utc_offset_min
    }

    /// A clip of the newest `seconds` (at most 60) of the stream: from the
    /// biggest encode's last minute, from a keyframe, with its sound, into an MP4 in the
    /// recordings' `Clips` folder. Its path. Only while live; takes a moment (call it off the
    /// main thread).
    pub fn save_clip(&self, seconds: u32) -> Result<PathBuf, String> {
        let outputs = self.hot.outputs.read().clone();
        let pick = outputs
            .iter()
            .filter_map(|o| Some((o, self.hot.video_encoder(o.video_encoder())?)))
            .max_by_key(|(_, e)| e.config.width * e.config.height)
            .ok_or("Clips are made while live.")?;
        let (o, e) = pick;
        let audio = self
            .hot
            .audio_encoder(o.audio_encoder)
            .map(|a| a.clip.snapshot())
            .unwrap_or_default();
        let sets = e.sent.lock().parameter_sets.clone();
        let st = self.state();
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let dir = crate::clips::folder(&self.shared);
        let named = crate::record::file_name(
            &dir,
            &format!("{} clip", st.workspace.name),
            unix,
            self.device.utc_offset_min,
        );
        crate::clips::write(
            &named,
            seconds.clamp(1, 60),
            &e.clip.snapshot(),
            &sets,
            (e.config.width, e.config.height),
            &audio,
        )?;
        Ok(named)
    }

    /// The screen's feeds while the project has a screen layer: the host pushes its
    /// frames (`push_pixel_buffer`, `push_rgba`) and sound (`push_audio`) into them, and says
    /// when sharing stops (`feed_state(video, Stopped)`).
    /// A source's own picture size in pixels, upright as it's drawn (what a layer's crop counts
    /// in): a camera's, screen's, video's or web layer's newest frame, an image's file; (0, 0)
    /// for one with no picture of its own (text, colour, shapes) or no frame yet.
    pub fn source_size(&self, source: Id) -> (u32, u32) {
        let program = self.hot.program();
        if let Some(feed) = program.feeds.get(&source)
            && let Some(f) = self.hot.feed(*feed)
            && let crate::feeds::Feed::Video(v) = &*f
        {
            return v.at(i64::MAX).map_or((0, 0), |(p, _, _)| p.upright_size());
        }
        let st = self.state();
        let Some(src) = st.workspace.sources.get(&source) else {
            return (0, 0);
        };
        if src.kind != templates::IMAGE {
            return (0, 0);
        }
        let Some(file) = src.settings["file"].as_str() else {
            return (0, 0);
        };
        let path = program.folder.join(file);
        let mut sizes = self.image_sizes.lock();
        if let Some(s) = sizes.get(&path) {
            return *s;
        }
        let size = image::image_dimensions(&path).unwrap_or((0, 0));
        if sizes.len() >= 64 {
            sizes.clear();
        }
        sizes.insert(path, size);
        size
    }

    pub fn screen_feeds(&self) -> Option<ScreenFeeds> {
        self.state().screen
    }

    /// What going live on `destinations` would take: the encodes, which destinations
    /// share each, and whether this phone can run them (beside a recording that's on, or
    /// `record`). For the Go live review.
    pub fn stream_plan(
        &self,
        destinations: &[crate::outputs::Destination],
        record: bool,
    ) -> crate::outputs::StreamPlan {
        let st = self.state();
        let formats = formats(&st.workspace, st.canvas);
        let recording = if st.recording == RecordingState::Idle {
            record as u32
        } else if st.live == LiveState::Off {
            // what's open is the recording's
            self.hot.video_encoders.read().len().max(1) as u32
        } else {
            1
        };
        crate::outputs::plan_in(
            destinations,
            &formats,
            self.device.video_encodes(),
            recording,
            self.device.formats_at_once(),
        )
    }

    /// The alert rules.
    pub fn alert_settings(&self) -> crate::alerts::Settings {
        crate::alerts::Settings::load(&self.state().folder)
    }

    pub fn set_alert_settings(&self, s: crate::alerts::Settings) {
        self.send(Msg::AlertSettings(s));
    }

    /// Shows a made-up alert of `kind` ("follow", "sub", "raid", "cheer", "paid", "redeem").
    pub fn test_alert(&self, kind: &str) {
        self.send(Msg::TestAlert(kind.into()));
    }

    /// The chat hub (tests put events in it).
    pub fn chat(&self) -> &Arc<crate::chat::Chat> {
        &self.chat
    }

    /// The chat channels read now, with how each is doing.
    pub fn chat_channels(&self) -> Vec<(lightair_live::Channel, lightair_live::Status)> {
        self.chat.channels()
    }

    /// The chat channels to read (empty: none).
    pub fn set_chat_channels(&self, channels: Vec<lightair_live::Channel>) {
        self.chat.set_channels(&channels);
    }

    /// Sends `text` to every channel read (or `platform`'s only).
    pub fn send_chat(&self, text: String, platform: Option<String>) {
        let tx = self.tx.clone();
        crate::chat::send(&self.chat, text, platform, move |reason| {
            let _ = tx.send(Msg::Said(reason));
        });
    }

    /// The phone's heat, battery and network, from the host.
    pub fn device_state(&self, state: DeviceState) {
        self.send(Msg::Device(state));
    }

    /// From the program sample being heard now: the next frame drawn and the sound mixed for
    /// that time on (however late a Bluetooth microphone's sound arrives) are both covered.
    pub fn set_privacy_cut(&self, on: bool) {
        let now = self.hot.clock.sample_at(crate::clock::host_now_ns());
        if self.hot.privacy().0 != on {
            self.hot.set_privacy(on, now);
        }
        self.send(Msg::Privacy);
    }

    // ── From the host ─────────────────────────────────────────────────────────────────────

    pub fn feed_state(&self, feed: u32, state: FeedState) {
        self.send(Msg::FeedState(feed, state));
    }

    pub fn audio_inputs_changed(&self, inputs: Vec<AudioInput>, active: Option<String>) {
        self.send(Msg::Inputs(inputs, active));
    }

    pub fn recording_finished(&self, recording: u32, result: RecordingResult) {
        self.send(Msg::RecordingFinished(recording, result));
    }

    pub fn encoder_failed(&self, encoder: u32, reason: String) {
        self.send(Msg::EncoderFailed(encoder, reason));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// `layer`'s source in the program scene of `ws`.
fn source_in(ws: &Workspace, layer: Id) -> Option<Arc<Source>> {
    let scene = ws
        .program
        .and_then(|p| ws.scene(p))
        .or_else(|| ws.scenes.first())?;
    ws.sources.get(&scene.object(layer)?.source?).cloned()
}

#[cfg(test)]
#[test]
fn a_destinations_height_scales_the_stream() {
    assert_eq!(stream_size((1920, 1080), Some(720)), (1280, 720));
    assert_eq!(stream_size((1080, 1920), Some(720)), (720, 1280));
    assert_eq!(stream_size((1080, 1920), Some(1280)), (720, 1280));
    assert_eq!(stream_size((1080, 1920), Some(1080)), (1080, 1920));
    assert_eq!(stream_size((1080, 1920), Some(2160)), (1080, 1920));
    assert_eq!(stream_size((1080, 1920), None), (1080, 1920));
}

/// Ends YouTube broadcasts made for a stream (on a helper thread: network calls).
fn end_broadcasts(ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("lightair-endlive".into())
        .spawn(move || {
            for id in ids {
                if let Err(e) = lightair_live::official::youtube_end_broadcast(&id) {
                    tracing::warn!("the YouTube broadcast didn't end: {e}");
                }
            }
        });
}

/// The size a text or timer source draws at (its picture's pixels; a timer at zero).
pub(crate) fn drawn_size(s: &Source) -> Option<(f32, f32)> {
    let text = match s.kind.as_str() {
        templates::TEXT => s.settings["text"].as_str().unwrap_or("").to_string(),
        templates::TIMER => crate::pictures::timer_text(&s.settings, 0, 0),
        _ => return None,
    };
    let style = crate::pictures::text_style(&s.settings, text);
    let (w, h) = lightair_render::generate::text(&style).size;
    Some((w.max(1) as f32, h.max(1) as f32))
}

/// Every box showing `before` resized to `after`'s drawn size, so its text never
/// stretches: each keeps its scale, its top, and the side its text is aligned to.
fn follow_text(ws: &Workspace, before: &Source, after: &Source) -> Vec<Command> {
    let (Some((bw, bh)), Some((aw, ah))) = (drawn_size(before), drawn_size(after)) else {
        return Vec::new();
    };
    if (bw, bh) == (aw, ah) {
        return Vec::new();
    }
    let align = crate::pictures::text_style(&after.settings, String::new()).align;
    let mut commands = Vec::new();
    for scene in &ws.scenes {
        for o in scene.objects.iter().filter(|o| o.source == Some(before.id)) {
            for (canvas, l) in &o.layouts {
                let mut l = *l;
                let w = l.width / bw * aw;
                l.x += match align {
                    0 => 0.0,
                    2 => l.width - w,
                    _ => (l.width - w) / 2.0,
                };
                l.width = w.max(1.0);
                l.height = (l.height / bh * ah).max(1.0);
                commands.push(Command::SetLayout {
                    scene: scene.id,
                    object: o.id,
                    canvas: *canvas,
                    layout: l,
                });
            }
        }
    }
    commands
}

fn merge(into: &mut Value, patch: Value) {
    match (into, patch) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                a.insert(k, v);
            }
        }
        (a, b) => *a = b,
    }
}

fn facing_name(f: Facing) -> &'static str {
    match f {
        Facing::Front => "front",
        Facing::Back => "back",
    }
}

// ── A layer's settings read back, for the Inspector ──────────────────────────────────────────

/// libobs's 0xAABBGGRR as 0xAARRGGBB.
fn argb_of(abgr: u32) -> u32 {
    (abgr & 0xFF00_FF00) | ((abgr & 0xFF) << 16) | ((abgr >> 16) & 0xFF)
}

/// A text (or timer) source's style as the interface edits it.
pub fn text_spec_of(s: &Source) -> TextSpec {
    let t = crate::pictures::text_style(
        &s.settings,
        s.settings["text"].as_str().unwrap_or("").to_string(),
    );
    TextSpec {
        text: t.text,
        font_family: t.font.map(|f| f.family),
        size: t.size,
        color: t.color,
        bold: t.bold,
        align: t.align,
        outline: t.outline,
        background: t.background.map(|(rgb, _)| rgb),
    }
}

pub fn timer_spec_of(s: &Source) -> TimerSpec {
    let mode = match s.settings["mode"].as_str() {
        Some("stopwatch") => TimerMode::CountUp,
        Some("clock") => TimerMode::Clock,
        _ => TimerMode::CountDown,
    };
    TimerSpec {
        mode,
        seconds: s.settings["seconds"].as_u64().unwrap_or(300) as u32,
        text: text_spec_of(s),
    }
}

/// A colour source's colour (0xAARRGGBB).
pub fn colour_of(s: &Source) -> u32 {
    argb_of(s.settings["color"].as_u64().unwrap_or(0xFFFF_FFFF) as u32)
}

/// A Studio shape (a colour source that says its shape) with its layout's outline.
pub fn shape_spec_of(s: &Source, layout: Option<&Layout>) -> Option<ShapeSpec> {
    let kind = match s.settings["shape"].as_str()? {
        "ellipse" => ShapeKind::Ellipse,
        "line" => ShapeKind::Line,
        _ => ShapeKind::Rectangle,
    };
    let border = layout.and_then(|l| l.shape).filter(|b| b.border > 0.0);
    Some(ShapeSpec {
        kind,
        fill: colour_of(s),
        stroke: border.map(|b| argb_of(b.border_color)),
        stroke_width: border.map_or(0.0, |b| b.border),
    })
}

/// The project's formats for a stream plan: the active one first.
fn formats(ws: &Workspace, active: Id) -> Vec<crate::outputs::Format> {
    let of = |c: &Canvas| crate::outputs::Format {
        id: c.id.to_string(),
        size: (c.width, c.height),
        fps: c.fps_num / c.fps_den.max(1),
    };
    let mut out: Vec<_> = ws.canvas(active).map(of).into_iter().collect();
    out.extend(ws.canvases.iter().filter(|c| c.id != active).map(of));
    if out.is_empty() {
        out.push(crate::outputs::Format {
            id: String::new(),
            size: (1080, 1920),
            fps: 30,
        });
    }
    out
}

fn format_id(s: &str) -> Option<Id> {
    uuid::Uuid::parse_str(s).ok().map(Id)
}

/// Which camera a camera source shows: its facing, and its external device (`device` in
/// its settings, as the desktop's camera source keeps it), if any.
pub fn device_of(s: &Source) -> Option<String> {
    s.settings["device"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(String::from)
}

/// A camera's identity for comparing which is running: its facing and its device's hash.
type CamKey = (Facing, u64);

fn cam_key(s: &Source) -> CamKey {
    use std::hash::{Hash, Hasher};
    let device = device_of(s).map_or(0, |d| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        d.hash(&mut h);
        h.finish() | 1
    });
    (facing_of(s), device)
}

pub fn facing_of(s: &Source) -> Facing {
    if s.settings["facing"].as_str() == Some("front") {
        Facing::Front
    } else {
        Facing::Back
    }
}

fn text_settings(t: &TextSpec) -> Value {
    templates::text_settings(
        &t.text,
        t.font_family.as_deref(),
        t.size,
        t.color,
        t.bold,
        t.align,
        t.outline,
        t.background,
    )
}

fn timer_settings(t: &TimerSpec) -> Value {
    let mode = match t.mode {
        TimerMode::CountUp => "stopwatch",
        TimerMode::CountDown => "countdown",
        TimerMode::Clock => "clock",
    };
    json!({ "mode": mode, "seconds": t.seconds, "autostart": true })
}

fn shape_name(k: ShapeKind) -> &'static str {
    match k {
        ShapeKind::Rectangle => "rectangle",
        ShapeKind::Ellipse => "ellipse",
        ShapeKind::Line => "line",
    }
}

fn shape_of(s: &ShapeSpec) -> Option<Shape> {
    let kind = match s.kind {
        ShapeKind::Ellipse => ModelShape::Circle,
        _ => ModelShape::Rounded,
    };
    let mut shape = Shape::new(kind);
    if kind == ModelShape::Rounded {
        shape.radius = 0.0;
    }
    if let Some(c) = s.stroke {
        shape.border = s.stroke_width.max(0.0);
        let (a, rgb) = (c >> 24, c & 0xFF_FFFF);
        shape.border_color =
            (a << 24) | ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF);
    }
    (s.kind == ShapeKind::Ellipse || s.stroke.is_some()).then_some(shape)
}

// ── The control thread ─────────────────────────────────────────────────────────────────────

struct Live {
    /// One encode per distinct setting, each with its own bitrate stepping.
    groups: Vec<Group>,
    /// One sound encode per distinct codec and bitrate.
    audios: Vec<((AudioCodec, u32), u32)>,
    since_ms: u64,
    on_air: bool,
    /// Ordered start: destinations still to start, in order; the one starting (since
    /// when); the one that didn't, waiting for "continue without it" or "stop all".
    queue: std::collections::VecDeque<crate::outputs::Destination>,
    starting: Option<(String, Instant)>,
    blocked: Option<String>,
    /// Stopped while live, to start again.
    stopped: Vec<crate::outputs::Destination>,
}

/// How a go-live (or an addition while live) starts.
#[derive(Debug, Clone, Copy)]
struct Start {
    record: bool,
    /// One at a time, each on air before the next.
    ordered: bool,
    /// Added to a stream that's on.
    adding: bool,
}

struct Group {
    video: u32,
    /// Its format (a canvas).
    format: Id,
    chosen_kbps: u32,
    destinations: Vec<String>,
    /// The encoder's settings now, and the chosen size (rung 0).
    config: VideoEncoderConfig,
    chosen: (u32, u32),
    /// The resolution floor's rung: 0 the chosen size, then 720p, 540p below it.
    rung: usize,
    changed_at: Instant,
}

impl Group {
    /// The short sides it may stream at: the chosen one, then 720 and 540 below it.
    fn rungs(&self) -> Vec<u32> {
        let short = self.chosen.0.min(self.chosen.1);
        std::iter::once(short)
            .chain([720, 540].into_iter().filter(|r| *r < short))
            .collect()
    }

    /// Rung `rung`'s picture for its destinations' bitrate floor: its share of the chosen
    /// picture's pixels, and whether it's the smallest.
    fn size_at(&self, rung: usize) -> (f32, bool) {
        let rungs = self.rungs();
        let short = self.chosen.0.min(self.chosen.1).max(1) as f32;
        let r = rungs.get(rung).copied().unwrap_or(short as u32) as f32 / short;
        (r * r, rung + 1 >= rungs.len())
    }
}

/// A switch waiting for the new camera's first frame.
struct Pending {
    from: Id,
    settings: TransitionSettings,
    since: Instant,
    feed: u32,
}

/// One file being recorded: the program's, or the camera clean's.
struct Part {
    id: u32,
    video: u32,
    audio: Option<u32>,
    file: PathBuf,
    done: bool,
}

struct Rec {
    /// The program first; the camera clean after it when asked for.
    parts: Vec<Part>,
    started: SystemTime,
    size: (u32, u32),
}

struct Control {
    doc: Document,
    package: lightair_store::Package,
    host: Arc<dyn Host>,
    events: Arc<dyn Events>,
    hot: Arc<Hot>,
    render: Sender<render::Msg>,
    state: Arc<RwLock<Arc<State>>>,
    folder: PathBuf,
    shared: PathBuf,
    device: crate::device::DeviceInfo,
    canvas: Id,
    version: u64,
    audio: la::Engine,
    routing: Arc<Mutex<Routing>>,
    /// The running cameras (two at most): (which, feed).
    cameras: Vec<(CamKey, u32)>,
    /// The cameras stopped by a switch: their feeds' last frames are kept for the scene going
    /// out until the switch is over.
    retired: Vec<(CamKey, u32)>,
    /// Live: the stream's encoders (shared by its destinations) and when it started.
    live: Option<Live>,
    phone: DeviceState,
    chat: Arc<crate::chat::Chat>,
    chat_at: Instant,
    /// In the background while live, and when the held picture was last sent.
    away: bool,
    /// Platform destinations' servers and keys are being found (going live).
    resolving: bool,
    /// iOS 17–26: destinations handed to the broadcast extension while the app is away, and
    /// when to take them back.
    handed: Vec<crate::outputs::Destination>,
    take_back_at: Option<Instant>,
    /// The render's dropped frames when the stream started (its drops count from there).
    dropped_at_start: u64,
    /// YouTube broadcasts made for this stream through sign-in, ended with it.
    broadcasts: Vec<String>,
    /// The control thread's own inbox (for work done on helper threads).
    me: Sender<Msg>,
    held_at: Instant,
    alert_settings: crate::alerts::Settings,
    alert_queue: AlertQueue,
    alert_until: Option<Instant>,
    alerts_feed: u32,
    governor: crate::governor::Governor,
    governor_message: Option<String>,
    health_at: Instant,
    /// A switch waiting for the new scene's camera's first frame (1.5 s at most).
    pending: Option<Pending>,
    /// The scene still drawn while a switch waits.
    hold: Option<Id>,
    mic: Option<u32>,
    /// Media sources' feeds: source → (video feed, sound feed).
    media: HashMap<Id, (u32, Option<u32>)>,
    /// The screen's feeds while the project has a screen layer.
    screen: Option<ScreenFeeds>,
    /// The program's web layers' feeds.
    web: HashMap<Id, crate::web::WebFeed>,
    next_id: u32,
    inputs: Vec<AudioInput>,
    active_input: Option<String>,
    recording: Option<Rec>,
    meters: HashMap<Id, Arc<la::meter::Meter>>,
    issues: HashMap<String, String>,
    stats_at: Instant,
    meters_at: Instant,
    frames_seen: u64,
    dropped_seen: u64,
    /// A run of the same kind of change to the same thing (a slider or a crop edge dragged:
    /// one change a frame) is one undo step: its key and when the last came.
    coalescing: Option<((&'static str, Id, Id), Instant)>,
}

/// Changes of the same kind to the same thing this close together are one undo step.
const COALESCE: Duration = Duration::from_millis(400);

impl Control {
    fn run(mut self, rx: Receiver<Msg>) {
        self.alert_settings = crate::alerts::Settings::load(&self.folder);
        self.inputs = self.host.audio_inputs();
        loop {
            self.beat();
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Close(done)) => {
                    self.close();
                    self.finish_recording(&rx, Duration::from_secs(5));
                    let _ = done.send(());
                    return;
                }
                Ok(m) => self.handle(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.close();
                    return;
                }
            }
            self.tick();
        }
    }

    /// Waits (up to `limit`) for the host to finish a stopped recording, so its file is in the
    /// index before the session goes.
    fn finish_recording(&mut self, rx: &Receiver<Msg>, limit: Duration) {
        let until = Instant::now() + limit;
        while self.recording.is_some() {
            self.beat();
            let left = until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(Msg::RecordingFinished(id, r)) => self.recording_finished(id, r),
                Ok(Msg::EncoderFailed(..)) | Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = self.package.flush();
    }

    /// Says the control thread is going round (`Hot::control_beat_ns`).
    fn beat(&self) {
        self.hot
            .control_beat_ns
            .store(crate::clock::host_now_ns(), Ordering::Release);
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn event(&self, e: Event) {
        self.events.event(e);
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Apply(c) => self.apply(c),
            Msg::Undo => {
                self.end_coalescing();
                if self.doc.undo().is_some() {
                    let _ = self.package.compact(self.doc.workspace());
                    self.refresh();
                }
            }
            Msg::Redo => {
                self.end_coalescing();
                if self.doc.redo().is_some() {
                    let _ = self.package.compact(self.doc.workspace());
                    self.refresh();
                }
            }
            Msg::Refresh => self.refresh(),
            Msg::SwitchScene(scene) => self.switch(scene),
            Msg::SetFormat(c) => {
                if self.doc.workspace().canvas(c).is_some() && self.recording.is_none() {
                    self.canvas = c;
                    self.refresh();
                } else if self.recording.is_some() {
                    self.event(Event::Failed {
                        reason: "The format can't change while recording.".into(),
                    });
                }
            }
            Msg::AddFormat(vertical) => self.add_format(vertical),
            Msg::AddSizedFormat {
                width,
                height,
                reply,
            } => {
                let _ = reply.send(self.add_sized_format(width, height));
            }
            Msg::AddLayer(l) => self.add_layer(l),
            Msg::Privacy => self.publish(),
            Msg::StartRecording { camera_clean } => self.start_recording(camera_clean),
            Msg::StopRecording => self.stop_recording(None),
            Msg::SelectMic(id) => {
                self.active_input = Some(id.clone());
                if let Some(f) = self.mic.take() {
                    self.host.stop_microphone(f);
                    self.hot.feeds.write().remove(&f);
                }
                self.refresh();
            }
            Msg::FeedState(feed, s) => self.feed_state(feed, s),
            Msg::Inputs(inputs, active) => {
                self.inputs = inputs;
                if active.is_some() {
                    self.active_input = active;
                }
                self.publish();
            }
            Msg::RecordingFinished(id, r) => self.recording_finished(id, r),
            Msg::EncoderFailed(e, reason) => {
                if self
                    .recording
                    .as_ref()
                    .is_some_and(|r| r.parts.iter().any(|p| p.video == e || p.audio == Some(e)))
                {
                    self.stop_recording(Some(format!("Recording stopped: {reason}")));
                }
            }
            Msg::GoLive(dests, start) => self.begin_live(dests, start),
            Msg::HandOver => {
                let ids: Vec<String> = self
                    .hot
                    .outputs
                    .read()
                    .iter()
                    .map(|o| o.dest.id.clone())
                    .collect();
                for id in ids {
                    if let Some(d) = self.stop_one(&id) {
                        self.handed.push(d);
                    }
                }
                self.take_back_at = None;
            }
            Msg::TakeBack => {
                // after the extension has let go (it hears the app within a second)
                if !self.handed.is_empty() {
                    self.take_back_at = Some(Instant::now() + Duration::from_millis(1500));
                }
            }
            Msg::StopDestination(id) => {
                if let Some(d) = self.stop_one(&id)
                    && let Some(l) = self.live.as_mut()
                {
                    l.stopped.push(d);
                }
            }
            Msg::Reorder(ids) => {
                let at = |id: &str| ids.iter().position(|i| i == id).unwrap_or(usize::MAX);
                self.hot.outputs.write().sort_by_key(|o| at(&o.dest.id));
                if let Some(l) = self.live.as_mut() {
                    l.queue.make_contiguous().sort_by_key(|d| at(&d.id));
                    l.stopped.sort_by_key(|d| at(&d.id));
                }
            }
            Msg::Update(d) => {
                let id = d.id.clone();
                let place = self.hot.outputs.read().iter().position(|o| o.dest.id == id);
                if let Some(place) = place
                    && self.stop_one(&id).is_some()
                {
                    let name = d.name.clone();
                    match self.start_one(d) {
                        Ok(()) => {
                            // back in its place
                            let mut outputs = self.hot.outputs.write();
                            if let Some(o) = outputs.pop() {
                                let at = place.min(outputs.len());
                                outputs.insert(at, o);
                            }
                        }
                        Err(e) => self.issue(&format!("output-{id}"), format!("{name}: {e}")),
                    }
                } else if let Some(l) = self.live.as_mut() {
                    // one waiting or stopped: its settings for when it starts
                    for q in l.queue.iter_mut().chain(l.stopped.iter_mut()) {
                        if q.id == id {
                            *q = d.clone();
                        }
                    }
                }
            }
            Msg::StartDestination(id) => {
                let d = self.live.as_mut().and_then(|l| {
                    let i = l.stopped.iter().position(|d| d.id == id)?;
                    Some(l.stopped.remove(i))
                });
                if let Some(d) = d {
                    let (id, name) = (d.id.clone(), d.name.clone());
                    if let Err(e) = self.start_one(d) {
                        self.issue(&format!("output-{id}"), format!("{name}: {e}"));
                    }
                }
            }
            Msg::SkipDestination(id) => {
                if self
                    .live
                    .as_ref()
                    .is_some_and(|l| l.blocked.as_deref() == Some(&id))
                {
                    if let Some(d) = self.stop_one(&id)
                        && let Some(l) = self.live.as_mut()
                    {
                        l.stopped.push(d);
                    }
                    if let Some(l) = self.live.as_mut() {
                        l.blocked = None;
                    }
                    self.clear_issue("start");
                    self.next_in_queue();
                }
            }
            Msg::Resolved(dests, failed, broadcasts, start) if start.adding => {
                self.broadcasts.extend(broadcasts);
                for (id, why) in failed {
                    self.issue(&format!("output-{id}"), why);
                }
                self.go_live(dests, start);
            }
            Msg::Resolved(dests, failed, broadcasts, record) => {
                if !std::mem::take(&mut self.resolving) {
                    // ended while it was being found: the broadcasts made for it end too
                    end_broadcasts(broadcasts);
                    return;
                }
                self.broadcasts = broadcasts;
                for (id, why) in failed {
                    self.issue(&format!("output-{id}"), why);
                }
                if dests.is_empty() {
                    self.set_live(LiveState::Off);
                } else {
                    self.go_live(dests, record);
                }
                if self.live.is_none() {
                    end_broadcasts(std::mem::take(&mut self.broadcasts));
                }
            }
            Msg::Said(reason) => self.event(Event::Failed { reason }),
            Msg::Edit(make) => {
                let ws = self.doc.workspace().clone();
                if let Some(c) = make(&ws) {
                    self.apply(c);
                }
            }
            Msg::AlertSettings(s) => {
                if let Err(e) = s.save(&self.folder) {
                    self.event(Event::Failed {
                        reason: format!("The alert settings weren't saved: {e}"),
                    });
                }
                self.alert_settings = s;
            }
            Msg::TestAlert(kind) => {
                let mut e = crate::alerts::test_event(&kind);
                e.id = u64::MAX - self.alert_queue.len() as u64;
                // a test shows whatever the rules say (even an alert that's off)
                let mut all = self.alert_settings.clone();
                for r in &mut all.rules {
                    r.enabled = true;
                    r.min_amount = 0.0;
                }
                if let Some(a) = crate::alerts::from_event(&e, &all) {
                    self.alert_queue.push(a);
                }
            }
            Msg::EndLive => {
                if std::mem::take(&mut self.resolving) {
                    self.set_live(LiveState::Off);
                }
                self.end_live(None)
            }
            Msg::Device(d) => {
                let networks_changed = d.networks != self.phone.networks;
                self.phone = d;
                if networks_changed {
                    let bonded: Vec<_> = self
                        .hot
                        .outputs
                        .read()
                        .iter()
                        .filter(|o| o.bonded())
                        .cloned()
                        .collect();
                    if !bonded.is_empty() {
                        let links = self.bonding_links();
                        for o in bonded {
                            o.set_links(links.clone());
                        }
                    }
                }
                self.govern();
            }
            Msg::Foreground => {
                self.away = false;
                for o in self.hot.outputs.read().iter() {
                    if let Some(e) = self.hot.video_encoder(o.video_encoder()) {
                        *e.held.lock() = Default::default();
                        e.keyframe_wanted.store(true, Ordering::Release);
                    }
                }
            }
            Msg::Background => {
                self.away = true;
                if self.recording.is_some() {
                    self.stop_recording(Some(
                        "Recording stopped: Lightair left the screen.".into(),
                    ));
                }
            }
            Msg::Close(_) => {}
        }
    }

    fn apply(&mut self, c: Command) {
        // a slider or an edge dragged sends a change a frame: the same key within COALESCE of
        // the last is merged into the open undo step (the document merges same-key changes while
        // a gesture is open); anything else closes it first
        let key = c.coalesce_key();
        let now = Instant::now();
        let same = matches!((key, self.coalescing), (Some(k), Some((open, at))) if open == k && now.duration_since(at) < COALESCE);
        if !same {
            self.end_coalescing();
            if key.is_some() {
                self.doc.begin_gesture();
            }
        }
        self.coalescing = key.map(|k| (k, now));
        match self.doc.apply(&c) {
            Ok(_) => {
                if let Err(e) = self.package.record(&c) {
                    tracing::warn!(%e, "the journal");
                }
                self.refresh();
            }
            Err(e) => self.event(Event::Failed {
                reason: e.to_string(),
            }),
        }
    }

    fn program_scene(&self) -> Id {
        let ws = self.doc.workspace();
        ws.program
            .filter(|p| ws.scene(*p).is_some())
            .or_else(|| ws.scenes.first().map(|s| s.id))
            .unwrap_or_default()
    }

    fn switch(&mut self, scene: Id) {
        // what's on screen now (a switch still waiting for its camera counts as not made)
        let from = self.hold.unwrap_or_else(|| self.program_scene());
        if self.program_scene() == scene && self.hold.is_none()
            || self.doc.workspace().scene(scene).is_none()
        {
            return;
        }
        if from == scene {
            // back to the scene still on screen while the other waited: nothing to switch
            self.pending = None;
            self.hold = None;
            let c = Command::SetProgram { scene: Some(scene) };
            if self.doc.apply(&c).is_ok() {
                let _ = self.package.record(&c);
            }
            self.refresh();
            return;
        }
        let ws = self.doc.workspace().clone();
        let settings = ws
            .scene(scene)
            .and_then(|s| s.transition)
            .unwrap_or(ws.transition);
        let c = Command::SetProgram { scene: Some(scene) };
        if let Err(e) = self.doc.apply(&c) {
            self.event(Event::Failed {
                reason: e.to_string(),
            });
            return;
        }
        let _ = self.package.record(&c);
        // a new camera starts cold (a third of a second on a phone): the old scene stays on
        // screen until its first frame, so neither scene shows black
        let before: Vec<u32> = self.cameras.iter().map(|c| c.1).collect();
        self.hold = Some(from);
        self.refresh();
        let new_feed = self
            .cameras
            .iter()
            .map(|c| c.1)
            .find(|f| !before.contains(f) && !self.feed_arrived(*f));
        let cold = new_feed.is_some_and(|f| !self.feed_arrived(f));
        if cold {
            self.pending = Some(Pending {
                from,
                settings,
                since: Instant::now(),
                feed: new_feed.unwrap_or(0),
            });
        } else {
            self.pending = None;
            self.go(from, settings);
        }
    }

    fn feed_arrived(&self, feed: u32) -> bool {
        self.hot
            .feed(feed)
            .and_then(|f| {
                f.video()
                    .map(|v| v.arrived_ns.load(Ordering::Acquire) != i64::MIN)
            })
            .unwrap_or(true)
    }

    /// The switch itself, from the frame after the one being drawn now (the render thread's
    /// time, not the sound's, which a Bluetooth microphone holds back), so the first frame
    /// starts at 0; published with the new scene in one go, so no frame shows one without the
    /// other.
    fn go(&mut self, from: Id, settings: TransitionSettings) {
        self.hold = None;
        let start = self.hot.clock.drawn_ns() + 16_000_000;
        let switch =
            (settings.kind != TransitionKind::Cut && settings.duration_ms > 0).then_some(Switch {
                from,
                settings,
                start_ns: start,
            });
        self.refresh_with(Some((switch, start)));
    }

    /// Republishes the document to the render thread, the feeds, the mixer and the interface.
    fn refresh(&mut self) {
        self.refresh_with(None);
    }

    /// As `refresh`, with a scene switch that starts now: (the transition, the time it starts).
    fn refresh_with(&mut self, switched: Option<(Option<Switch>, i64)>) {
        let ws = self.doc.workspace().clone();
        if ws.canvas(self.canvas).is_none() {
            self.canvas = ws.canvases.first().map(|c| c.id).unwrap_or_default();
        }
        let program = self.program_scene();
        let feeds = self.ensure_feeds(&ws, program);
        // while a switch waits for its camera, the scene going out is still drawn
        let scene = self.hold.unwrap_or(program);
        let old = self.hot.program();
        let drawn = self.hot.clock.drawn_ns();
        let (switch, on_air) = match switched {
            Some((s, start)) => (s, start),
            None => (
                old.switch.clone().filter(|s| {
                    s.from != scene
                        && drawn < s.start_ns + s.settings.duration_ms as i64 * 1_000_000
                }),
                if old.scene == scene {
                    old.on_air_ns
                } else {
                    drawn
                },
            ),
        };
        self.hot.set_program(Program {
            workspace: ws.clone(),
            folder: self.folder.clone(),
            scene,
            canvas: self.canvas,
            switch,
            privacy_scene: ws.scenes.iter().find(|s| is_privacy_scene(s)).map(|s| s.id),
            feeds,
            on_air_ns: on_air,
            utc_offset_min: self.device.utc_offset_min,
            frozen: self.retired.iter().map(|r| r.1).collect(),
        });
        self.publish();
    }

    fn publish(&mut self) {
        let latency = self
            .inputs
            .iter()
            .find(|i| Some(&i.id) == self.active_input.as_ref())
            .map_or(0, |i| i.latency_ms as i64 * 1_000_000);
        self.hot.mic_latency_ns.store(latency, Ordering::Relaxed);
        self.version += 1;
        let ws = self.doc.workspace().clone();
        let program = self.program_scene();
        let old = self.state.read().clone();
        *self.state.write() = Arc::new(State {
            version: self.version,
            workspace: ws,
            project_id: old.project_id.clone(),
            folder: self.folder.clone(),
            canvas: self.canvas,
            program,
            inputs: self.inputs.clone(),
            active_input: self.active_input.clone(),
            recording: old.recording.clone(),
            live: old.live.clone(),
            privacy: self.hot.privacy().0,
            undo: self.doc.undo_label().map(String::from),
            redo: self.doc.redo_label().map(String::from),
            screen: self.screen,
            web: self.web.values().cloned().collect(),
        });
        self.event(Event::Changed {
            version: self.version,
        });
    }

    fn set_recording(&mut self, r: RecordingState) {
        let mut st = (*self.state.read().clone()).clone();
        st.recording = r.clone();
        self.version += 1;
        st.version = self.version;
        *self.state.write() = Arc::new(st);
        self.event(Event::Recording { state: r });
        self.event(Event::Changed {
            version: self.version,
        });
    }

    fn issue(&mut self, id: &str, text: String) {
        if self.issues.get(id) != Some(&text) {
            self.issues.insert(id.into(), text.clone());
            self.event(Event::Issue {
                id: id.into(),
                text,
                fix: None,
            });
        }
    }

    fn clear_issue(&mut self, id: &str) {
        if self.issues.remove(id).is_some() {
            self.event(Event::IssueCleared { id: id.into() });
        }
    }

    /// Starts and stops captures for what `scene` shows; returns which feed shows each source.
    fn ensure_feeds(&mut self, ws: &Workspace, scene: Id) -> HashMap<Id, u32> {
        let mut map = HashMap::new();
        let Some(s) = ws.scene(scene) else { return map };
        let sources: Vec<Arc<Source>> = s
            .flatten()
            .objects
            .iter()
            .filter_map(|o| ws.sources.get(&o.source?).cloned())
            .collect();
        // the cameras: the scene's camera layers' cameras, two at once where the phone
        // can, else the first
        let limit = if self.device.multi_camera { 2 } else { 1 };
        let mut wanted: Vec<(CamKey, Facing, Option<String>)> = Vec::new();
        for s in sources.iter().filter(|s| s.kind == templates::CAMERA) {
            let key = cam_key(s);
            if wanted.len() < limit && !wanted.iter().any(|w| w.0 == key) {
                wanted.push((key, key.0, device_of(s)));
            }
        }
        // the ones no longer wanted stop first (a phone running one at a time needs it free);
        // their last frames stay for the scene going out
        let (keep, stop): (Vec<_>, Vec<_>) = std::mem::take(&mut self.cameras)
            .into_iter()
            .partition(|c| wanted.iter().any(|w| w.0 == c.0));
        self.cameras = keep;
        for (key, f) in stop {
            self.host.stop_camera(f);
            self.retired.push((key, f));
        }
        while self.retired.len() > 2 {
            let (_, older) = self.retired.remove(0);
            self.drop_feed(older);
        }
        let two = wanted.len() > 1;
        for (key, facing, device) in wanted {
            if self.cameras.iter().any(|c| c.0 == key) {
                continue;
            }
            let id = self.id();
            self.hot
                .feeds
                .write()
                .insert(id, Arc::new(Feed::Video(VideoFeed::default())));
            let canvas = ws
                .canvas(self.canvas)
                .cloned()
                .unwrap_or_else(Canvas::vertical);
            let mut short = canvas.width.min(canvas.height);
            let mut fps = canvas.fps_num / canvas.fps_den.max(1);
            if two {
                // two at once: the hardware's usual cap
                short = short.min(1080);
                fps = fps.min(30);
            }
            let request = CameraRequest {
                facing,
                width: short * 16 / 9,
                height: short,
                fps,
                device,
            };
            match self.host.start_camera(id, request) {
                Ok(()) => self.clear_issue("camera"),
                Err(e) => self.issue("camera", format!("The camera didn't start: {e}")),
            }
            self.cameras.push((key, id));
        }
        // every scene's camera layers: a running camera's, and a retired one's last frame (the
        // scene going out still shows it)
        for s in ws.sources.values().filter(|s| s.kind == templates::CAMERA) {
            let key = cam_key(s);
            if let Some((_, f)) = self.cameras.iter().find(|c| c.0 == key) {
                map.insert(s.id, *f);
            } else if let Some((_, f)) = self.retired.iter().rev().find(|r| r.0 == key) {
                map.insert(s.id, *f);
            }
        }
        // the screen: its feeds for as long as the project has one (sharing isn't restarted by
        // a scene switch)
        // (a layer deleted leaves its source in the project for undo: a screen nobody's layer
        // shows has no feeds, so Add > Screen adds a layer again and its sharing reaches it)
        let shown = |id: Id| {
            ws.scenes
                .iter()
                .any(|sc| sc.flatten().objects.iter().any(|o| o.source == Some(id)))
        };
        let screen = ws
            .sources
            .values()
            .find(|s| s.kind == templates::SCREEN && shown(s.id));
        let wanted = screen.map(|s| (s.id, ws.tracks.iter().any(|t| t.source == s.id)));
        let have = self.screen.map(|s| (s.source, s.audio.is_some()));
        if wanted != have {
            if let Some(old) = self.screen.take() {
                self.drop_feed(old.video);
                if let Some(a) = old.audio {
                    self.drop_feed(a);
                }
                self.clear_issue("screen");
            }
            if let Some((source, sound)) = wanted {
                let video = self.id();
                let audio = sound.then(|| self.id());
                let mut feeds = self.hot.feeds.write();
                feeds.insert(video, Arc::new(Feed::Video(VideoFeed::default())));
                if let Some(a) = audio {
                    feeds.insert(a, Arc::new(Feed::Audio(AudioFeed::default())));
                }
                self.screen = Some(ScreenFeeds {
                    source,
                    video,
                    audio,
                });
            }
        }
        if let Some(s) = self.screen {
            map.insert(s.source, s.video);
        }
        // web layers (imported widgets, pages) in the program: a feed each for the host's web view
        let pages: Vec<(Id, (String, u32, u32, u32))> = sources
            .iter()
            .filter(|s| s.kind == crate::widgets::KIND)
            .filter_map(|s| crate::web::page(&s.settings, &self.shared).map(|p| (s.id, p)))
            .collect();
        let gone: Vec<Id> = self
            .web
            .keys()
            .filter(|id| !pages.iter().any(|(s, _)| s == *id))
            .copied()
            .collect();
        for id in gone {
            if let Some(w) = self.web.remove(&id) {
                self.drop_feed(w.feed);
            }
        }
        for (source, (url, width, height, fps)) in pages {
            let feed = match self.web.get(&source) {
                Some(w) => w.feed,
                None => {
                    let id = self.id();
                    self.hot
                        .feeds
                        .write()
                        .insert(id, Arc::new(Feed::Video(VideoFeed::default())));
                    id
                }
            };
            self.web.insert(
                source,
                crate::web::WebFeed {
                    source,
                    feed,
                    url,
                    width,
                    height,
                    fps,
                },
            );
            map.insert(source, feed);
        }
        // the widget server knows every widget's settings (an open page gets changes at once; a
        // subathon timer counts there, on screen or not)
        let timers = ws
            .sources
            .values()
            .any(|s| crate::widgets::instance(&s.settings).is_some_and(|i| i.kind == "subathon"));
        if (!self.web.is_empty() || timers)
            && let Some(server) = crate::web::server(&self.shared)
        {
            server.set_instances(
                ws.sources
                    .values()
                    .filter_map(|s| crate::widgets::instance(&s.settings))
                    .collect(),
            );
        }
        // media files
        let media: Vec<&Arc<Source>> = sources
            .iter()
            .filter(|s| s.kind == templates::MEDIA)
            .collect();
        let gone: Vec<Id> = self
            .media
            .keys()
            .filter(|id| !media.iter().any(|s| s.id == **id))
            .copied()
            .collect();
        for id in gone {
            if let Some((v, a)) = self.media.remove(&id) {
                self.host.close_media(v);
                self.drop_feed(v);
                if let Some(a) = a {
                    self.drop_feed(a);
                }
            }
        }
        for s in media {
            if !self.media.contains_key(&s.id) {
                let v = self.id();
                let sound = ws.tracks.iter().any(|t| t.source == s.id);
                let a = sound.then(|| self.id());
                self.hot
                    .feeds
                    .write()
                    .insert(v, Arc::new(Feed::Video(VideoFeed::default())));
                if let Some(a) = a {
                    self.hot
                        .feeds
                        .write()
                        .insert(a, Arc::new(Feed::Audio(AudioFeed::default())));
                }
                let file = s.settings["local_file"].as_str().unwrap_or("");
                let path = if Path::new(file).is_absolute() {
                    PathBuf::from(file)
                } else {
                    self.folder.join(file)
                };
                let request = MediaRequest {
                    path: path.to_string_lossy().into(),
                    looped: s.settings["looping"].as_bool().unwrap_or(true),
                    with_sound: sound,
                    audio_feed: a,
                };
                if let Err(e) = self.host.open_media(v, request) {
                    self.issue(
                        &format!("media-{}", s.id),
                        format!("{} can't be played: {e}", s.name),
                    );
                }
                self.media.insert(s.id, (v, a));
            }
            map.insert(s.id, self.media[&s.id].0);
        }
        // the microphone, while the project has one with a track
        let mic = ws
            .sources
            .values()
            .find(|s| s.kind == templates::MIC && ws.tracks.iter().any(|t| t.source == s.id))
            .cloned();
        match (&mic, self.mic) {
            (Some(_), None) => {
                let id = self.id();
                self.hot
                    .feeds
                    .write()
                    .insert(id, Arc::new(Feed::Audio(AudioFeed::default())));
                match self.host.start_microphone(id, self.active_input.clone()) {
                    Ok(()) => self.clear_issue("microphone"),
                    Err(e) => self.issue("microphone", format!("The microphone didn't start: {e}")),
                }
                self.mic = Some(id);
                self.hot.master.store(id, Ordering::Release);
            }
            (None, Some(f)) => {
                self.host.stop_microphone(f);
                self.hot.feeds.write().remove(&f);
                self.mic = None;
                self.hot.master.store(0, Ordering::Release);
            }
            _ => {}
        }
        // the mixer: which feed is which track's source
        let mut routing = Routing::default();
        // the alerts' sound, opened once, always in the mix
        if self.alerts_feed == 0 {
            let id = self.id();
            self.hot
                .feeds
                .write()
                .insert(id, Arc::new(Feed::Audio(AudioFeed::default())));
            self.alerts_feed = id;
        }
        routing
            .feeds
            .push((self.alerts_feed, audio::ALERTS_INPUT, false));
        let mut inputs: HashMap<Id, u32> = HashMap::new();
        if let (Some(m), Some(f)) = (&mic, self.mic) {
            routing.feeds.push((f, audio::node(m.id), true));
            inputs.insert(m.id, f);
        }
        for (src, (_, a)) in &self.media {
            if let Some(a) = a {
                routing.feeds.push((*a, audio::node(*src), false));
                inputs.insert(*src, *a);
            }
        }
        // the screen's sound, its own track, on the program clock as a video's
        if let Some(ScreenFeeds {
            source,
            audio: Some(a),
            ..
        }) = self.screen
        {
            routing.feeds.push((a, audio::node(source), false));
            inputs.insert(source, a);
        }
        *self.routing.lock() = routing;
        let spec = audio::spec(ws, &inputs);
        if *self.audio.spec() != spec {
            self.audio.set(spec);
        }
        self.meters = ws
            .tracks
            .iter()
            .map(|t| (t.id, self.audio.meter(audio::node(t.id))))
            .collect();
        map
    }

    fn add_format(&mut self, vertical: bool) {
        let ws = self.doc.workspace();
        let name = if vertical { "Vertical" } else { "Landscape" };
        if let Some(c) = ws.canvases.iter().find(|c| c.name == name) {
            self.canvas = c.id;
            self.refresh();
            return;
        }
        let first = ws
            .canvases
            .first()
            .cloned()
            .unwrap_or_else(Canvas::vertical);
        let short = first.width.min(first.height);
        let c = templates::canvas(vertical, short, first.fps_num / first.fps_den.max(1));
        self.canvas = c.id;
        self.add_canvas(c);
        self.refresh();
    }

    fn add_sized_format(&mut self, width: u32, height: u32) -> Option<Id> {
        let fits = |n: u32| (16..=4096).contains(&n) && n.is_multiple_of(2);
        if !fits(width) || !fits(height) {
            return None;
        }
        let ws = self.doc.workspace();
        let id = match ws
            .canvases
            .iter()
            .find(|c| c.width == width && c.height == height)
        {
            Some(c) => c.id,
            None => {
                let first = ws
                    .canvases
                    .first()
                    .cloned()
                    .unwrap_or_else(Canvas::vertical);
                let c = Canvas {
                    id: Id::new(),
                    name: format!("{width}×{height}"),
                    width,
                    height,
                    fps_num: first.fps_num,
                    fps_den: first.fps_den,
                };
                let id = c.id;
                self.add_canvas(c);
                id
            }
        };
        if self.recording.is_none() {
            self.canvas = id;
        }
        self.refresh();
        Some(id)
    }

    /// Adds `c` to the project with every layer laid out in it as in the first format, scaled.
    fn add_canvas(&mut self, c: Canvas) {
        let ws = self.doc.workspace().clone();
        let first = ws
            .canvases
            .first()
            .cloned()
            .unwrap_or_else(Canvas::vertical);
        let (kx, ky) = (
            c.width as f32 / first.width.max(1) as f32,
            c.height as f32 / first.height.max(1) as f32,
        );
        let label = format!("Add {}", c.name);
        let mut commands = vec![Command::AddCanvas { canvas: c.clone() }];
        // every object laid out in the new format as in the first, scaled
        for s in &ws.scenes {
            for o in &s.objects {
                if let Some(l) = o.layouts.get(&first.id) {
                    let mut l = *l;
                    l.x *= kx;
                    l.y *= ky;
                    l.width *= kx;
                    l.height *= ky;
                    commands.push(Command::SetLayout {
                        scene: s.id,
                        object: o.id,
                        canvas: c.id,
                        layout: l,
                    });
                }
            }
        }
        self.apply(Command::Batch { label, commands });
    }

    /// Copies a picked file into the project's `assets/` and returns its relative path.
    fn take_file(&self, path: &str) -> Option<String> {
        copy_asset(&self.folder, Path::new(path))
    }

    fn add_layer(&mut self, layer: NewLayer) {
        let ws = self.doc.workspace().clone();
        let scene = self.program_scene();
        let mut extra = Vec::new();
        let (source, object, label) = match layer {
            NewLayer::Screen { with_sound } => {
                // one screen a project: a second layer shows the same source
                let s = ws
                    .sources
                    .values()
                    .find(|s| s.kind == templates::SCREEN)
                    .map(|s| (**s).clone());
                let fresh = s.is_none();
                let s =
                    s.unwrap_or_else(|| templates::source("Screen", templates::SCREEN, json!({})));
                if fresh && with_sound {
                    let mut t = Track::new("Screen", s.id);
                    t.named = true;
                    extra.push(Command::AddTrack {
                        track: t,
                        index: None,
                    });
                }
                let o = templates::object_filling(&ws, &s, Fit::Fit);
                (s, o, "Add screen")
            }
            NewLayer::ExternalCamera { device, name } => {
                let s = templates::source(
                    if name.is_empty() { "Camera" } else { &name },
                    templates::CAMERA,
                    json!({ "facing": "back", "device": device }),
                );
                let o = templates::object_filling(&ws, &s, Fit::Fill);
                (s, o, "Add camera")
            }
            NewLayer::Camera { facing } => {
                let s = templates::source(
                    "Camera",
                    templates::CAMERA,
                    json!({ "facing": facing_name(facing) }),
                );
                let o = templates::object_filling(&ws, &s, Fit::Fill);
                (s, o, "Add camera")
            }
            NewLayer::Image { path } | NewLayer::Gif { path } => {
                let Some(rel) = self.take_file(&path) else {
                    self.event(Event::Failed {
                        reason: "That picture couldn't be added.".into(),
                    });
                    return;
                };
                let (w, h) = image::image_dimensions(self.folder.join(&rel)).unwrap_or((16, 9));
                let name = Path::new(&path)
                    .file_stem()
                    .map_or("Image".into(), |n| n.to_string_lossy().to_string());
                let s = templates::source(&name, templates::IMAGE, json!({ "file": rel }));
                let o = templates::object_centred(&ws, &s, 0.6, w as f32 / h.max(1) as f32);
                (s, o, "Add image")
            }
            NewLayer::Video {
                path,
                looped,
                with_sound,
            } => {
                let Some(rel) = self.take_file(&path) else {
                    self.event(Event::Failed {
                        reason: "That video couldn't be added.".into(),
                    });
                    return;
                };
                let name = Path::new(&path)
                    .file_stem()
                    .map_or("Video".into(), |n| n.to_string_lossy().to_string());
                let s = templates::source(
                    &name,
                    templates::MEDIA,
                    json!({ "local_file": rel, "is_local_file": true, "looping": looped }),
                );
                if with_sound {
                    let mut t = Track::new(&name, s.id);
                    t.named = true;
                    extra.push(Command::AddTrack {
                        track: t,
                        index: None,
                    });
                }
                let o = templates::object_centred(&ws, &s, 0.8, 16.0 / 9.0);
                (s, o, "Add video")
            }
            NewLayer::Text { text } => {
                let s = templates::source(
                    &text.text.chars().take(24).collect::<String>(),
                    templates::TEXT,
                    text_settings(&text),
                );
                let (w, h) = drawn_size(&s).unwrap_or((400.0, 80.0));
                let o = templates::object_sized(&ws, &s, w, h);
                (s, o, "Add text")
            }
            NewLayer::Timer { timer } => {
                let mut settings = text_settings(&timer.text);
                merge(&mut settings, timer_settings(&timer));
                let s = templates::source("Timer", templates::TIMER, settings);
                let (w, h) = drawn_size(&s).unwrap_or((400.0, 80.0));
                let o = templates::object_sized(&ws, &s, w, h);
                (s, o, "Add timer")
            }
            NewLayer::Shape { shape } => {
                let mut settings = templates::colour_settings(shape.fill, 512, 512);
                merge(&mut settings, json!({ "shape": shape_name(shape.kind) }));
                let s = templates::source("Shape", templates::COLOUR, settings);
                let aspect = if shape.kind == ShapeKind::Line {
                    40.0
                } else {
                    1.0
                };
                let mut o = templates::object_centred(&ws, &s, 0.4, aspect);
                for l in o.layouts.values_mut() {
                    l.shape = shape_of(&shape);
                }
                (s, o, "Add shape")
            }
            NewLayer::Widget { kind } => {
                let Some((settings, (w, h))) = crate::widgets::new_settings(&kind) else {
                    self.event(Event::Failed {
                        reason: format!("There's no {kind} widget."),
                    });
                    return;
                };
                let name = lightair_widgets::catalog::by_kind(&kind).map_or("Widget", |b| b.name);
                let s = templates::source(name, crate::widgets::KIND, settings);
                // its catalogue size is in a 1080p format's terms: the same share of the short side
                let frac = (w as f32 / 1080.0).clamp(0.2, 0.95);
                let o = templates::object_centred(&ws, &s, frac, w as f32 / h.max(1) as f32);
                (s, o, "Add widget")
            }
            NewLayer::ImportedWidget { path } => {
                let custom = match lightair_widgets::import(Path::new(&path)) {
                    Ok(c) => c,
                    Err(e) => {
                        self.event(Event::Failed {
                            reason: format!("That widget couldn't be imported: {e}"),
                        });
                        return;
                    }
                };
                let name = Path::new(&path)
                    .file_stem()
                    .map_or("Widget".into(), |n| n.to_string_lossy().to_string());
                let instance = lightair_widgets::Instance::new("custom", Some(custom));
                let (w, h) = (800u32, 600u32);
                let s = templates::source(
                    &name,
                    crate::widgets::KIND,
                    json!({ "url": "", "width": w, "height": h, "fps": 30, "widget": instance }),
                );
                let o = templates::object_centred(&ws, &s, 0.7, w as f32 / h as f32);
                (s, o, "Add widget")
            }
            NewLayer::Colour { argb } => {
                let s = templates::source(
                    "Colour",
                    templates::COLOUR,
                    templates::colour_settings(argb, 64, 64),
                );
                let o = templates::object_filling(&ws, &s, Fit::Stretch);
                (s, o, "Add colour")
            }
        };
        let object_id = object.id;
        // a source the project has already (the screen) isn't added again
        let mut commands = if ws.sources.contains_key(&source.id) {
            Vec::new()
        } else {
            vec![Command::AddSource { source }]
        };
        commands.extend(extra);
        commands.push(Command::AddObject {
            scene,
            object,
            index: None,
        });
        let before = self.doc.workspace().clone();
        self.apply(Command::Batch {
            label: label.into(),
            commands,
        });
        if !Arc::ptr_eq(&before, self.doc.workspace()) {
            self.event(Event::LayerAdded { layer: object_id });
        }
    }

    fn feed_state(&mut self, feed: u32, s: FeedState) {
        // a feed already dropped (a camera switched away from, a layer removed) may still say
        // it stopped or failed: nothing of it is shown, so no issue would ever clear
        if self.hot.feed(feed).is_none() {
            return;
        }
        if self.screen.is_some_and(|s| s.video == feed) {
            match s {
                FeedState::Running => self.clear_issue("screen"),
                FeedState::Stopped => self.issue(
                    "screen",
                    "Screen sharing stopped. Share the screen again from its layer.".into(),
                ),
                FeedState::Interrupted { reason } | FeedState::Failed { reason } => {
                    self.issue("screen", format!("Screen sharing paused: {reason}"))
                }
            }
            return;
        }
        let which = if self.cameras.iter().any(|c| c.1 == feed) {
            "camera"
        } else if self.mic == Some(feed) {
            "microphone"
        } else {
            "media"
        };
        match s {
            FeedState::Running => self.clear_issue(which),
            FeedState::Interrupted { reason } => {
                let what = match which {
                    "microphone" => "Microphone",
                    "camera" => "Camera",
                    _ => "Video",
                };
                self.issue(which, format!("{what} paused: {reason}"))
            }
            FeedState::Failed { reason } => self.issue(which, reason),
            FeedState::Stopped => {}
        }
    }

    fn start_recording(&mut self, camera_clean: bool) {
        if self.recording.is_some() {
            return;
        }
        let ws = self.doc.workspace().clone();
        let Some(c) = ws.canvas(self.canvas).cloned() else {
            return;
        };
        let fps = c.fps_num / c.fps_den.max(1);
        let short = c.width.min(c.height);
        let bitrate = match (short, fps) {
            (s, f) if s >= 1080 && f > 30 => 24_000,
            (s, _) if s >= 1080 => 16_000,
            (s, _) if s >= 720 => 8_000,
            _ => 5_000,
        };
        let config = VideoEncoderConfig {
            codec: Codec::H264,
            width: c.width,
            height: c.height,
            fps,
            bitrate_kbps: bitrate,
            keyframe_interval_s: 2,
            realtime: true,
        };
        self.set_recording(RecordingState::Starting);
        let dir = crate::record::folder(&self.shared);
        let _ = std::fs::create_dir_all(&dir);
        let now = SystemTime::now();
        let unix = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let file = crate::record::file_name(&dir, &ws.name, unix, self.device.utc_offset_min);
        let main = match self.open_part(&config, false, file) {
            Ok(p) => p,
            Err(e) => {
                self.set_recording(RecordingState::Idle);
                self.issue("recording", format!("Recording couldn't start: {e}"));
                return;
            }
        };
        let main_file = main.file.clone();
        let mut parts = vec![main];
        // the camera alone, in a file of its own, where the phone can run a second encode
        if camera_clean {
            if self.device.video_encodes() < 2 {
                self.issue(
                    "camera-clean",
                    "This phone can't record the camera clean as well as the program.".into(),
                );
            } else if self.cameras.is_empty() {
                self.issue(
                    "camera-clean",
                    "No camera in this scene to record clean.".into(),
                );
            } else {
                let stem = main_file
                    .file_stem()
                    .map_or(String::new(), |s| s.to_string_lossy().to_string());
                let file = dir.join(format!("{stem} (camera).mp4"));
                match self.open_part(&config, true, file) {
                    Ok(p) => {
                        self.clear_issue("camera-clean");
                        parts.push(p);
                    }
                    Err(e) => {
                        self.issue("camera-clean", format!("Recording only the program: {e}"))
                    }
                }
            }
        }
        self.clear_issue("recording");
        self.host.keep_screen_on(true);
        self.recording = Some(Rec {
            parts,
            started: now,
            size: (c.width, c.height),
        });
        self.set_recording(RecordingState::Recording {
            since_ms: unix as u64 * 1000,
            file: main_file.to_string_lossy().into(),
        });
    }

    /// Opens a video and an audio encoder and starts their file.
    fn open_part(
        &mut self,
        config: &VideoEncoderConfig,
        clean: bool,
        file: PathBuf,
    ) -> Result<Part, HostError> {
        // registered before the host opens them: its pull loops may start inside the open call
        let video = self.id();
        self.hot.opening.lock().insert(video);
        let target = self.host.open_video_encoder(video, config.clone());
        let target = match target {
            Ok(t) => t,
            Err(e) => {
                self.hot.opening.lock().remove(&video);
                return Err(e);
            }
        };
        let mut enc = VideoEncoder::new(video, config.clone(), target);
        if clean {
            enc = enc.camera_clean();
        }
        let enc = Arc::new(enc);
        self.hot.video_encoders.write().insert(video, enc.clone());
        self.hot.opening.lock().remove(&video);
        let _ = self.render.send(render::Msg::AddEncoder(enc));
        let audio = self.id();
        let sound = AudioEncoderConfig {
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 160,
            codec: AudioCodec::Aac,
        };
        self.hot
            .audio_encoders
            .write()
            .insert(audio, Arc::new(AudioEncoder::new(audio)));
        let audio = match self.host.open_audio_encoder(audio, sound) {
            Ok(()) => Some(audio),
            Err(e) => {
                self.hot.audio_encoders.write().remove(&audio);
                self.issue("recording-sound", format!("Recording without sound: {e}"));
                None
            }
        };
        let id = self.id();
        let request = RecordingRequest {
            path: file.to_string_lossy().into(),
            video_encoder: video,
            audio_encoder: audio,
            fragment_interval_s: 2,
        };
        if let Err(e) = self.host.start_recording(id, request) {
            self.close_encoders(video, audio);
            return Err(e);
        }
        Ok(Part {
            id,
            video,
            audio,
            file,
            done: false,
        })
    }

    fn close_encoders(&mut self, video: u32, audio: Option<u32>) {
        if let Some(e) = self.hot.video_encoders.write().remove(&video) {
            e.close();
        }
        let _ = self.render.send(render::Msg::RemoveEncoder(video));
        self.host.close_video_encoder(video);
        if let Some(a) = audio {
            if let Some(e) = self.hot.audio_encoders.write().remove(&a) {
                e.close();
            }
            self.host.close_audio_encoder(a);
        }
    }

    fn stop_recording(&mut self, why: Option<String>) {
        let Some(r) = &self.recording else { return };
        let parts: Vec<(u32, u32, Option<u32>)> = r
            .parts
            .iter()
            .filter(|p| !p.done)
            .map(|p| (p.id, p.video, p.audio))
            .collect();
        for (id, video, audio) in parts {
            // no more frames or sound: the host's pulls end once what's queued is taken
            if let Some(e) = self.hot.video_encoder(video) {
                e.close();
            }
            if let Some(a) = audio.and_then(|a| self.hot.audio_encoder(a)) {
                a.close();
            }
            self.host.stop_recording(id);
        }
        self.set_recording(RecordingState::Finishing);
        if let Some(text) = why {
            self.issue("recording", text);
        }
    }

    fn recording_finished(&mut self, id: u32, result: RecordingResult) {
        let Some(r) = self.recording.as_mut() else {
            return;
        };
        let Some(i) = r.parts.iter().position(|p| p.id == id && !p.done) else {
            return;
        };
        r.parts[i].done = true;
        let (video, audio, file) = (r.parts[i].video, r.parts[i].audio, r.parts[i].file.clone());
        let (started, size) = (r.started, r.size);
        let all_done = r.parts.iter().all(|p| p.done);
        self.close_encoders(video, audio);
        match result {
            RecordingResult::Finished { duration_ms, .. } => {
                let name = file
                    .file_stem()
                    .map_or(String::new(), |s| s.to_string_lossy().to_string());
                let created_ms = started
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                let item = crate::record::Item {
                    name,
                    created_ms,
                    duration_ms,
                    width: size.0,
                    height: size.1,
                    file,
                };
                if let Err(e) = crate::record::add_to_index(&self.shared, &item) {
                    tracing::warn!(%e, "the Studio index");
                }
            }
            RecordingResult::Failed { reason } => self.issue(
                "recording",
                format!("The recording couldn't be finished: {reason}"),
            ),
        }
        if all_done {
            self.recording = None;
            self.host.keep_screen_on(false);
            self.set_recording(RecordingState::Idle);
        }
    }

    /// One alert at a time, each for its duration.
    fn alerts(&mut self) {
        if self.alert_until.is_some_and(|t| Instant::now() >= t) {
            self.alert_until = None;
            *self.hot.alert.write() = None;
        }
        if self.alert_until.is_none()
            && let Some(a) = self.alert_queue.pop()
        {
            self.alert_until = Some(Instant::now() + Duration::from_millis(a.duration_ms as u64));
            *self.hot.alert.write() = Some((a.clone(), self.hot.clock.drawn_ns()));
            self.event(Event::Alert {
                alert: a,
                sound_feed: self.alerts_feed,
            });
        }
    }

    fn drop_feed(&mut self, f: u32) {
        self.hot.feeds.write().remove(&f);
        let _ = self.render.send(render::Msg::ForgetFeed(f));
    }

    /// A waiting switch goes once its camera has a frame (or after 1.5 s); a retired camera's
    /// feed goes once no switch needs it.
    fn switches(&mut self) {
        if let Some(p) = &self.pending
            && (self.feed_arrived(p.feed) || p.since.elapsed() > Duration::from_millis(1500))
        {
            let p = self.pending.take().unwrap();
            self.go(p.from, p.settings);
        }
        if !self.retired.is_empty() && self.pending.is_none() {
            let prog = self.hot.program();
            let drawn = self.hot.clock.drawn_ns();
            let switching = prog.switch.as_ref().is_some_and(|s| {
                drawn < s.start_ns + s.settings.duration_ms as i64 * 1_000_000 + 100_000_000
            });
            if !switching {
                for (_, f) in std::mem::take(&mut self.retired) {
                    self.drop_feed(f);
                }
                self.refresh();
            }
        }
    }

    fn set_live(&mut self, l: LiveState) {
        let mut st = (*self.state.read().clone()).clone();
        st.live = l.clone();
        self.version += 1;
        st.version = self.version;
        *self.state.write() = Arc::new(st);
        self.event(Event::Live { state: l });
        self.event(Event::Changed {
            version: self.version,
        });
    }

    /// Going live: platform destinations' servers and keys are found first on a helper thread
    /// (Twitch's nearest ingest, sign-in), then `go_live`.
    fn begin_live(&mut self, dests: Vec<crate::outputs::Destination>, record: Start) {
        let allowed = if record.adding {
            self.live.is_some()
        } else {
            self.live.is_none() && !self.resolving
        };
        if !allowed || dests.is_empty() {
            return;
        }
        let platform = |d: &crate::outputs::Destination| {
            matches!(d.kind, crate::outputs::DestinationKind::Platform { .. })
        };
        if !dests.iter().any(platform) {
            self.go_live(dests, record);
            return;
        }
        if !record.adding {
            self.resolving = true;
            self.set_live(LiveState::Starting);
        }
        let me = self.me.clone();
        // a YouTube broadcast made through sign-in is called the title kept for the next YouTube
        // stream, else the project's name
        let title = lightair_live::oauth::kept("youtube-next-title")
            .unwrap_or_else(|| self.doc.workspace().name.clone());
        let spawned = std::thread::Builder::new()
            .name("lightair-golive".into())
            .spawn(move || {
                let (mut ok, mut failed, mut broadcasts) = (Vec::new(), Vec::new(), Vec::new());
                for d in dests {
                    let id = d.id.clone();
                    match d.resolve(&title) {
                        Ok((r, broadcast)) => {
                            ok.push(r);
                            broadcasts.extend(broadcast);
                        }
                        Err(e) => failed.push((id, e)),
                    }
                }
                let _ = me.send(Msg::Resolved(ok, failed, broadcasts, record));
            });
        if spawned.is_err() && !record.adding {
            self.resolving = false;
            self.set_live(LiveState::Off);
        }
    }

    fn go_live(&mut self, dests: Vec<crate::outputs::Destination>, start: Start) {
        if start.adding {
            for d in dests {
                let (id, name) = (d.id.clone(), d.name.clone());
                if let Err(e) = self.start_one(d) {
                    self.issue(&format!("output-{id}"), format!("{name}: {e}"));
                }
            }
            return;
        }
        if self.live.is_some() || dests.is_empty() {
            return;
        }
        let ws = self.doc.workspace().clone();
        // one encode per distinct setting, as many as the phone can run
        let plan = self.stream_plan(&ws, &dests, start.record);
        if let Some(m) = plan.message {
            self.issue("live", m);
            self.set_live(LiveState::Off);
            return;
        }
        self.set_live(LiveState::Starting);
        self.dropped_at_start = self.hot.stats.dropped.load(Ordering::Relaxed);
        let since_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.live = Some(Live {
            groups: Vec::new(),
            audios: Vec::new(),
            since_ms,
            on_air: false,
            queue: Default::default(),
            starting: None,
            blocked: None,
            stopped: Vec::new(),
        });
        if start.ordered {
            if let Some(l) = self.live.as_mut() {
                l.queue = dests.into();
            }
            self.next_in_queue();
        } else {
            for d in dests {
                let (id, name) = (d.id.clone(), d.name.clone());
                if let Err(e) = self.start_one(d) {
                    self.issue(&format!("output-{id}"), format!("{name}: {e}"));
                }
            }
            if self.hot.outputs.read().is_empty() {
                self.issue(
                    "live",
                    "Going live didn't start: no destination could be reached.".into(),
                );
                self.end_live(None);
                return;
            }
        }
        self.clear_issue("live");
        self.host.keep_screen_on(true);
        self.govern();
        if start.record && self.recording.is_none() {
            self.start_recording(false);
        }
    }

    /// Ordered start: the next destination, unless one is starting or waiting for a decision.
    fn next_in_queue(&mut self) {
        let Some(l) = self.live.as_mut() else { return };
        if l.starting.is_some() || l.blocked.is_some() {
            return;
        }
        let Some(d) = l.queue.pop_front() else { return };
        let (id, name) = (d.id.clone(), d.name.clone());
        match self.start_one(d.clone()) {
            Ok(()) => {
                if let Some(l) = self.live.as_mut() {
                    l.starting = Some((id, Instant::now()));
                }
            }
            Err(e) => {
                self.block(&id, &name, &e);
                if let Some(l) = self.live.as_mut() {
                    l.stopped.push(d);
                }
            }
        }
    }

    /// Ordered start: `id` didn't start; the rest wait for "continue without it" or "stop all".
    fn block(&mut self, id: &str, name: &str, why: &str) {
        if let Some(l) = self.live.as_mut() {
            l.blocked = Some(id.into());
            l.starting = None;
        }
        self.issue(
            "start",
            format!("{name} didn't start: {why}. Continue without it, or stop everything."),
        );
    }

    /// Destinations away longer than their own limit are stopped, to start again.
    fn give_up(&mut self) {
        let now = Instant::now();
        let over: Vec<(String, String, u32)> = self
            .hot
            .outputs
            .read()
            .iter()
            .filter_map(|o| {
                let limit = o.dest.give_up_after_s?;
                (o.away_for(now)? >= Duration::from_secs(limit as u64))
                    .then(|| (o.dest.id.clone(), o.dest.name.clone(), limit))
            })
            .collect();
        for (id, name, limit) in over {
            if let Some(d) = self.stop_one(&id)
                && let Some(l) = self.live.as_mut()
            {
                l.stopped.push(d);
            }
            let after = if limit >= 120 {
                format!("{} min", limit / 60)
            } else {
                format!("{limit} s")
            };
            self.issue(
                &format!("output-{id}"),
                format!(
                    "{name} couldn't be reached for {after}: stopped. Start it again from the list."
                ),
            );
        }
    }

    /// Ordered start, once a second: the one starting is on air (the next starts) or failed.
    fn advance_start(&mut self) {
        let Some((id, since)) = self.live.as_ref().and_then(|l| l.starting.clone()) else {
            return;
        };
        let Some(o) = self
            .hot
            .outputs
            .read()
            .iter()
            .find(|o| o.dest.id == id)
            .cloned()
        else {
            return;
        };
        match o.health().state {
            crate::outputs::OutputState::Live | crate::outputs::OutputState::Congested => {
                if let Some(l) = self.live.as_mut() {
                    l.starting = None;
                }
                self.next_in_queue();
            }
            crate::outputs::OutputState::Failed { reason } => {
                let name = o.dest.name.clone();
                self.block(&id, &name, &reason);
            }
            _ if since.elapsed() > Duration::from_secs(20) => {
                let name = o.dest.name.clone();
                self.block(&id, &name, "it didn't connect within 20 s");
            }
            _ => {}
        }
    }

    /// One destination onto the stream that's on: its encode (shared when the settings match
    /// one that's on, else opened if the phone can run one more) and its sound encode.
    fn start_one(&mut self, d: crate::outputs::Destination) -> Result<(), String> {
        let ws = self.doc.workspace().clone();
        let e = crate::outputs::plan_in(
            std::slice::from_ref(&d),
            &formats(&ws, self.canvas),
            u32::MAX,
            0,
            u32::MAX,
        )
        .encodes
        .remove(0);
        let format = format_id(&e.format).unwrap_or(self.canvas);
        let Some(l) = self.live.as_ref() else {
            return Err("nothing is live".into());
        };
        let existing = l
            .groups
            .iter()
            .find(|g| {
                (g.format, g.chosen, g.config.fps, g.chosen_kbps)
                    == (format, (e.width, e.height), e.fps, e.video_kbps)
            })
            .map(|g| g.video);
        // a second format where the phone draws one
        if existing.is_none() {
            let mut drawn: Vec<Id> = l.groups.iter().map(|g| g.format).collect();
            drawn.push(format);
            drawn.sort();
            drawn.dedup();
            if drawn.len() as u32 > self.device.formats_at_once() {
                return Err("this phone streams one format at a time: give it the same format as the ones that are on".into());
            }
        }
        let video = match existing {
            Some(v) => v,
            None => {
                let recording = self
                    .recording
                    .as_ref()
                    .map_or(0, |r| r.parts.iter().filter(|p| !p.done).count() as u32);
                if l.groups.len() as u32 + recording >= self.device.video_encodes() {
                    return Err("this phone can't run another stream encode now; give it the same quality as one that's on".into());
                }
                let config = VideoEncoderConfig {
                    codec: Codec::H264,
                    width: e.width,
                    height: e.height,
                    fps: e.fps,
                    bitrate_kbps: e.video_kbps,
                    keyframe_interval_s: 2,
                    realtime: true,
                };
                let v = self
                    .open_stream_encoder(config.clone(), format)
                    .map_err(|e| format!("the phone's encoder didn't open: {e}"))?;
                if let Some(l) = self.live.as_mut() {
                    l.groups.push(Group {
                        video: v,
                        format,
                        chosen_kbps: e.video_kbps,
                        destinations: Vec::new(),
                        config,
                        chosen: (e.width, e.height),
                        rung: 0,
                        changed_at: Instant::now(),
                    });
                }
                v
            }
        };
        // its sound: Opus for WHIP, else AAC, one encode per codec and bitrate
        let key = (
            match d.kind {
                crate::outputs::DestinationKind::Whip { .. } => AudioCodec::Opus,
                _ => AudioCodec::Aac,
            },
            d.audio_kbps.max(64),
        );
        let audio = match self
            .live
            .as_ref()
            .and_then(|l| l.audios.iter().find(|(k, _)| *k == key).map(|(_, a)| *a))
        {
            Some(a) => a,
            None => {
                let audio = self.id();
                self.hot
                    .audio_encoders
                    .write()
                    .insert(audio, Arc::new(AudioEncoder::new(audio)));
                let sound = AudioEncoderConfig {
                    sample_rate: 48_000,
                    channels: 2,
                    bitrate_kbps: key.1,
                    codec: key.0,
                };
                if let Err(e) = self.host.open_audio_encoder(audio, sound) {
                    self.hot.audio_encoders.write().remove(&audio);
                    self.issue("live-sound", format!("Live without sound: {e}"));
                }
                if let Some(l) = self.live.as_mut() {
                    l.audios.push((key, audio));
                }
                audio
            }
        };
        let id = d.id.clone();
        // its network: bound to it when the phone has it, else said
        let bind = match d.network {
            Some(kind) => {
                let path = self.phone.networks.iter().find(|n| n.kind == kind);
                match path.and_then(|p| p.bind()) {
                    Some(b) => Some(b),
                    None => {
                        self.release_unused();
                        return Err(format!(
                            "{} isn't available on this phone now",
                            match kind {
                                NetworkKind::Cellular => "Mobile data",
                                NetworkKind::Wired => "A wired network",
                                _ => "Wi-Fi",
                            }
                        ));
                    }
                }
            }
            None => None,
        };
        let links = if matches!(d.kind, crate::outputs::DestinationKind::Srtla { .. }) {
            self.bonding_links()
        } else {
            Vec::new()
        };
        let o = match crate::outputs::Output::start(d, video, audio, bind, links) {
            Ok(o) => Arc::new(o),
            Err(e) => {
                self.release_unused();
                return Err(e);
            }
        };
        if let Some(g) = self
            .live
            .as_mut()
            .and_then(|l| l.groups.iter_mut().find(|g| g.video == video))
        {
            g.destinations.push(id);
            // a shared encode may be at a smaller size already; the smallest has no floor
            o.set_size(g.size_at(g.rung));
        }
        self.hot.outputs.write().push(o.clone());
        // the parameter sets the encoder gave already (with its first keyframe): a sender needs
        // them for the sequence header; and a keyframe for the new destination
        if let Some(e) = self.hot.video_encoder(video) {
            let nals = e.sent.lock().parameter_sets.clone();
            if !nals.is_empty() {
                o.parameter_sets(&nals);
            }
            e.keyframe_wanted.store(true, Ordering::Release);
        }
        self.apply_bitrate();
        Ok(())
    }

    /// One destination off the stream (the rest go on); its encodes close when nothing else
    /// uses them. The destination as it was, to start again.
    fn stop_one(&mut self, id: &str) -> Option<crate::outputs::Destination> {
        let o = {
            let mut outputs = self.hot.outputs.write();
            let i = outputs.iter().position(|o| o.dest.id == id)?;
            outputs.remove(i)
        };
        if let Some(l) = self.live.as_mut() {
            for g in &mut l.groups {
                g.destinations.retain(|d| d != id);
            }
        }
        self.clear_issue(&format!("output-{id}"));
        self.clear_issue(&format!("output-{id}-ok"));
        self.release_unused();
        Some(o.dest.clone())
    }

    /// Encodes no destination uses any more, closed.
    /// A bonded destination's links: every network the phone has, each bound to it; one
    /// unbound link where the host doesn't say (or can't bind).
    fn bonding_links(&self) -> crate::outputs::Links {
        let mut links: crate::outputs::Links = Vec::new();
        for n in &self.phone.networks {
            let Some(bind) = n.bind() else { continue };
            let kind = match n.kind {
                NetworkKind::Wifi => "Wi-Fi",
                NetworkKind::Cellular => "Mobile data",
                NetworkKind::Wired => "Wired",
                NetworkKind::None => continue,
            };
            // two of a kind (two SIMs, a USB modem) by their interface too
            let name = if links.iter().any(|(l, _)| l == kind) {
                format!("{kind} ({})", n.interface)
            } else {
                kind.to_string()
            };
            links.push((name, Some(bind)));
        }
        if links.is_empty() {
            links.push(("Network".into(), None));
        }
        links
    }

    fn release_unused(&mut self) {
        let outputs = self.hot.outputs.read().clone();
        let Some(l) = self.live.as_mut() else { return };
        let (groups, keep): (Vec<Group>, Vec<Group>) = std::mem::take(&mut l.groups)
            .into_iter()
            .partition(|g| !outputs.iter().any(|o| o.video_encoder() == g.video));
        l.groups = keep;
        let (audios, keep_a): (Vec<_>, Vec<_>) = std::mem::take(&mut l.audios)
            .into_iter()
            .partition(|(_, a)| !outputs.iter().any(|o| o.audio_encoder == *a));
        l.audios = keep_a;
        for g in groups {
            self.close_encoders(g.video, None);
        }
        for (_, a) in audios {
            self.close_audio_encoder(a);
        }
    }

    /// The encodes `dests` take, and whether the phone can run them beside a
    /// recording (one that's on, or `record`).
    fn stream_plan(
        &self,
        ws: &Workspace,
        dests: &[crate::outputs::Destination],
        record: bool,
    ) -> crate::outputs::StreamPlan {
        let recording = match &self.recording {
            Some(r) => r.parts.iter().filter(|p| !p.done).count() as u32,
            None => record as u32,
        };
        crate::outputs::plan_in(
            dests,
            &formats(ws, self.canvas),
            self.device.video_encodes(),
            recording,
            self.device.formats_at_once(),
        )
    }

    /// A stream's video encoder, opened through the host and drawn into.
    fn open_stream_encoder(
        &mut self,
        config: VideoEncoderConfig,
        format: Id,
    ) -> Result<u32, String> {
        let video = self.id();
        self.hot.opening.lock().insert(video);
        let target = match self.host.open_video_encoder(video, config.clone()) {
            Ok(t) => t,
            Err(e) => {
                self.hot.opening.lock().remove(&video);
                return Err(e.to_string());
            }
        };
        // registered before it leaves `opening`: a pull loop the host started inside the open
        // call always finds it in one of the two (in neither reads as closed, and iOS's loop
        // ends there: the encode never gets a frame)
        let enc = Arc::new(VideoEncoder::new(video, config, target).in_format(format));
        self.hot.video_encoders.write().insert(video, enc.clone());
        self.hot.opening.lock().remove(&video);
        let _ = self.render.send(render::Msg::AddEncoder(enc));
        Ok(video)
    }

    fn close_audio_encoder(&mut self, a: u32) {
        if let Some(e) = self.hot.audio_encoders.write().remove(&a) {
            e.close();
        }
        self.host.close_audio_encoder(a);
    }

    fn end_live(&mut self, why: Option<String>) {
        end_broadcasts(std::mem::take(&mut self.broadcasts));
        self.handed.clear();
        self.take_back_at = None;
        let Some(l) = self.live.take() else { return };
        self.set_live(LiveState::Ending);
        drop(std::mem::take(&mut *self.hot.outputs.write()));
        // what the stream said goes with it (its destinations' lines, a missing sound); why it
        // ended, if the engine ended it, stays
        let said: Vec<String> = self
            .issues
            .keys()
            .filter(|k| {
                k.starts_with("output-")
                    || k.starts_with("resolution-")
                    || *k == "live-sound"
                    || *k == "live"
                    || *k == "start"
            })
            .cloned()
            .collect();
        for id in said {
            self.clear_issue(&id);
        }
        for g in &l.groups {
            self.close_encoders(g.video, None);
        }
        for (_, a) in &l.audios {
            self.close_audio_encoder(*a);
        }
        if self.recording.is_none() {
            self.host.keep_screen_on(false);
        }
        if let Some(why) = why {
            self.issue("live", why);
        }
        self.set_live(LiveState::Off);
    }

    /// Once a second while live: each destination's state machine and bitrate, the
    /// stream's keyframes, and what to say.
    fn watch_outputs(&mut self) {
        self.advance_start();
        self.give_up();
        let Some(l) = &self.live else { return };
        let since = l.since_ms;
        let outputs = self.hot.outputs.read().clone();
        let now = Instant::now();
        let mut changed = false;
        for o in &outputs {
            let (target, keyframe, message) = o.watch(now);
            changed |= target.is_some();
            if keyframe && let Some(e) = self.hot.video_encoder(o.video_encoder()) {
                e.keyframe_wanted.store(true, Ordering::Release);
            }
            if let Some(m) = message {
                let id = format!("output-{}", o.dest.id);
                // one line per destination: the trouble, or (in its place) the good news
                let ok = format!("{id}-ok");
                if o.health().state == crate::outputs::OutputState::Live {
                    self.clear_issue(&id);
                    self.issue(&ok, m);
                } else {
                    self.clear_issue(&ok);
                    self.issue(&id, m);
                }
            }
        }
        if changed {
            self.apply_bitrate();
        }
        self.resolution_floor(now);
        let on_air = outputs
            .iter()
            .any(|o| o.health().state != crate::outputs::OutputState::Connecting);
        if let Some(l) = self.live.as_mut()
            && on_air
            && !l.on_air
        {
            l.on_air = true;
            self.set_live(LiveState::Live { since_ms: since });
        }
    }

    /// The resolution floor: an encode whose destination sits congested at the
    /// bitrate floor steps its size down a rung (1080 → 720 → 540) instead; back up a rung
    /// once all its destinations have streamed at their full bitrate for a calm minute.
    fn resolution_floor(&mut self, now: Instant) {
        let Some(l) = &self.live else { return };
        let outputs = self.hot.outputs.read().clone();
        let mut changes = Vec::new();
        for (i, g) in l.groups.iter().enumerate() {
            if now.duration_since(g.changed_at) < Duration::from_secs(10) {
                continue;
            }
            let mine: Vec<_> = outputs
                .iter()
                .filter(|o| o.video_encoder() == g.video)
                .collect();
            if mine.is_empty() {
                continue;
            }
            if g.rung + 1 < g.rungs().len() && mine.iter().any(|o| o.at_floor(now)) {
                changes.push((i, g.rung + 1));
            } else if g.rung > 0 && mine.iter().all(|o| o.steady_at_full(now)) {
                changes.push((i, g.rung - 1));
            }
        }
        for (i, rung) in changes {
            self.set_rung(i, rung, now);
        }
    }

    /// Reopens encode `i` at `rung`'s size; its destinations take the new encoder's packets.
    fn set_rung(&mut self, i: usize, rung: usize, now: Instant) {
        let Some(g) = self.live.as_ref().and_then(|l| l.groups.get(i)) else {
            return;
        };
        let (old, short, chosen, format) = (g.video, g.rungs()[rung], g.chosen, g.format);
        let (size, down) = (g.size_at(rung), rung > g.rung);
        let mut config = g.config.clone();
        (config.width, config.height) = stream_size(chosen, Some(short));
        let outputs: Vec<_> = self
            .hot
            .outputs
            .read()
            .iter()
            .filter(|o| o.video_encoder() == old)
            .cloned()
            .collect();
        // smaller: it starts at what its destinations' networks carry, not above the bitrate
        // they've stepped to (the floor is lower for the smaller picture: often under the old
        // one); larger: at the bitrate they've stepped to
        let kbps = outputs
            .iter()
            .map(|o| {
                if down {
                    o.kbps_at(size)
                } else {
                    o.target_kbps()
                }
            })
            .min();
        if let Some(kbps) = kbps {
            config.bitrate_kbps = kbps.max(300);
        }
        let new = match self.open_stream_encoder(config.clone(), format) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("the stream's encoder didn't reopen at {short}p: {e}");
                return;
            }
        };
        for o in &outputs {
            o.set_video_encoder(new);
            o.size_changed(size, down);
        }
        self.close_encoders(old, None);
        if let Some(g) = self.live.as_mut().and_then(|l| l.groups.get_mut(i)) {
            g.video = new;
            g.config = config;
            g.rung = rung;
            g.changed_at = now;
        }
        let names: Vec<&str> = outputs.iter().map(|o| o.dest.name.as_str()).collect();
        let id = format!("resolution-{i}");
        if rung == 0 {
            self.clear_issue(&id);
        } else {
            self.issue(
                &id,
                format!(
                    "Your network is slow: streaming to {} at {short}p so the picture stays watchable.",
                    names.join(", ")
                ),
            );
        }
        self.apply_bitrate();
    }

    /// Each stream encode's bitrate: the slowest step of the destinations sharing it (only
    /// those), under the governor's cap.
    fn apply_bitrate(&mut self) {
        let Some(l) = &self.live else { return };
        let outputs = self.hot.outputs.read().clone();
        let cap = self.governor.effects(Instant::now()).bitrate_percent;
        let set: Vec<(u32, u32)> = l
            .groups
            .iter()
            .filter_map(|g| {
                let min = outputs
                    .iter()
                    .filter(|o| o.video_encoder() == g.video)
                    .map(|o| o.target_kbps())
                    .min()?;
                Some((g.video, min.min(g.chosen_kbps * cap / 100).max(300)))
            })
            .collect();
        for (video, kbps) in set {
            self.host.set_video_bitrate(video, kbps);
        }
    }

    /// The thermal governor on the device state.
    fn govern(&mut self) {
        let now = Instant::now();
        let d = self.phone.clone();
        let msg = self.governor.update(d.thermal, d.battery, d.charging, now);
        let fx = self.governor.effects(now);
        self.hot
            .preview_every
            .store(fx.preview_every, Ordering::Relaxed);
        self.hot
            .encode_every
            .store(fx.encode_every, Ordering::Relaxed);
        self.hot
            .governor_step
            .store(self.governor.step as u32, Ordering::Relaxed);
        if let Some(m) = msg {
            self.governor_message = Some(m.to_string());
            if self.governor.step == 0 {
                self.clear_issue("thermal");
            } else {
                self.issue("thermal", m.to_string());
            }
            self.apply_bitrate();
        }
        if fx.stop_recording && self.recording.is_some() && self.live.is_some() {
            self.stop_recording(Some(
                "Recording stopped: the phone is too hot. The stream goes on.".into(),
            ));
        }
        if fx.end_stream && self.live.is_some() {
            self.end_live(Some(
                "The stream ended: the phone was too hot for too long. It ended cleanly, before the system could stop the camera.".into(),
            ));
        }
    }

    fn health(&mut self) {
        if self.live.is_none() && self.recording.is_none() {
            return;
        }
        let groups: Vec<(u32, u32, u32)> = self.live.as_ref().map_or(Vec::new(), |l| {
            l.groups
                .iter()
                .map(|g| (g.video, g.config.width, g.config.height))
                .collect()
        });
        let outputs: Vec<crate::outputs::OutputHealth> = self
            .hot
            .outputs
            .read()
            .iter()
            .map(|o| {
                let mut h = o.health();
                if let Some(i) = groups.iter().position(|g| g.0 == o.video_encoder()) {
                    (h.encode, h.width, h.height) = (i as u32, groups[i].1, groups[i].2);
                }
                h.encoder_dropped = self
                    .hot
                    .video_encoder(o.video_encoder())
                    .map_or(0, |e| e.dropped.load(Ordering::Relaxed));
                h
            })
            .collect();
        let dropped_network = outputs.iter().map(|o| o.dropped_frames).sum();
        let report = HealthReport {
            outputs,
            device: self.phone.clone(),
            governor_step: self.governor.step,
            governor_message: self.governor_message.clone(),
            queued: self.live.as_ref().map_or(Vec::new(), |l| {
                l.queue.iter().map(|d| d.id.clone()).collect()
            }),
            blocked: self.live.as_ref().and_then(|l| l.blocked.clone()),
            stopped: self.live.as_ref().map_or(Vec::new(), |l| {
                l.stopped.iter().map(|d| d.id.clone()).collect()
            }),
            dropped_network,
            dropped_encoder: groups
                .iter()
                .filter_map(|g| self.hot.video_encoder(g.0))
                .map(|e| e.dropped.load(Ordering::Relaxed))
                .sum(),
            dropped_render: self
                .hot
                .stats
                .dropped
                .load(Ordering::Relaxed)
                .saturating_sub(self.dropped_at_start),
        };
        self.event(Event::Health { report });
    }

    /// In the background while live: the held picture again, at the sound's time.
    fn repeat_held(&mut self) {
        if !self.away || self.held_at.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.held_at = Instant::now();
        let pts = crate::clock::samples_to_ns(self.hot.clock.samples());
        for o in self.hot.outputs.read().iter() {
            let Some(e) = self.hot.video_encoder(o.video_encoder()) else {
                continue;
            };
            if let Some((data, length_prefixed)) = &e.held.lock().packet {
                e.packet(data.len(), pts, true);
                o.video(data, *length_prefixed, pts, true);
            }
        }
    }

    /// The open run of changes is one undo step from here on.
    fn end_coalescing(&mut self) {
        if self.coalescing.take().is_some() {
            self.doc.end_gesture();
        }
    }

    fn tick(&mut self) {
        if self
            .coalescing
            .is_some_and(|(_, at)| at.elapsed() >= COALESCE)
        {
            self.end_coalescing();
        }
        self.switches();
        if self.take_back_at.is_some_and(|t| Instant::now() >= t) {
            self.take_back_at = None;
            for d in std::mem::take(&mut self.handed) {
                let (id, name) = (d.id.clone(), d.name.clone());
                if self.live.is_some()
                    && let Err(e) = self.start_one(d)
                {
                    self.issue(&format!("output-{id}"), format!("{name}: {e}"));
                }
            }
        }
        self.repeat_held();
        if self.chat_at.elapsed() >= Duration::from_millis(100) {
            self.chat_at = Instant::now();
            // chat safety (App Store 1.2): hidden people left out, filtered words masked, before
            // the chat panel, the chat box overlay and the alerts (spoken ones included) see it
            let messages: Vec<_> = self
                .chat
                .take_new()
                .into_iter()
                .filter_map(crate::moderation::apply)
                .collect();
            if !messages.is_empty() {
                let mut c = self.hot.chat.write();
                c.0.extend(messages.iter().cloned());
                let over = c.0.len().saturating_sub(300);
                c.0.drain(..over);
                c.1 += messages.len() as u64;
            }
            for m in &messages {
                if let Some(a) = crate::alerts::from_event(m, &self.alert_settings) {
                    self.alert_queue.push(a);
                }
            }
            if !messages.is_empty() {
                self.event(Event::Chat { messages });
            }
            self.alerts();
        }
        if self.health_at.elapsed() >= Duration::from_secs(1) {
            self.health_at = Instant::now();
            self.watch_outputs();
            self.govern();
            self.health();
        }
        // an engine thread's trouble: once, in plain words
        let failed = self.hot.failed.lock().take();
        if let Some(why) = failed {
            self.issue(
                "engine",
                format!("{why}. Close the project and open it again."),
            );
            self.event(Event::Failed { reason: why });
        }
        self.audio.collect();
        if self.meters_at.elapsed() >= Duration::from_millis(50) {
            self.meters_at = Instant::now();
            let levels: Vec<Level> = self
                .meters
                .iter()
                .map(|(t, m)| {
                    let r = m.read();
                    Level {
                        track: *t,
                        peak_db: r.peak_db[0].max(r.peak_db[1]),
                        rms_db: r.rms_db[0].max(r.rms_db[1]),
                    }
                })
                .collect();
            let program_lufs = self.audio.meter(la::PROGRAM).loudness().momentary_lufs;
            *self.hot.levels.write() = levels.clone();
            self.event(Event::Meters {
                levels,
                program_lufs,
            });
        }
        if self.stats_at.elapsed() >= Duration::from_secs(1) {
            let secs = self.stats_at.elapsed().as_secs_f32();
            self.stats_at = Instant::now();
            let frames = self.hot.stats.frames.load(Ordering::Relaxed);
            let dropped = self.hot.stats.dropped.load(Ordering::Relaxed);
            let us = self.hot.stats.render_us.swap(0, Ordering::Relaxed);
            let n = frames - self.frames_seen;
            self.event(Event::Stats {
                fps: n as f32 / secs,
                render_ms: if n > 0 {
                    us as f32 / n as f32 / 1000.0
                } else {
                    0.0
                },
                dropped: (dropped - self.dropped_seen) as u32,
            });
            self.frames_seen = frames;
            self.dropped_seen = dropped;
            let _ = self.package.flush();
            // a camera that stopped sending
            let mut quiet = None;
            for (_, f) in &self.cameras {
                if let Some(v) = self.hot.feed(*f).and_then(|f| match &*f {
                    Feed::Video(_) => Some(f.clone()),
                    _ => None,
                }) && let Some(v) = v.video()
                {
                    let last = v.arrived_ns.load(Ordering::Acquire);
                    if last != i64::MIN {
                        let now_quiet = crate::clock::host_now_ns() - last > 500_000_000;
                        quiet = Some(quiet.unwrap_or(false) || now_quiet);
                    }
                }
            }
            match quiet {
                Some(true) => self.issue(
                    "camera-paused",
                    "Camera paused: no pictures from the camera.".into(),
                ),
                Some(false) => self.clear_issue("camera-paused"),
                None => {}
            }
        }
    }

    fn close(&mut self) {
        self.end_live(None);
        if self.recording.is_some() {
            self.stop_recording(None);
        }
        for (_, f) in std::mem::take(&mut self.cameras) {
            self.host.stop_camera(f);
        }
        if let Some(f) = self.mic.take() {
            self.host.stop_microphone(f);
        }
        for (_, (v, _)) in self.media.drain() {
            self.host.close_media(v);
        }
        self.host.keep_screen_on(false);
        let _ = self.package.compact(self.doc.workspace());
        let _ = self.package.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_inspector_sets_reads_back_the_same() {
        let text = TextSpec {
            text: "Live soon".into(),
            font_family: Some("Inter".into()),
            size: 72.0,
            color: 0x12_34_56,
            bold: true,
            align: 2,
            outline: Some(0x00_00_00),
            background: Some(0xAA_BB_CC),
        };
        let src = templates::source("T", templates::TEXT, text_settings(&text));
        assert_eq!(text_spec_of(&src), text);

        let timer = TimerSpec {
            mode: TimerMode::CountUp,
            seconds: 90,
            text: text.clone(),
        };
        let mut settings = text_settings(&timer.text);
        merge(&mut settings, timer_settings(&timer));
        assert_eq!(
            timer_spec_of(&templates::source("Timer", templates::TIMER, settings)),
            timer
        );

        let shape = ShapeSpec {
            kind: ShapeKind::Ellipse,
            fill: 0x80_FF_00_00,
            stroke: Some(0xFF_00_FF_00),
            stroke_width: 6.0,
        };
        let mut settings = templates::colour_settings(shape.fill, 512, 512);
        merge(&mut settings, json!({ "shape": shape_name(shape.kind) }));
        let src = templates::source("Shape", templates::COLOUR, settings);
        let mut layout = Layout::boxed(0.0, 0.0, 10.0, 10.0);
        layout.shape = shape_of(&shape);
        assert_eq!(shape_spec_of(&src, Some(&layout)), Some(shape));
        assert_eq!(colour_of(&src), 0x80_FF_00_00);
        // a plain colour isn't a shape
        let plain = templates::source(
            "C",
            templates::COLOUR,
            templates::colour_settings(0xFF_11_22_33, 64, 64),
        );
        assert_eq!(shape_spec_of(&plain, None), None);
        assert_eq!(colour_of(&plain), 0xFF_11_22_33);
        assert_eq!(
            templates::colour_settings(0xFF_11_22_33, 0, 0)["color"],
            json!(0xFF_33_22_11u32)
        );
    }
}
