//! What the engine's threads share: the clock, the feeds, the
//! encoders' queues and what the render thread draws. The hot-path calls (`ffi`) reach it through
//! a registry of open sessions by handle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use lightair_model::{Id, TransitionSettings, Workspace};
use parking_lot::{Mutex, RwLock};

use crate::clock::ProgramClock;
use crate::encode::{AudioEncoder, VideoEncoder};
use crate::feeds::Feed;

/// A scene switch in progress.
#[derive(Debug, Clone)]
pub struct Switch {
    pub from: Id,
    pub settings: TransitionSettings,
    /// Program time it started at.
    pub start_ns: i64,
}

/// What the render thread draws now.
#[derive(Clone)]
pub struct Program {
    pub workspace: Arc<Workspace>,
    /// The project's folder (relative paths in settings).
    pub folder: PathBuf,
    pub scene: Id,
    pub canvas: Id,
    pub switch: Option<Switch>,
    /// The project's own privacy scene, if it has one (whether the cut is on is `Hot::privacy`).
    pub privacy_scene: Option<Id>,
    /// Which feed shows each camera or media source.
    pub feeds: HashMap<Id, u32>,
    /// Program time the scene went on air (timers count from it).
    pub on_air_ns: i64,
    pub utc_offset_min: i32,
    /// A camera feed being retired (its camera stopped for another): its last frame stays, not
    /// dimmed, for the scene going out.
    pub frozen: Vec<u32>,
}

#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub dropped: AtomicU64,
    /// Sum of render times since the last read, in µs.
    pub render_us: AtomicU64,
}

pub struct Hot {
    /// Android's zero-copy camera frames: the tokens of buffers the host may let go of.
    pub released: Arc<Mutex<Vec<i64>>>,
    pub clock: ProgramClock,
    pub feeds: RwLock<HashMap<u32, Arc<Feed>>>,
    pub video_encoders: RwLock<HashMap<u32, Arc<VideoEncoder>>>,
    /// The stream's destinations: packets from their encoders go to each.
    pub outputs: RwLock<Vec<Arc<crate::outputs::Output>>>,
    /// The thermal governor's effects on the render thread: the preview and the
    /// encoders drawn every nth frame.
    pub preview_every: AtomicU32,
    pub encode_every: AtomicU32,
    /// The thermal governor's step now, for the Looks screen.
    pub governor_step: AtomicU32,
    /// A layer drawn without its looks on the preview alone (the Looks screen's Compare).
    pub preview_bypass: Mutex<Option<Id>>,
    /// When the control thread last went round its loop (host clock): closing tells a thread
    /// stuck in a host call from one finishing a recording by it.
    pub control_beat_ns: AtomicI64,
    /// The alert on now and the program time it began: the alert box widget draws it.
    pub alert: RwLock<Option<(crate::alerts::Alert, i64)>>,
    /// Chat and events so far (the last 300), for the widgets, and a count that changes with it.
    pub chat: RwLock<(Vec<lightair_live::ChatEvent>, u64)>,
    /// Video encoders the host is opening (its pull loop may start before the engine has the
    /// target it returns): a pull for one waits instead of being told it's closed.
    pub opening: Mutex<std::collections::HashSet<u32>>,
    pub audio_encoders: RwLock<HashMap<u32, Arc<AudioEncoder>>>,
    pub program: RwLock<Arc<Program>>,
    pub stats: Stats,
    /// The microphone feed whose sound drives the clock (0: none).
    pub master: AtomicU32,
    /// How late the master's sound arrives after it was heard (a Bluetooth microphone's
    /// 150–250 ms), in ns: the clock line is moved back by it, so pictures and sound stay in
    /// step.
    pub mic_latency_ns: AtomicI64,
    /// The privacy cut, the one place it's kept: on (bit 63) from program sample (the rest).
    /// The picture and the sound both change at that program time, whatever a Bluetooth
    /// microphone's delay (`privacy_at`).
    privacy: AtomicU64,
    /// Something went wrong on an engine thread (a panic, a GPU error): the control thread says
    /// so (`Event::Failed`).
    pub failed: Mutex<Option<String>>,
    /// Each track's level now (peak, rms dB), for a remote controller's meters.
    pub levels: RwLock<Vec<crate::host::Level>>,
    /// Said once: the first trouble is the one worth saying (a lost GPU errs every frame).
    failed_once: std::sync::atomic::AtomicBool,
    /// The mix thread, woken by the master's pushes.
    pub mixer: Mutex<Option<std::thread::Thread>>,
}

