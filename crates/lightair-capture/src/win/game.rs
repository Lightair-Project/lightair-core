//! The Game source (Windows): the streamer picks a game once, and the capture chooses how to
//! capture it, switching by itself as the game changes mode. Nothing is ever injected into the
//! game: no hook, no layer, no code in its process.
//!
//! 1. It watches how the game presents ([`PresentMode`]), from Windows' event tracing, without
//!    touching the game. That needs administrator rights or the "Performance Log Users" group;
//!    without them ([`Detection::NeedsRights`]) it judges by whether frames arrive.
//! 2. Composed or flip model (nearly every game): Windows Graphics Capture of the game's window.
//! 3. Legacy flip (exclusive fullscreen): Desktop Duplication of the game's display, cropped to
//!    the game. When the cause is the game's "Disable fullscreen optimizations" flag for the
//!    current user, [`Status::offers_fix`] says the Inspector can offer to take it off
//!    ([`fso::plan`], then [`fso::apply`] when the user clicks): the game goes back to step 2 at
//!    its next start.
//! 4. Nothing arrives (a game that forces exclusive fullscreen and opts out of capture): the
//!    source shows what it last had, and [`Status::message`] says why and what to do.
//!
//! The switch takes a moment; the engine keeps showing the last frame meanwhile.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::duplication;
use super::etw::{Presents, Refused};
use super::screen::{Listed, frame_of, main_window_of, monitor_of, monitors, top_windows};
use super::wgc;
use super::{Deliver, Sink, contract, host_now_ns, same_exe, stem};
use crate::screen::Config;
use crate::{Error, Frame, VideoSink};

pub use super::etw::PresentMode;
pub use super::fso;

/// A game that's running, as a picker lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    /// What settings store: the game's executable, as a full path.
    pub exe: String,
    /// Its executable's name without the extension.
    pub name: String,
    pub pid: u32,
    /// Its largest window: its id (`HWND`), title and size in pixels.
    pub window: u32,
    pub title: String,
    pub size: (u32, u32),
}

/// What's running with a window someone might capture, one entry an executable (its largest
/// window), Lightair left out.
pub fn running() -> Vec<Game> {
    let mut games: Vec<(Game, u64)> = Vec::new();
    for listed in top_windows() {
        let (Some(exe), Some(pid)) = (listed.exe.clone(), listed.window.pid) else { continue };
        let area = listed.window.size.0 as u64 * listed.window.size.1 as u64;
        let game = Game {
            name: stem(&exe),
            exe,
            pid: pid as u32,
            window: listed.window.id,
            title: listed.window.title.clone(),
            size: listed.window.size,
        };
        match games.iter_mut().find(|(g, _)| same_exe(&g.exe, &game.exe)) {
            Some((existing, a)) if area > *a => {
                *existing = game;
                *a = area;
            }
            Some(_) => {}
            None => games.push((game, area)),
        }
    }
    games.into_iter().map(|(g, _)| g).collect()
}

/// How the game is being captured now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The game isn't running (or has no window yet).
    Waiting,
    /// Windows Graphics Capture of its window.
    Window,
    /// Desktop Duplication of the display it's on (its device path, as `screen::Display::uuid`),
    /// cropped to the game.
    Display(String),
    /// Nothing gets its frames: see [`Status::message`].
    Nothing,
}

/// How the capture knows the game's present mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// From Windows' event tracing.
    Events,
    /// Event tracing needs administrator rights or the "Performance Log Users" group: the mode
    /// is judged by whether frames arrive.
    NeedsRights,
    /// Event tracing couldn't start for another reason (said), judged by frames as above.
    Unavailable(String),
}

/// What the Inspector shows for a Game source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub route: Route,
    /// The game's present mode, as last seen ([`PresentMode::Unknown`] without events).
    pub present: PresentMode,
    pub detection: Detection,
    /// "Disable fullscreen optimizations" is ticked for this game for the current user, which
    /// Lightair can undo ([`fso::plan`]).
    pub fso_off_for_user: bool,
    /// The same flag set for every user, which only an administrator can undo
    /// ([`fso::machine_location`] says where).
    pub fso_off_for_machine: bool,
    /// The game is in exclusive fullscreen on Windows 11 24H2 or later with more than one
    /// display: every capture gets it at about 3 frames a second.
    pub slow_with_displays: bool,
    /// The game's executable.
    pub exe: String,
}

impl Status {
    /// Whether to offer "Let Windows optimise this game so Lightair can capture it".
    pub fn offers_fix(&self) -> bool {
        self.fso_off_for_user && matches!(self.route, Route::Display(_) | Route::Nothing)
    }

