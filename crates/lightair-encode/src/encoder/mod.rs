//! What every encoder shares, whichever backend makes the packets: the settings, the encoded
//! picture and sound packets, and two traits a host can hold any encoder through.
//!
//! On macOS and iOS these types are [`crate::apple`]'s own; elsewhere they are defined here
//! with the same fields and methods, so code that builds settings and reads packets is the same
//! on every platform. Each backend takes its pictures in its own form (a `CVPixelBuffer`, a
//! Direct3D 11 texture, bytes in memory), so feeding frames stays backend-specific; everything
//! after that goes through [`VideoEncode`] and [`AudioEncode`].

use crate::Error;
use crate::format::AudioFormat;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use crate::apple::{
    AudioSettings, EncodedAudio, EncodedVideo, Hardware, InputFormat, VideoOutput, VideoSettings, VideoSink,
};

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
mod types;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub use types::{
    AudioSettings, EncodedAudio, EncodedVideo, Hardware, InputFormat, VideoOutput, VideoSettings, VideoSink,
};

/// A video encoder once it is running, whatever its backend.
pub trait VideoEncode: Send + Sync {
    fn settings(&self) -> &VideoSettings;
    /// The next frame is a keyframe.
    fn request_keyframe(&self);
    /// The average bitrate from the next frame on (the ceiling follows it).
    fn set_bitrate(&self, kbps: u32);
    fn bitrate_kbps(&self) -> u32;
    /// Whether a GPU's media engine encodes (rather than software).
    fn using_hardware(&self) -> bool;
    /// Pictures encoded so far.
    fn packets(&self) -> u64;
    /// Encodes every frame still inside the encoder; their pictures reach the sink before this
    /// returns. Never call it from the sink.
    fn flush(&self);
}

/// A sound encoder: interleaved 32-bit float in, whole packets out.
pub trait AudioEncode: Send {
    /// The stream's format: the codec configuration a file or stream needs, and the priming.
    fn format(&self) -> &AudioFormat;
    /// Encodes `interleaved` frames starting at program time `pts_ns`; returns the packets they
    /// completed.
    fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error>;
    /// The last partial packet (padded with silence) and whatever the encoder still holds; the
    /// encoder takes no more after.
    fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error>;
}

/// One frame's nominal length in nanoseconds for `settings`.
#[allow(dead_code)]
pub(crate) fn frame_ns(settings: &VideoSettings) -> i64 {
    settings.fps_den.max(1) as i64 * 1_000_000_000 / settings.fps_num.max(1) as i64
}

/// The keyframe interval in frames for `settings` (at least 1).
#[allow(dead_code)]
pub(crate) fn keyframe_frames(settings: &VideoSettings) -> u32 {
    let fps = settings.fps_num.max(1) as f64 / settings.fps_den.max(1) as f64;
    (settings.keyframe_interval_s.max(0.1) as f64 * fps).round().max(1.0) as u32
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl VideoEncode for crate::apple::VideoEncoder {
    fn settings(&self) -> &VideoSettings {
        crate::apple::VideoEncoder::settings(self)
    }

    fn request_keyframe(&self) {
        crate::apple::VideoEncoder::request_keyframe(self)
    }

    fn set_bitrate(&self, kbps: u32) {
        crate::apple::VideoEncoder::set_bitrate(self, kbps)
    }

    fn bitrate_kbps(&self) -> u32 {
        crate::apple::VideoEncoder::bitrate_kbps(self)
    }

    fn using_hardware(&self) -> bool {
        crate::apple::VideoEncoder::using_hardware(self)
    }

    fn packets(&self) -> u64 {
        crate::apple::VideoEncoder::packets(self)
    }

    fn flush(&self) {
        crate::apple::VideoEncoder::flush(self)
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl AudioEncode for crate::apple::AudioEncoder {
    fn format(&self) -> &AudioFormat {
        crate::apple::AudioEncoder::format(self)
    }

    fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error> {
        crate::apple::AudioEncoder::encode(self, interleaved, pts_ns)
    }

    fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        crate::apple::AudioEncoder::flush(self)
    }
}