impl Hot {
    pub fn new(program: Program, start_ns: i64) -> Hot {
        Hot {
            clock: ProgramClock::new(start_ns),
            feeds: RwLock::new(HashMap::new()),
            video_encoders: RwLock::new(HashMap::new()),
            opening: Mutex::new(Default::default()),
            alert: RwLock::new(None),
            chat: RwLock::new((Vec::new(), 0)),
            outputs: RwLock::new(Vec::new()),
            preview_every: AtomicU32::new(1),
            governor_step: AtomicU32::new(0),
            preview_bypass: Mutex::new(None),
            control_beat_ns: AtomicI64::new(i64::MIN),
            encode_every: AtomicU32::new(1),
            released: Default::default(),
            audio_encoders: RwLock::new(HashMap::new()),
            program: RwLock::new(Arc::new(program)),
            stats: Stats::default(),
            master: AtomicU32::new(0),
            mic_latency_ns: AtomicI64::new(0),
            privacy: AtomicU64::new(0),
            failed: Mutex::new(None),
            levels: RwLock::new(Vec::new()),
            failed_once: std::sync::atomic::AtomicBool::new(false),
            mixer: Mutex::new(None),
        }
    }

    pub fn feed(&self, id: u32) -> Option<Arc<Feed>> {
        self.feeds.read().get(&id).cloned()
    }

    pub fn video_encoder(&self, id: u32) -> Option<Arc<VideoEncoder>> {
        self.video_encoders.read().get(&id).cloned()
    }

    pub fn audio_encoder(&self, id: u32) -> Option<Arc<AudioEncoder>> {
        self.audio_encoders.read().get(&id).cloned()
    }

    pub fn program(&self) -> Arc<Program> {
        self.program.read().clone()
    }

    pub fn set_program(&self, p: Program) {
        *self.program.write() = Arc::new(p);
    }

    /// Turns the privacy cut on or off from program sample `from`.
    pub fn set_privacy(&self, on: bool, from: u64) {
        let from = from & !(1 << 63);
        self.privacy.store(from | ((on as u64) << 63), Ordering::Release);
    }

    /// (on, from which program sample).
    pub fn privacy(&self) -> (bool, u64) {
        let v = self.privacy.load(Ordering::Acquire);
        (v >> 63 == 1, v & !(1 << 63))
    }

    /// Whether program sample `at` is covered.
    pub fn privacy_at(&self, at: u64) -> bool {
        let (on, from) = self.privacy();
        if at >= from { on } else { !on }
    }

    /// An engine thread's trouble, for the interface.
    pub fn fail(&self, why: String) {
        if !self.failed_once.swap(true, Ordering::AcqRel) {
            tracing::error!(%why, "Studio's engine");
            *self.failed.lock() = Some(why);
        }
    }

    /// Wakes the mix thread (a push to the master feed).
    pub fn wake_mixer(&self) {
        if let Some(t) = self.mixer.lock().as_ref() {
            t.unpark();
        }
    }
}

/// A caught panic's message.
pub fn panic_text(e: &Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "an unexpected error".into())
}

/// Open sessions by handle, for the hot-path calls.
static REGISTRY: RwLock<Option<HashMap<u64, Arc<Hot>>>> = RwLock::new(None);
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn register(hot: Arc<Hot>) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    REGISTRY.write().get_or_insert_with(HashMap::new).insert(id, hot);
    id
}

pub fn unregister(id: u64) {
    if let Some(m) = REGISTRY.write().as_mut() {
        m.remove(&id);
    }
}

/// The session behind `handle`; `None` once it's closed.
pub fn lookup(handle: u64) -> Option<Arc<Hot>> {
    REGISTRY.read().as_ref()?.get(&handle).cloned()
}
