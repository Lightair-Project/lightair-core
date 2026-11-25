//! Cameras and capture cards (`lightair_model::kinds::CAMERA`): built-in cameras, USB (UVC)
//! cameras and capture cards, Continuity Camera and Desk View; their formats and frame rates;
//! captures of them; and word when one is plugged in or taken away.
//!
//! On macOS through AVFoundation. Listing devices and formats opens nothing (no camera light,
//! no access prompt); only a running [`CameraCapture`] does.

use crate::{Error, PixelFormat, VideoSink};

#[cfg(target_os = "macos")]
use crate::macos::camera as imp;
#[cfg(not(target_os = "macos"))]
use crate::other::camera as imp;

/// What kind of device a camera is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraKind {
    BuiltIn,
    /// USB, Thunderbolt or another external camera, or a capture card.
    External,
    /// An iPhone used as a camera.
    Continuity,
    /// An iPhone's ultra-wide camera looking down at the desk.
    DeskView,
    Other,
}

/// A camera or capture card.
#[derive(Debug, Clone, PartialEq)]
pub struct Camera {
    /// What settings store (`device`): stays the same for this device across reconnects.
    pub id: String,
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    pub kind: CameraKind,
    /// Its pictures and sound come as one (some capture cards): the sound isn't a separate
    /// audio device.
    pub muxed: bool,
    /// Another app has it open (it may still open here).
    pub in_use: bool,
    /// Its lid is closed or its privacy shutter on: it sends nothing.
    pub suspended: bool,
}

/// A range of frame rates a format runs at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FpsRange {
    pub min: f64,
    pub max: f64,
}

/// One of a camera's formats.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    /// Its place in the camera's list (what [`Config::format`] picks), valid while the camera
    /// stays connected.
    pub index: usize,
    /// Its size in pixels.
    pub size: (u32, u32),
    /// The format the device sends, as a four-character code (`420v`, `yuvs`, `jpeg`, …).
    pub fourcc: u32,
    pub fps: Vec<FpsRange>,
}

impl Format {
    /// The fastest frame rate it runs at.
    pub fn max_fps(&self) -> f64 {
        self.fps.iter().map(|r| r.max).fold(0.0, f64::max)
    }

    /// Whether it runs at `fps` (within a hundredth).
    pub fn runs_at(&self, fps: f64) -> bool {
        self.fps.iter().any(|r| fps >= r.min - 0.01 && fps <= r.max + 0.01)
    }

    /// Its four-character code as text ("420v").
    pub fn fourcc_text(&self) -> String {
        self.fourcc
            .to_be_bytes()
            .iter()
            .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
            .collect()
    }
}

/// Whether the user has already allowed the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Not asked yet: opening a camera asks.
    NotAsked,
    Allowed,
    Denied,
    /// Not the user's to give (parental controls, a management profile).
    Restricted,
}

/// Whether the camera is allowed (asks nothing).
pub fn access() -> Access {
    imp::access()
}

/// The cameras and capture cards connected now.
pub fn cameras() -> Vec<Camera> {
    imp::cameras()
}

/// A camera's formats, as the device lists them.
pub fn formats(id: &str) -> Result<Vec<Format>, Error> {
    imp::formats(id)
}

/// How to capture.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    /// A format by its index; `None` to pick one by `height` and `fps`.
    pub format: Option<usize>,
    /// The short side wanted (720, 1080, 2160): the format closest to it, larger preferred
    /// to smaller; `None` for the camera's own best.
    pub height: Option<u32>,
    /// The frame rate wanted: the format's nearest; `None` for the fastest it runs at.
    pub fps: Option<f64>,
    /// What the frames are converted to (by the capture, on the device's own hardware where it
    /// can); BGRA by default.
    pub pixel_format: PixelFormat,
}

/// Picks the format for `config` from `formats`: `config.format` if it's there; else the one
/// whose short side is nearest `config.height` (at or above it first), running at `config.fps`
/// if any does, the largest and fastest otherwise.
pub fn choose(formats: &[Format], config: &Config) -> Option<usize> {
    if let Some(i) = config.format {
        return formats.iter().position(|f| f.index == i);
    }
    let short = |f: &Format| f.size.0.min(f.size.1);
    let fps_ok = |f: &Format| config.fps.is_none_or(|fps| f.runs_at(fps));
    let pool: Vec<usize> = {
        let ok: Vec<usize> = (0..formats.len()).filter(|&i| fps_ok(&formats[i])).collect();
        if ok.is_empty() { (0..formats.len()).collect() } else { ok }
    };
    let key = |i: &usize| {
        let f = &formats[*i];
        let s = short(f);
        // nearest to the height asked for (at or above it before below it), then the largest,
        // then the fastest; formats the engine draws as they are (420v, 420f) before others
        let distance = match config.height {
            Some(h) if s >= h => (0u8, s - h),
            Some(h) => (1, h - s),
            None => (0, u32::MAX - s),
        };
        let native = PixelFormat::from_fourcc(f.fourcc).is_some();
        (
            distance,
            std::cmp::Reverse(f.size.0 as u64 * f.size.1 as u64),
            std::cmp::Reverse((f.max_fps() * 100.0) as u64),
            !native,
        )
    };
    pool.into_iter().min_by_key(key)
}

/// A running capture of a camera. Captures until dropped; the camera's light goes off then.
pub struct CameraCapture {
    inner: imp::Session,
}

impl CameraCapture {
    /// Opens camera `id` and starts capturing into `sink`. Asks for camera access the first
    /// time and fails with [`Error::Permission`] while it's denied. Blocks until the camera
    /// runs: call it off the main thread.
    pub fn start(id: &str, config: &Config, sink: impl VideoSink) -> Result<CameraCapture, Error> {
        Ok(CameraCapture { inner: imp::Session::start(id, config, Box::new(sink))? })
    }

    /// The format it runs in.
    pub fn format(&self) -> &Format {
        self.inner.format()
    }

    /// The frame rate it runs at.
    pub fn fps(&self) -> f64 {
        self.inner.fps()
    }
}

impl std::fmt::Debug for CameraCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraCapture").field("format", self.format()).field("fps", &self.fps()).finish()
    }
}

/// A camera came or went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Connected { id: String, name: String },
    Disconnected { id: String, name: String },
}

/// Hears cameras come and go until dropped.
pub struct DeviceWatch {
    _inner: imp::Watch,
}

/// Calls `f` whenever a camera or capture card is connected or disconnected (on a queue of the
/// system's; keep it short).
pub fn watch(f: impl Fn(DeviceEvent) + Send + Sync + 'static) -> DeviceWatch {
    DeviceWatch { _inner: imp::Watch::new(Box::new(f)) }
}
