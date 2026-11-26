//! The encoder settings and packets on platforms without the Apple encoders: the same fields
//! and methods as `apple`'s, so a host's code reads the same everywhere. Keep the two in step.

use std::sync::Arc;

use crate::format::{Color, H264Profile, Packet, VideoCodec, VideoFormat};
use crate::nal::NalLayout;

/// The pictures the encoder takes, and so the format the renderer's last pass writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputFormat {
    /// 8-bit BGRA: what the engine's encoder targets draw today (converted on the GPU when the
    /// encoder doesn't take it).
    Bgra,
    /// 8-bit 4:2:0 YCbCr, two planes: the hardware encoders' own format (no conversion).
    Nv12,
    /// 10-bit 4:2:0 YCbCr, two planes: for HEVC Main 10 and HDR.
    P010,
}

/// Whether a software encoder will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hardware {
    /// A GPU's media engine when there is one, else software.
    Prefer,
    /// A GPU's media engine or nothing.
    Require,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoSettings {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction.
    pub fps_num: u32,
    pub fps_den: u32,
    /// The average; ignored by ProRes (its rate follows the flavour and the picture size).
    pub bitrate_kbps: u32,
    /// A hard ceiling over one second; `None`: one and a half times the average.
    pub max_bitrate_kbps: Option<u32>,
    pub keyframe_interval_s: f32,
    /// Encode as fast as frames come (live) rather than for the best quality.
    pub realtime: bool,
    /// B-frames: better pictures for a file, a frame or two more delay, and decode times
    /// before presentation times. Off for streams.
    pub allow_reordering: bool,
    pub input: InputFormat,
    pub color: Color,
    pub hardware: Hardware,
    /// H.264's profile (HEVC's is the codec's `main10`). Apple's encoders follow it; the others
    /// keep their own.
    pub h264_profile: H264Profile,
    /// Constant quality instead of an average bitrate, 0.0 (smallest) to 1.0 (best), under
    /// `max_bitrate_kbps` when one is given; `None`: the bitrate. Apple's encoders follow it, and
    /// Media Foundation's where the encoder takes constant quality.
    pub quality: Option<f32>,
    /// Spatial adaptive quantisation on or off; `None`: the encoder's own choice. Apple's
    /// encoders follow it (macOS 15 and later).
    pub spatial_aq: Option<bool>,
}

impl VideoSettings {
    /// A live stream's: realtime, no reordering, a keyframe every 2 s, BGRA input.
    pub fn streaming(codec: VideoCodec, width: u32, height: u32, fps: u32, kbps: u32) -> Self {
        VideoSettings {
            codec,
            width,
            height,
            fps_num: fps.max(1),
            fps_den: 1,
            bitrate_kbps: kbps,
            max_bitrate_kbps: None,
            keyframe_interval_s: 2.0,
            realtime: true,
            allow_reordering: false,
            input: InputFormat::Bgra,
            color: Color::BT709,
            hardware: Hardware::Prefer,
            h264_profile: H264Profile::High,
            quality: None,
            spatial_aq: None,
        }
    }

    /// A recording's: still realtime (the program is live), reordering allowed, a keyframe
    /// every 2 s.
    pub fn recording(codec: VideoCodec, width: u32, height: u32, fps: u32, kbps: u32) -> Self {
        VideoSettings { allow_reordering: true, ..VideoSettings::streaming(codec, width, height, fps, kbps) }
    }
}

/// One encoded picture.
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    /// NAL units with 4-byte lengths (H.264, HEVC), without parameter sets.
    pub data: Vec<u8>,
    pub pts_ns: i64,
    /// Before `pts_ns` when pictures are reordered.
    pub dts_ns: i64,
    pub keyframe: bool,
    pub codec: VideoCodec,
    /// The stream's format: on the first picture and whenever it changes (with a keyframe).
    pub format: Option<Arc<VideoFormat>>,
}

impl EncodedVideo {
    /// [`NalLayout::LengthPrefixed`] for H.264 and HEVC, [`NalLayout::Raw`] for ProRes.
    pub fn layout(&self) -> NalLayout {
        if self.codec.is_nal() { NalLayout::LengthPrefixed } else { NalLayout::Raw }
    }

    /// As the recording writer takes it.
    pub fn packet(&self) -> Packet<'_> {
        Packet {
            data: &self.data,
            layout: self.layout(),
            pts_ns: self.pts_ns,
            dts_ns: self.dts_ns,
            keyframe: self.keyframe,
        }
    }
}

/// What the encoder sends back, on the encoder's own thread.
#[derive(Debug)]
pub enum VideoOutput {
    Packet(EncodedVideo),
    /// The encoder dropped the frame shown at `pts_ns`.
    Dropped {
        pts_ns: i64,
    },
    /// The encoder failed a frame; [`crate::Error::Invalidated`] when it was taken away (the
    /// GPU was removed or reset): open it again.
    Failed(crate::Error),
}

pub type VideoSink = Box<dyn FnMut(VideoOutput) + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    pub codec: crate::format::AudioCodec,
    /// 48 000 for both codecs in Lightair (Opus only takes 48 kHz here).
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u32,
    /// AAC: 96–320; Opus: 32–256.
    pub bitrate_kbps: u32,
}

impl AudioSettings {
    pub fn aac(kbps: u32) -> AudioSettings {
        AudioSettings { codec: crate::format::AudioCodec::Aac, sample_rate: 48_000, channels: 2, bitrate_kbps: kbps }
    }

    pub fn opus(kbps: u32) -> AudioSettings {
        AudioSettings { codec: crate::format::AudioCodec::Opus, sample_rate: 48_000, channels: 2, bitrate_kbps: kbps }
    }
}

/// One encoded packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudio {
    /// Raw AAC (no ADTS header) or an Opus packet.
    pub data: Vec<u8>,
    /// The program time of the packet's first input frame.
    pub pts_ns: i64,
    pub frames: u32,
}

impl EncodedAudio {
    pub fn packet(&self) -> Packet<'_> {
        Packet::audio(&self.data, self.pts_ns)
    }
}
