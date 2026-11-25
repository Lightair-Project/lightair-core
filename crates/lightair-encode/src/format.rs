//! What encoded streams are: codecs, their decoder configuration, colour, and packets.

use crate::nal::{self, NalLayout};

/// The four ProRes 422 flavours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProRes {
    Proxy,
    Lt,
    Standard,
    Hq,
}

impl ProRes {
    /// The sample entry's four-character code (`apco`, `apcs`, `apcn`, `apch`).
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            ProRes::Proxy => *b"apco",
            ProRes::Lt => *b"apcs",
            ProRes::Standard => *b"apcn",
            ProRes::Hq => *b"apch",
        }
    }
}

/// The H.264 profile an encoder makes: High by default; Main or Baseline for decoders that
/// take only those (a platform's plan can ask for them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum H264Profile {
    Baseline,
    Main,
    #[default]
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264, High profile unless the settings say another ([`H264Profile`]).
    H264,
    /// HEVC Main (8-bit) or Main 10.
    Hevc {
        main10: bool,
    },
    ProRes(ProRes),
}

impl VideoCodec {
    /// H.264 or HEVC: pictures are NAL units with parameter sets.
    pub fn is_nal(self) -> bool {
        matches!(self, VideoCodec::H264 | VideoCodec::Hevc { .. })
    }

    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc { main10: false } => "HEVC",
            VideoCodec::Hevc { main10: true } => "HEVC Main 10",
            VideoCodec::ProRes(ProRes::Proxy) => "ProRes 422 Proxy",
            VideoCodec::ProRes(ProRes::Lt) => "ProRes 422 LT",
            VideoCodec::ProRes(ProRes::Standard) => "ProRes 422",
            VideoCodec::ProRes(ProRes::Hq) => "ProRes 422 HQ",
        }
    }
}

/// Colour as ISO/IEC 23091-2 code points (the values MP4's `colr`, Matroska's `Colour` and
/// the codecs' VUI all use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

impl Color {
    /// HD video: BT.709 primaries, transfer and matrix, limited range.
    pub const BT709: Color = Color { primaries: 1, transfer: 1, matrix: 1, full_range: false };
    /// HDR10: BT.2020 primaries, PQ, BT.2020 non-constant luminance, limited range.
    pub const BT2100_PQ: Color = Color { primaries: 9, transfer: 16, matrix: 9, full_range: false };
    /// HLG: BT.2020 primaries, HLG, BT.2020 non-constant luminance, limited range.
    pub const BT2100_HLG: Color = Color { primaries: 9, transfer: 18, matrix: 9, full_range: false };
}

impl Default for Color {
    fn default() -> Self {
        Color::BT709
    }
}

/// One video stream as a file or a stream needs to describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormat {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction (`30/1`, `30000/1001`): the nominal frame length.
    pub fps_num: u32,
    pub fps_den: u32,
    /// H.264: SPS then PPS; HEVC: VPS, SPS, PPS. Bare NAL units (no start codes or lengths).
    /// Empty for ProRes.
    pub parameter_sets: Vec<Vec<u8>>,
    /// The decoder configuration record: `avcC` (H.264) or `hvcC` (HEVC) contents; empty for
    /// ProRes. [`VideoFormat::new`] builds it from the parameter sets.
    pub config: Vec<u8>,
    pub color: Color,
}

impl VideoFormat {
    /// A format whose decoder configuration record is built from `parameter_sets`.
    pub fn new(
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: (u32, u32),
        parameter_sets: Vec<Vec<u8>>,
        color: Color,
    ) -> Result<VideoFormat, crate::Error> {
        let config = match codec {
            VideoCodec::H264 => nal::avc_config(&parameter_sets)?,
            VideoCodec::Hevc { .. } => nal::hevc_config(&parameter_sets)?,
            VideoCodec::ProRes(_) => Vec::new(),
        };
        Ok(VideoFormat {
            codec,
            width,
            height,
            fps_num: fps.0.max(1),
            fps_den: fps.1.max(1),
            parameter_sets,
            config,
            color,
        })
    }

    /// One frame's nominal length in nanoseconds.
    pub fn frame_ns(&self) -> i64 {
        (self.fps_den as i64 * 1_000_000_000) / self.fps_num.max(1) as i64
    }

    /// The parameter sets as Annex B (start codes before each), as a stream's first keyframe
    /// carries them.
    pub fn parameter_sets_annex_b(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for p in &self.parameter_sets {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(p);
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC-LC, 1024 frames a packet.
    Aac,
    /// Opus, 960 frames (20 ms at 48 kHz) a packet.
    Opus,
}

impl AudioCodec {
    pub fn frames_per_packet(self) -> u32 {
        match self {
            AudioCodec::Aac => 1024,
            AudioCodec::Opus => 960,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Opus => "Opus",
        }
    }
}

/// One audio stream as a file or a stream needs to describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate_kbps: u32,
    /// Input frames the encoder adds before the first real sample (AAC-LC: 2112; Opus: its
    /// pre-skip). A file trims them; a packet's time is its first input frame's.
    pub priming_frames: u32,
    /// AAC: the AudioSpecificConfig; Opus: the `OpusHead` identification header.
    pub config: Vec<u8>,
}

impl AudioFormat {
    /// A format whose configuration is built from the codec's parameters.
    pub fn new(
        codec: AudioCodec,
        sample_rate: u32,
        channels: u32,
        bitrate_kbps: u32,
        priming_frames: u32,
    ) -> AudioFormat {
        let config = match codec {
            AudioCodec::Aac => crate::audio_config::aac_specific_config(sample_rate, channels),
            AudioCodec::Opus => crate::audio_config::opus_head(channels, priming_frames as u16, sample_rate),
        };
        AudioFormat { codec, sample_rate, channels, bitrate_kbps, priming_frames, config }
    }

    pub fn frames_per_packet(&self) -> u32 {
        self.codec.frames_per_packet()
    }

    /// One packet's length in nanoseconds.
    pub fn packet_ns(&self) -> i64 {
        self.frames_per_packet() as i64 * 1_000_000_000 / self.sample_rate.max(1) as i64
    }

    /// The priming in nanoseconds.
    pub fn priming_ns(&self) -> i64 {
        self.priming_frames as i64 * 1_000_000_000 / self.sample_rate.max(1) as i64
    }
}

/// What a recording or stream track carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackFormat {
    Video(VideoFormat),
    Audio(AudioFormat),
}

/// One encoded picture or sound packet, borrowed, as the writers take it.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    pub data: &'a [u8],
    /// How H.264/HEVC data is laid out; [`NalLayout::Raw`] for ProRes and sound.
    pub layout: NalLayout,
    /// Presentation time, nanoseconds, on the program clock.
    pub pts_ns: i64,
    /// Decode time; equal to `pts_ns` unless pictures are reordered.
    pub dts_ns: i64,
    pub keyframe: bool,
}

impl<'a> Packet<'a> {
    /// A sound packet (every one a sync sample).
    pub fn audio(data: &'a [u8], pts_ns: i64) -> Packet<'a> {
        Packet { data, layout: NalLayout::Raw, pts_ns, dts_ns: pts_ns, keyframe: true }
    }

    /// A picture without reordering (decode time = presentation time).
    pub fn video(data: &'a [u8], layout: NalLayout, pts_ns: i64, keyframe: bool) -> Packet<'a> {
        Packet { data, layout, pts_ns, dts_ns: pts_ns, keyframe }
    }
}
