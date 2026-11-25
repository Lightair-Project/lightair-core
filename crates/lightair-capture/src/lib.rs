//! Lightair's own captures of what's on the machine, without libobs: displays, windows and apps
//! (ScreenCaptureKit), cameras and capture cards (AVFoundation), and the sound the computer or
//! one app plays (Core Audio process taps).
//!
//! Each capture is a handle that runs while it's held and stops when it's dropped, so a host
//! starts one only while something shows it. Pictures arrive as [`Frame`]s: IOSurface-backed
//! `CVPixelBuffer`s stamped on the host clock (`mach_absolute_time` in nanoseconds, the engine's
//! `clock::host_now_ns`), handed over as they are, never copied. Sound arrives as interleaved
//! `f32` samples with the host time of their first frame.
//!
//! What lists the choices ([`screen::shareable`], [`camera::cameras`], [`sound::apps`]) never
//! starts a capture. Listing windows and apps through ScreenCaptureKit, opening a camera and
//! making a process tap each ask the user for access the first time (macOS's own prompts); the
//! `*_access` functions say whether access is already given without asking.
//!
//! macOS only for now; elsewhere the lists are empty and starting a capture fails with
//! [`Error::Unsupported`].

pub mod camera;
pub mod screen;
pub mod sound;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod other;

/// Why a capture couldn't be listed or started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The user hasn't allowed it (Screen Recording, Camera, System Audio Recording).
    #[error("not allowed: {0}")]
    Permission(String),
    /// The display, window, app or device isn't there (now).
    #[error("not found: {0}")]
    NotFound(String),
    /// Not on this platform or this version of it.
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
}

/// How a capture's pixels are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PixelFormat {
    /// 8-bit BGRA, sRGB: what the engine draws as it is (`lightair_render::texture::PixelBuffer`).
    #[default]
    Bgra,
    /// 8-bit 4:2:0, luma then interleaved chroma, video range (`420v`): what most cameras and
    /// capture cards make themselves, half BGRA's memory. The engine converts it on the GPU.
    Nv12Video,
    /// The same, full range (`420f`).
    Nv12Full,
}

impl PixelFormat {
    /// Its Core Video four-character code.
    pub fn fourcc(self) -> u32 {
        match self {
            PixelFormat::Bgra => u32::from_be_bytes(*b"BGRA"),
            PixelFormat::Nv12Video => u32::from_be_bytes(*b"420v"),
            PixelFormat::Nv12Full => u32::from_be_bytes(*b"420f"),
        }
    }

    /// The format with this Core Video four-character code, if it's one of these.
    pub fn from_fourcc(code: u32) -> Option<PixelFormat> {
        [PixelFormat::Bgra, PixelFormat::Nv12Video, PixelFormat::Nv12Full].into_iter().find(|f| f.fourcc() == code)
    }
}

/// A rectangle (its units are said where it's used).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One captured picture: a pixel buffer in an IOSurface, held (one reference) until the frame is
/// dropped or handed over with [`Frame::into_raw`].
pub struct Frame {
    #[cfg(target_os = "macos")]
    pub(crate) buffer: objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>,
    /// When it was captured, on the host clock (nanoseconds).
    pub host_ns: i64,
    /// Its size in pixels.
    pub size: (u32, u32),
    pub format: PixelFormat,
    /// Screen captures: the part of the picture that holds what was captured, as
    /// ScreenCaptureKit reports it (`SCStreamFrameInfoContentRect`, in points); `None` where
    /// the capture doesn't say. A window smaller than the picture it's drawn into fills only
    /// part of it: its pixels are `content`'s size ÷ `content_scale` × `scale`.
    pub content: Option<Rect>,
    /// Screen captures: how much the content was scaled to fit the picture
    /// (`SCStreamFrameInfoContentScale`).
    pub content_scale: Option<f64>,
    /// Screen captures: pixels to a point of the captured display (2 on a Retina display;
    /// `SCStreamFrameInfoScaleFactor`).
    pub scale: Option<f64>,
}

// A captured pixel buffer isn't written again once it's handed over, and Core Video counts its
// references thread-safely.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl Frame {
    /// The `CVPixelBufferRef`, still held by this frame.
    #[cfg(target_os = "macos")]
    pub fn pixel_buffer(&self) -> &objc2_core_video::CVPixelBuffer {
        &self.buffer
    }

    /// Gives up the frame's reference as a raw `CVPixelBufferRef` (+1), for what takes buffers
    /// over that way (`lightair_engine::ffi::push_pixel_buffer`,
    /// `lightair_render::texture::PixelBuffer::from_raw`).
    #[cfg(target_os = "macos")]
    pub fn into_raw(self) -> *mut std::ffi::c_void {
        objc2_core_foundation::CFRetained::into_raw(self.buffer).as_ptr().cast()
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("host_ns", &self.host_ns)
            .field("size", &self.size)
            .field("format", &self.format)
            .field("content", &self.content)
            .field("content_scale", &self.content_scale)
            .field("scale", &self.scale)
            .finish()
    }
}

/// Where a running picture capture delivers. Called on the capture's own serial queue, one call
/// at a time; keep it short (hand the frame to a ring and return).
pub trait VideoSink: Send + 'static {
    fn frame(&mut self, frame: Frame);
    /// The capture stopped by itself (the window closed, the display or device went away,
    /// access was taken back) and delivers nothing more. Not called when its handle is dropped.
    fn stopped(&mut self, reason: &str) {
        let _ = reason;
    }
}

impl<F: FnMut(Frame) + Send + 'static> VideoSink for F {
    fn frame(&mut self, frame: Frame) {
        self(frame)
    }
}

/// A block of captured sound.
#[derive(Debug, Clone, Copy)]
pub struct Sound<'a> {
    /// Interleaved samples, `channels` to a frame.
    pub samples: &'a [f32],
    pub channels: u32,
    /// Frames a second.
    pub rate: u32,
    /// When its first frame was captured, on the host clock (nanoseconds).
    pub host_ns: i64,
}

impl Sound<'_> {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
}

/// Where a running sound capture delivers. Called on Core Audio's real-time I/O thread: never
/// block, allocate or lock there (`lightair_engine::feeds::AudioFeed::push` is made for it).
pub trait SoundSink: Send + 'static {
    fn sound(&mut self, sound: &Sound<'_>);
    /// The capture stopped by itself and delivers nothing more (not called when its handle is
    /// dropped). Called on a thread of the capture's own, never the I/O thread.
    fn stopped(&mut self, reason: &str) {
        let _ = reason;
    }
}

impl<F: FnMut(&Sound<'_>) + Send + 'static> SoundSink for F {
    fn sound(&mut self, sound: &Sound<'_>) {
        self(sound)
    }
}