    /// What to tell the user, in plain words; `None` when all is well.
    pub fn message(&self) -> Option<String> {
        let name = stem(&self.exe);
        let mut parts: Vec<String> = Vec::new();
        match &self.route {
            Route::Waiting => parts.push(format!("Waiting for {name} to start.")),
            Route::Window => {}
            Route::Display(_) => parts.push(
                "This game runs in exclusive fullscreen, so Lightair captures the whole display it's on: \
                 pop-ups and notifications there show too. Windows' Do Not Disturb keeps notifications away."
                    .into(),
            ),
            Route::Nothing => parts.push(
                "This game runs in exclusive fullscreen, which Windows doesn't let other apps capture. \
                 Switch it to borderless in its video settings."
                    .into(),
            ),
        }
        if matches!(self.route, Route::Display(_) | Route::Nothing) {
            if self.fso_off_for_user {
                parts.push(
                    "Fullscreen optimisations are turned off for this game. Let Windows optimise it and Lightair \
                     captures just the game from its next start."
                        .into(),
                );
            } else if self.fso_off_for_machine {
                parts.push(format!(
                    "Fullscreen optimisations are turned off for this game for every user, which needs an \
                     administrator to change: {}.",
                    fso::machine_location(&self.exe)
                ));
            }
        }
        if self.slow_with_displays {
            parts.push(
                "With more than one display in Extended mode, Windows 11 24H2 and later let other apps capture a \
                 game in exclusive fullscreen at only about 3 frames a second. Switch the game to borderless, or \
                 set the displays to Duplicate."
                    .into(),
            );
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

// ------------------------------------------------------------------ the capture

/// Hands one route's frames to the host's sink and notes when they came.
struct Relay {
    sink: Arc<Sink>,
    last_ns: AtomicI64,
    ended: AtomicBool,
}

impl Relay {
    fn new(sink: &Arc<Sink>) -> Arc<Relay> {
        Arc::new(Relay { sink: sink.clone(), last_ns: AtomicI64::new(0), ended: AtomicBool::new(false) })
    }

    /// Frames came within `within` (or, if none ever did, the route started within it).
    fn delivering(&self, within: Duration, started: Instant) -> bool {
        let last = self.last_ns.load(Ordering::Relaxed);
        if last == 0 {
            return started.elapsed() < within;
        }
        host_now_ns() - last < within.as_nanos() as i64
    }
}

impl Deliver for Relay {
    fn frame(&self, frame: Frame) {
        self.last_ns.store(host_now_ns(), Ordering::Relaxed);
        self.sink.frame(frame);
    }

    // a route ending isn't the source ending: the controller picks another
    fn stopped(&self, _: &str) {
        self.ended.store(true, Ordering::Relaxed);
    }
}

enum Active {
    None,
    Window { hwnd: HWND, capture: wgc::Capture, relay: Arc<Relay>, started: Instant },
    Display { uuid: String, capture: duplication::Capture, relay: Arc<Relay>, started: Instant },
}

impl Active {
    fn relay(&self) -> Option<(&Arc<Relay>, Instant)> {
        match self {
            Active::None => None,
            Active::Window { relay, started, .. } | Active::Display { relay, started, .. } => Some((relay, *started)),
        }
    }
}

struct Shared {
    exe: String,
    config: Mutex<Config>,
    status: Mutex<Status>,
    size: Mutex<(u32, u32)>,
    stop: AtomicBool,
}

/// A running capture of a game. Captures until dropped.
pub struct GameCapture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl GameCapture {
    /// Starts capturing the game at `exe` (its full path) into `sink`, now or whenever it
    /// starts. Returns at once; [`GameCapture::status`] says how it's going.
    pub fn start(exe: &str, config: &Config, sink: impl VideoSink) -> Result<GameCapture, Error> {
        let sink = Sink::new(Box::new(sink));
        let shared = Arc::new(Shared {
            exe: exe.to_string(),
            config: Mutex::new(config.clone()),
            status: Mutex::new(Status {
                route: Route::Waiting,
                present: PresentMode::Unknown,
                detection: Detection::Events,
                fso_off_for_user: false,
                fso_off_for_machine: false,
                slow_with_displays: false,
                exe: exe.to_string(),
            }),
            size: Mutex::new((0, 0)),
            stop: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-game".into())
            .spawn(move || control(thread_shared, sink))
            .map_err(|e| Error::Failed(format!("starting the game capture: {e}")))?;
        Ok(GameCapture { shared, thread: Some(thread) })
    }

    /// How the game is being captured, and what to tell the user.
    pub fn status(&self) -> Status {
        self.shared.status.lock().clone()
    }

    /// The change that would turn the game's fullscreen optimisations back on for the current
    /// user, for the Inspector's button (made with [`fso::apply`]).
    pub fn fix(&self) -> Option<fso::Change> {
        fso::plan(&self.shared.exe)
    }

    /// Changes size, frame rate, format or queue depth from the next route started (and the
    /// frame rate and size of the one running).
    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        *self.shared.config.lock() = config.clone();
        Ok(())
    }

    /// The picture's size now.
    pub fn size(&self) -> (u32, u32) {
        *self.shared.size.lock()
    }
}

impl std::fmt::Debug for GameCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameCapture").field("status", &self.status()).finish()
    }
}

impl Drop for GameCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How often the controller looks at the game.
const TICK: Duration = Duration::from_millis(500);
/// Without frames from a fullscreen, foreground game's window for this long, its frames are
/// taken not to reach the compositor (judged so only without event tracing).
const SILENT_WINDOW: Duration = Duration::from_secs(2);
/// Without frames from Desktop Duplication for this long, nothing captures the game.
const SILENT_DISPLAY: Duration = Duration::from_secs(3);

/// Whether `frame` covers `monitor` (an exclusive or borderless fullscreen game's window).
fn covers(frame: &RECT, monitor: &RECT) -> bool {
    let near = |a: i32, b: i32| (a - b).abs() <= 2;
    near(frame.left, monitor.left)
        && near(frame.top, monitor.top)
        && near(frame.right, monitor.right)
        && near(frame.bottom, monitor.bottom)
}

/// The controller: finds the game, decides the route, starts and switches captures.
fn control(shared: Arc<Shared>, sink: Arc<Sink>) {
    super::com();
    let presents: Result<Arc<Presents>, Refused> = Presents::shared();
    let detection = match &presents {
        Ok(_) => Detection::Events,
        Err(Refused::Rights) => Detection::NeedsRights,
        Err(Refused::Failed(why)) => Detection::Unavailable(why.clone()),
    };
    let presents = presents.ok();
    let mut active = Active::None;
    let mut inferred_legacy = false;
    let mut flags = fso::read(&shared.exe);
    let mut flags_read = Instant::now();
    let mut started_display_failed = false;

    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        let config = shared.config.lock().clone();
        let Some(Listed { hwnd, window, .. }) = main_window_of(&shared.exe) else {
            active = Active::None;
            inferred_legacy = false;
            let mut status = shared.status.lock();
            status.route = Route::Waiting;
            status.present = PresentMode::Unknown;
            status.detection = detection.clone();
            continue;
        };
        let pid = window.pid.unwrap_or(0) as u32;
        let present = match &presents {
            Some(p) if p.running() => {
                p.watch(pid, &[]);
                p.mode(pid)
            }
            _ => PresentMode::Unknown,
        };
        let frame = frame_of(hwnd).unwrap_or_default();
        let monitor = monitor_of(hwnd);
        let fullscreen = monitor.as_ref().is_some_and(|m| covers(&frame, &m.rect));
        let foreground = unsafe { GetForegroundWindow() } == hwnd;

        // is the game in legacy flip?
        let legacy = if presents.is_some() && present != PresentMode::Unknown {
            inferred_legacy = false;
            present == PresentMode::LegacyFlip
        } else {
            // judged by frames: a fullscreen foreground window whose capture is silent
            match &active {
                Active::Window { relay, started, .. } if fullscreen && foreground => {
                    if !relay.delivering(SILENT_WINDOW, *started) {
                        inferred_legacy = true;
                    }
                }
                _ if !(fullscreen && foreground) => inferred_legacy = false,
                _ => {}
            }
            inferred_legacy
        };

        // where it should be captured
        let wanted_display = if legacy { monitor.clone() } else { None };
        let switch = match (&active, &wanted_display) {
            (Active::None, _) => true,
            (Active::Window { hwnd: h, relay, .. }, None) => *h != hwnd || relay.ended.load(Ordering::Relaxed),
            (Active::Window { .. }, Some(_)) => true,
            (Active::Display { .. }, None) => true,
            (Active::Display { uuid, relay, .. }, Some(m)) => {
                *uuid != m.display.uuid || relay.ended.load(Ordering::Relaxed)
            }
        };
        if switch {
            // the new route starts before the old one stops, so frames keep coming
            let started =
                match &wanted_display {
                    Some(m) => {
                        let relay = Relay::new(&sink);
                        duplication::Capture::start(m, Some(frame), &config, relay.clone()).map(|capture| {
                            Active::Display { uuid: m.display.uuid.clone(), capture, relay, started: Instant::now() }
                        })
                    }
                    None => {
                        let relay = Relay::new(&sink);
                        wgc::Capture::window(hwnd, &config, relay.clone()).map(|capture| Active::Window {
                            hwnd,
                            capture,
                            relay,
                            started: Instant::now(),
                        })
                    }
                };
            match started {
                Ok(next) => {
                    active = next;
                    started_display_failed = false;
                }
                Err(e) => {
                    tracing::info!("capture: game {}: {e}", shared.exe);
                    started_display_failed = wanted_display.is_some();
                    if wanted_display.is_some() {
                        active = Active::None;
                    }
                }
            }
        }
        match &active {
            Active::Display { capture, .. } => {
                capture.set_crop(Some(frame));
                let _ = capture.reconfigure(&config);
                *shared.size.lock() = capture.size();
            }
            Active::Window { capture, .. } => {
                let _ = capture.reconfigure(&config);
                *shared.size.lock() = capture.size();
            }
            Active::None => {}
        }

        if flags_read.elapsed() > Duration::from_secs(5) {
            flags = fso::read(&shared.exe);
            flags_read = Instant::now();
        }
        let route = match &active {
            Active::Window { .. } => Route::Window,
            Active::Display { uuid, .. } => {
                let silent = active.relay().is_some_and(|(r, s)| !r.delivering(SILENT_DISPLAY, s));
                if silent { Route::Nothing } else { Route::Display(uuid.clone()) }
            }
            Active::None if started_display_failed => Route::Nothing,
            Active::None => Route::Waiting,
        };
        let mut status = shared.status.lock();
        status.route = route;
        status.present = if present == PresentMode::Unknown && legacy { PresentMode::LegacyFlip } else { present };
        status.detection = detection.clone();
        status.fso_off_for_user = flags.off_for_user();
        status.fso_off_for_machine = flags.off_for_machine();
        status.slow_with_displays = legacy && contract(19) && monitors().len() >= 2;
    }
}

/// The application filling its monitor in front now (the Game source with no game chosen:
/// "any full-screen application"): the foreground window's game, when the window covers its
/// whole monitor. Never the desktop, the taskbar or Lightair.
pub fn fullscreen() -> Option<Game> {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowThreadProcessId};
    let window = unsafe { GetForegroundWindow() };
    if window.is_invalid() {
        return None;
    }
    let mut class = [0u16; 64];
    let n = unsafe { GetClassNameW(window, &mut class) }.max(0) as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if shell_window(&class) {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    let frame = frame_of(window)?;
    if !fills(&frame, &monitor_of(window)?.rect) {
        return None;
    }
    running().into_iter().find(|g| g.pid == pid)
}

/// Whether a window of `class` is the shell's: the desktop or a taskbar (never a game).
fn shell_window(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

/// Whether a window's `frame` covers the whole of its `monitor` (full screen, borderless or
/// exclusive; a maximised window leaves the taskbar out).
fn fills(frame: &RECT, monitor: &RECT) -> bool {
    frame.left <= monitor.left && frame.top <= monitor.top && frame.right >= monitor.right && frame.bottom >= monitor.bottom
}

/// How often [`FullscreenCapture`] looks for the full-screen application.
const FULLSCREEN_TICK: Duration = Duration::from_millis(500);

/// A Game source with no game chosen: captures whatever application is full screen in front,
/// moving to another when it takes over (the last one stays while none is, say while Lightair
/// itself is in front). Each is captured as a chosen game is ([`GameCapture`]). Captures until
/// dropped.
pub struct FullscreenCapture {
    shared: Arc<Fullscreen>,
    thread: Option<JoinHandle<()>>,
}

struct Fullscreen {
    config: Mutex<Config>,
    current: Mutex<Option<GameCapture>>,
    stop: AtomicBool,
}

/// The one sink every game captured in turn delivers to.
struct Forward(Arc<Mutex<Box<dyn VideoSink>>>);

impl VideoSink for Forward {
    fn frame(&mut self, frame: Frame) {
        self.0.lock().frame(frame);
    }

    // a game that went away: the next full-screen application is captured when one comes
    fn stopped(&mut self, _reason: &str) {}
}

impl FullscreenCapture {
    /// Starts looking for a full-screen application, captured into `sink`. Returns at once.
    pub fn start(config: &Config, sink: impl VideoSink) -> Result<FullscreenCapture, Error> {
        let sink: Arc<Mutex<Box<dyn VideoSink>>> = Arc::new(Mutex::new(Box::new(sink)));
        let shared =
            Arc::new(Fullscreen { config: Mutex::new(config.clone()), current: Mutex::new(None), stop: AtomicBool::new(false) });
        let watch = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-fullscreen".into())
            .spawn(move || {
                while !watch.stop.load(Ordering::Relaxed) {
                    if let Some(game) = fullscreen() {
                        let same = watch.current.lock().as_ref().is_some_and(|c| same_exe(&c.shared.exe, &game.exe));
                        if !same {
                            let config = watch.config.lock().clone();
                            match GameCapture::start(&game.exe, &config, Forward(sink.clone())) {
                                Ok(c) => {
                                    tracing::info!(game = game.name, "game capture: the full-screen application now");
                                    // (the one before stops as it's dropped)
                                    *watch.current.lock() = Some(c);
                                }
                                Err(e) => tracing::warn!(%e, game = game.name, "game capture: the full-screen application wasn't captured"),
                            }
                        }
                    }
                    std::thread::sleep(FULLSCREEN_TICK);
                }
            })
            .map_err(|e| Error::Failed(format!("starting the full-screen capture: {e}")))?;
        Ok(FullscreenCapture { shared, thread: Some(thread) })
    }

    /// How the application in front is being captured (waiting while there's none yet).
    pub fn status(&self) -> Status {
        match self.shared.current.lock().as_ref() {
            Some(c) => c.status(),
            None => Status {
                route: Route::Waiting,
                present: PresentMode::Unknown,
                detection: Detection::Events,
                fso_off_for_user: false,
                fso_off_for_machine: false,
                slow_with_displays: false,
                exe: String::new(),
            },
        }
    }

    /// The fix [`GameCapture::fix`] offers for the application captured now.
    pub fn fix(&self) -> Option<fso::Change> {
        self.shared.current.lock().as_ref().and_then(GameCapture::fix)
    }

    /// As [`GameCapture::reconfigure`], for this application and the ones after it.
    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        *self.shared.config.lock() = config.clone();
        match self.shared.current.lock().as_ref() {
            Some(c) => c.reconfigure(config),
            None => Ok(()),
        }
    }

    /// The picture's size now.
    pub fn size(&self) -> (u32, u32) {
        self.shared.current.lock().as_ref().map_or((0, 0), GameCapture::size)
    }
}

impl Drop for FullscreenCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.shared.current.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    /// A window covering its whole monitor is full screen (exactly, or overhanging it as a
    /// borderless window's frame can); a maximised one (the taskbar left out) or a smaller one
    /// isn't. On a second monitor, its own rectangle counts.
    #[test]
    fn a_window_is_full_screen_when_it_covers_its_monitor() {
        let monitor = rect(0, 0, 1920, 1080);
        assert!(fills(&rect(0, 0, 1920, 1080), &monitor));
        assert!(fills(&rect(-8, -8, 1928, 1088), &monitor), "overhanging the edges");
        assert!(!fills(&rect(0, 0, 1920, 1040), &monitor), "maximised above the taskbar");
        assert!(!fills(&rect(-8, -8, 1928, 1048), &monitor), "maximised with its frame");
        assert!(!fills(&rect(100, 100, 1280, 820), &monitor), "a window");
        assert!(!fills(&rect(1, 0, 1920, 1080), &monitor), "a pixel short");
        let second = rect(1920, -200, 4480, 1240);
        assert!(fills(&rect(1920, -200, 4480, 1240), &second));
        assert!(!fills(&monitor, &second), "full screen on the other monitor");
    }

    /// The desktop and the taskbars are never the full-screen application, even when in front
    /// and covering the monitor.
    #[test]
    fn the_shell_is_never_a_game() {
        for class in ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
            assert!(shell_window(class), "{class}");
        }
        for class in ["UnityWndClass", "UnrealWindow", "SDL_app", "Chrome_WidgetWin_1", "", "progman"] {
            assert!(!shell_window(class), "{class:?}");
        }
    }

    /// Whatever is in front while the tests run (a terminal, an editor: this process's own window
    /// never counts), the answer is a running game or none, never this process.
    #[test]
    fn the_full_screen_application_is_never_this_process() {
        if let Some(game) = fullscreen() {
            assert_ne!(game.pid, std::process::id());
        }
    }
}
