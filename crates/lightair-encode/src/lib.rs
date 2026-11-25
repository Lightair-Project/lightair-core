//! Lightair's encoders and recording files.
//!
//! - [`apple`] (macOS and iOS): [`apple::VideoEncoder`], a VideoToolbox compression session
//!   (H.264 High, HEVC Main and Main 10, ProRes 422) whose own IOSurface pixel buffer pool the
//!   renderer draws into, and [`apple::AudioEncoder`], AAC-LC or Opus through AudioToolbox.
//! - `windows` (Windows): `windows::VideoEncoder`, the GPU's Media Foundation encoder (H.264,
//!   HEVC) fed the compositor's shared Direct3D 11 textures, and `windows::AudioEncoder`, AAC
//!   through Media Foundation or Opus in software.
//! - [`software`]: H.264 through OpenH264 on any platform (the `openh264` feature), and Opus
//!   (Windows).
//! - [`encoder`] (every platform): the settings and packets every backend shares, and the
//!   [`encoder::VideoEncode`] and [`encoder::AudioEncode`] traits a host holds any encoder by.
//! - [`mux`] (every platform): [`mux::Recorder`] writes encoded packets, never encoded again, to
//!   a crash-safe fragmented MP4 or MOV (finished as an ordinary progressive file, with
//!   chapters) or to Matroska.
//! - [`nal`] and [`audio_config`]: NAL unit layouts and the codecs' configuration records.
//!
//! Times are nanoseconds on the program clock throughout, as the engine keeps them.

pub mod audio_config;
pub mod encoder;
pub mod format;
pub mod mux;
pub mod nal;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
#[cfg(any(windows, feature = "openh264"))]
pub mod software;
#[cfg(windows)]
pub mod windows;

pub use encoder::{AudioEncode, VideoEncode};
pub use format::{AudioCodec, AudioFormat, Color, Packet, ProRes, TrackFormat, VideoCodec, VideoFormat};
pub use mux::{Container, Finished, Recorder, RecorderOptions, TrackSpec};
pub use nal::NalLayout;

/// What can go wrong encoding or writing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The platform can't do it (a codec, size or format it has no encoder for).
    #[error("{0}")]
    Unsupported(String),
    /// The platform's encoder failed (an `OSStatus` on Apple platforms, an `HRESULT` on
    /// Windows).
    #[error("{what} (status {status})")]
    Platform { what: &'static str, status: i32 },
    /// The encoder was taken away (iOS sends an app's sessions away in the background):
    /// open it again.
    #[error("the encoder was invalidated")]
    Invalidated,
    /// Data that doesn't parse (a parameter set, a packet).
    #[error("{0}")]
    Invalid(&'static str),
    /// A recording's track or packet doesn't fit its container.
    #[error("{0}")]
    Mux(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Whether this platform has [`apple::VideoEncoder`] and [`apple::AudioEncoder`].
pub const HAS_PLATFORM_ENCODERS: bool = cfg!(any(target_os = "macos", target_os = "ios"));
