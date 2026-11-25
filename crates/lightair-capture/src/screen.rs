//! Displays, windows and apps (`lightair_model::kinds::SCREEN`): what there is to capture, and
//! captures of them.
//!
//! On macOS through ScreenCaptureKit. A running capture has the window server compose a copy of
//! what it captures every frame, so a host keeps one running only while the capture is shown
//! (on air, in a preview, in a thumbnail) and drops it shortly after nothing does.

use crate::{Error, PixelFormat, VideoSink};

#[cfg(target_os = "macos")]
use crate::macos::screen as imp;
#[cfg(not(target_os = "macos"))]
use crate::other::screen as imp;

/// A display.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    /// The display's id while it's connected (`CGDirectDisplayID`).
    pub id: u32,
    /// What settings store (`display`): stays the same across reconnects and restarts.
    pub uuid: String,
    /// "DELL U2720Q": the system's name for it when listed on the main thread (AppKit's
    /// screens are the main thread's); "Built-in Display" or "Display 2" otherwise.
    pub name: String,
    /// Its size in points.
    pub size: (u32, u32),
    /// Its size in pixels, as captured at full resolution.
    pub pixel_size: (u32, u32),
    /// Where it is in the desktop's space, in points (the main display at 0,0).
    pub origin: (f64, f64),
    /// The display with the menu bar.
    pub main: bool,
    pub built_in: bool,
}

/// An app with windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// What settings store (`app`).
    pub bundle: String,
    pub name: String,
    pub pid: i32,
}

/// A window.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// What settings store (`window`): the window's id, as long as it's open.
    pub id: u32,
    pub title: String,
    /// The app it belongs to (its bundle id), when it has one.
    pub app: Option<String>,
    pub app_name: Option<String>,
    pub pid: Option<i32>,
    /// Its size in points.
    pub size: (u32, u32),
    pub on_screen: bool,
    /// The window server's layer: 0 for ordinary windows; menus, the Dock and overlays above.
    pub layer: i64,
}

impl Window {
    /// An ordinary window someone would pick: in the normal layer, with a title or an app, not
    /// tiny (status items and invisible helpers are).
    pub fn is_ordinary(&self) -> bool {
        self.layer == 0 && (self.app.is_some() || !self.title.is_empty()) && self.size.0 >= 40 && self.size.1 >= 40
    }
}

/// Everything there is to capture now.
#[derive(Debug, Clone, Default)]
pub struct Shareable {
    pub displays: Vec<Display>,
    /// On-screen windows, front to back, Lightair's own left out.
    pub windows: Vec<Window>,
    /// Apps with windows, Lightair left out.
    pub apps: Vec<App>,
}

/// Whether the user has already allowed screen recording (asks nothing).
pub fn access() -> bool {
    imp::access()
}

/// The displays, without asking for screen recording access (Core Graphics; their names from
/// AppKit when called on the main thread). Cheap: fine for a picker to call as it opens.
pub fn displays() -> Vec<Display> {
    imp::displays()
}

/// The displays, windows and apps there are now. Asks for screen recording access the first
/// time (macOS's prompt) and fails with [`Error::Permission`] until it's given. Blocks until
/// the window server answers: call it off the main thread and off any real-time one.
pub fn shareable() -> Result<Shareable, Error> {
    imp::shareable()
}

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A whole display, by its UUID (or its `CGDirectDisplayID` as decimal text).
    Display(String),
    /// One window, by its id; it's followed when it moves, and the capture stops when it
    /// closes.
    Window(u32),
    /// Every window of one app (by bundle id), on the display that holds most of them (or the
    /// one given), as they're arranged there.
    App { bundle: String, display: Option<String> },
}

/// How to capture.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The picture's size in pixels; `None` for the target's own at full resolution. The
    /// content is scaled to fit, its shape kept.
    pub size: Option<(u32, u32)>,
    /// Frames a second at most (the capture sends fewer while nothing on screen changes).
    pub fps: u32,
    pub format: PixelFormat,
    /// Draw the pointer into the picture (Lightair draws its own, so usually not).
    pub cursor: bool,
    /// Leave Lightair's own windows out of a display capture.
    pub exclude_own: bool,
    /// Pixel buffers the capture may have in flight (3–8): more lets the consumer hold frames
    /// longer before the capture stalls.
    pub queue_depth: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { size: None, fps: 60, format: PixelFormat::Bgra, cursor: false, exclude_own: true, queue_depth: 5 }
    }
}

/// A running capture of a display, window or app. Captures until dropped.
pub struct ScreenCapture {
    inner: imp::Stream,
}

impl ScreenCapture {
    /// Starts capturing `target` into `sink`; returns once frames are on their way (or the
    /// window server said no). Asks for screen recording access the first time. Blocks: call it
    /// off the main thread.
    pub fn start(target: &Target, config: &Config, sink: impl VideoSink) -> Result<ScreenCapture, Error> {
        Ok(ScreenCapture { inner: imp::Stream::start(target, config, Box::new(sink))? })
    }

    /// Changes size, frame rate, format, pointer or queue depth without stopping (the target
    /// and `exclude_own` stay as started).
    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        self.inner.reconfigure(config)
    }

    /// The picture's size in pixels now.
    pub fn size(&self) -> (u32, u32) {
        self.inner.size()
    }
}

impl std::fmt::Debug for ScreenCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenCapture").field("size", &self.size()).finish()
    }
}
