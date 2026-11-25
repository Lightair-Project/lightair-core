//! H.264 in software through OpenH264, built from its source (BSD-2-Clause): the fallback when a
//! machine has no hardware encoder, on any platform.
//!
//! Frames come in memory (I420, NV12 or BGRA) and are encoded on the caller's thread; the
//! pictures reach the sink before [`H264Encoder::encode`] returns, as length-prefixed NAL units
//! without parameter sets, with the stream's format (parameter sets, `avcC`) whenever it
//! changes: the same shape as the hardware encoders' packets.
//!
//! The encoder makes Constrained Baseline pictures (no B-frames, so decode time = presentation
//! time) up to 3840×2160.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use openh264::encoder::{
    BitRate, ColorPrimaries, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    MatrixCoefficients, Profile, RateControlMode, SpsPpsStrategy, TransferCharacteristics, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::{OpenH264API, Timestamp};
use openh264_sys2::{ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};

use crate::Error;
use crate::encoder::{EncodedVideo, VideoEncode, VideoOutput, VideoSettings, VideoSink, keyframe_frames};
use crate::format::{Color, VideoCodec, VideoFormat};
use crate::nal::{self, NalLayout};

/// One frame in memory, rows top to bottom.
#[derive(Debug, Clone, Copy)]
pub enum Frame<'a> {
    /// Three planes: luma, then the two chroma planes at half size each way.
    I420 { y: &'a [u8], u: &'a [u8], v: &'a [u8], y_stride: usize, uv_stride: usize },
    /// Luma, then interleaved Cb/Cr at half size each way.
    Nv12 { y: &'a [u8], uv: &'a [u8], y_stride: usize, uv_stride: usize },
    /// 8-bit BGRA, converted to limited-range YCbCr with the settings' matrix.
    Bgra { data: &'a [u8], stride: usize },
}

type KnownFormat = Option<(Vec<Vec<u8>>, Arc<VideoFormat>)>;

struct Inner {
    encoder: Encoder,
    /// The I420 planes NV12 and BGRA frames are converted into.
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// The parameter sets last seen and the format made from them.
    format: KnownFormat,
}

pub struct H264Encoder {
    inner: Mutex<Inner>,
    sink: Mutex<VideoSink>,
    settings: VideoSettings,
    bitrate_kbps: AtomicU32,
    keyframe_wanted: AtomicBool,
    packets: AtomicU64,
}

fn platform(what: &'static str) -> impl FnOnce(openh264::Error) -> Error {
    move |_| Error::Platform { what, status: -1 }
}

fn vui(color: &Color) -> VuiConfig {
    let primaries = match color.primaries {
        9 => ColorPrimaries::Bt2020,
        6 => ColorPrimaries::Smpte170M,
        _ => ColorPrimaries::Bt709,
    };
    let transfer = match color.transfer {
        16 => TransferCharacteristics::Smpte2084,
        18 => TransferCharacteristics::Hlg,
        13 => TransferCharacteristics::Srgb,
        6 => TransferCharacteristics::Smpte170M,
        _ => TransferCharacteristics::Bt709,
    };
    let matrix = match color.matrix {
        9 => MatrixCoefficients::Bt2020Ncl,
        6 => MatrixCoefficients::Smpte170M,
        _ => MatrixCoefficients::Bt709,
    };
    VuiConfig::new()
        .color_primaries(primaries)
        .transfer_characteristics(transfer)
        .matrix_coefficients(matrix)
        .full_range(color.full_range)
}

fn set_bitrate_option(encoder: &mut Encoder, option: openh264_sys2::ENCODER_OPTION, bps: u32) {
    let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: bps.min(i32::MAX as u32) as i32 };
    // the next frame's rate control takes it; a refusal leaves the bitrate as it was
    let _ = unsafe { encoder.raw_api().set_option(option, (&mut info as *mut SBitrateInfo).cast()) };
}

impl H264Encoder {
    /// An encoder for `settings` (H.264 only; 8-bit; any input format but P010); pictures go to
    /// `sink` on the thread that calls [`H264Encoder::encode`].
    pub fn new(settings: VideoSettings, sink: VideoSink) -> Result<H264Encoder, Error> {
        if settings.codec != VideoCodec::H264 {
            return Err(Error::Unsupported(format!(
                "the software encoder makes H.264 only, not {}",
                settings.codec.name()
            )));
        }
        if !settings.width.is_multiple_of(2)
            || !settings.height.is_multiple_of(2)
            || settings.width == 0
            || settings.height == 0
        {
            return Err(Error::Unsupported(format!("{}×{} isn't an even size", settings.width, settings.height)));
        }
        if settings.width.max(settings.height) > 3840 || settings.width.min(settings.height) > 2160 {
            return Err(Error::Unsupported("the software H.264 encoder stops at 3840×2160".into()));
        }
        let fps = settings.fps_num.max(1) as f32 / settings.fps_den.max(1) as f32;
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u16;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(settings.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(fps))
            .rate_control_mode(RateControlMode::Bitrate)
            // a live picture is never skipped to save bits: the stream's timing stays whole
            .skip_frames(false)
            // the program is moving pictures (games, cameras), not a still screen of text
            .usage_type(if settings.realtime {
                UsageType::CameraVideoRealTime
            } else {
                UsageType::CameraVideoNonRealTime
            })
            .profile(Profile::Baseline)
            .complexity(if settings.realtime { Complexity::Low } else { Complexity::Medium })
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(keyframe_frames(&settings)))
            .num_threads(threads)
            .vui(vui(&settings.color));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| Error::Unsupported(format!("the software H.264 encoder wouldn't start: {e}")))?;
        let (w, h) = (settings.width as usize, settings.height as usize);
        Ok(H264Encoder {
            inner: Mutex::new(Inner {
                encoder,
                y: vec![0; w * h],
                u: vec![0; w * h / 4],
                v: vec![0; w * h / 4],
                format: None,
            }),
            sink: Mutex::new(sink),
            bitrate_kbps: AtomicU32::new(settings.bitrate_kbps),
            settings,
            keyframe_wanted: AtomicBool::new(true),
            packets: AtomicU64::new(0),
        })
    }

    pub fn settings(&self) -> &VideoSettings {
        &self.settings
    }

    /// Encodes `frame`, shown at `pts_ns`; a keyframe when `keyframe` or one was asked for.
    /// The picture reaches the sink before this returns.
    pub fn encode(&self, frame: Frame<'_>, pts_ns: i64, keyframe: bool) -> Result<(), Error> {
        let force = keyframe | self.keyframe_wanted.swap(false, Ordering::AcqRel);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.encode_locked(&mut inner, frame, pts_ns, force);
        drop(inner);
        match result {
            Ok(Some(out)) => {
                if matches!(out, VideoOutput::Packet(_)) {
                    self.packets.fetch_add(1, Ordering::Relaxed);
                }
                (self.sink.lock().unwrap_or_else(|e| e.into_inner()))(out);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => {
                if force {
                    self.keyframe_wanted.store(true, Ordering::Release);
                }
                Err(e)
            }
        }
    }

    fn encode_locked(
        &self,
        inner: &mut Inner,
        frame: Frame<'_>,
        pts_ns: i64,
        force: bool,
    ) -> Result<Option<VideoOutput>, Error> {
        let (w, h) = (self.settings.width as usize, self.settings.height as usize);
        let Inner { encoder, y, u, v, format } = inner;
        let (planes, strides) = match frame {
            Frame::I420 { y: py, u: pu, v: pv, y_stride, uv_stride } => {
                check_plane(py, y_stride, w, h)?;
                check_plane(pu, uv_stride, w / 2, h / 2)?;
                check_plane(pv, uv_stride, w / 2, h / 2)?;
                (
                    (&py[..y_stride * h], &pu[..uv_stride * (h / 2)], &pv[..uv_stride * (h / 2)]),
                    (y_stride, uv_stride, uv_stride),
                )
            }
            Frame::Nv12 { y: py, uv, y_stride, uv_stride } => {
                check_plane(py, y_stride, w, h)?;
                check_plane(uv, uv_stride, w, h / 2)?;
                for row in 0..h {
                    y[row * w..row * w + w].copy_from_slice(&py[row * y_stride..row * y_stride + w]);
                }
                for row in 0..h / 2 {
                    let src = &uv[row * uv_stride..row * uv_stride + w];
                    for x in 0..w / 2 {
                        u[row * (w / 2) + x] = src[2 * x];
                        v[row * (w / 2) + x] = src[2 * x + 1];
                    }
                }
                ((&y[..], &u[..], &v[..]), (w, w / 2, w / 2))
            }
            Frame::Bgra { data, stride } => {
                check_plane(data, stride, w * 4, h)?;
                bgra_to_i420(data, stride, w, h, &self.settings.color, y, u, v);
                ((&y[..], &u[..], &v[..]), (w, w / 2, w / 2))
            }
        };
        let source = YUVSlices::new(planes, (w, h), strides);
        if force {
            encoder.force_intra_frame();
        }
        let millis = (pts_ns.max(0) / 1_000_000) as u64;
        let stream = encoder
            .encode_at(&source, Timestamp::from_millis(millis))
            .map_err(platform("the software H.264 encoder failed a frame"))?;
        let frame_type = stream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(Some(VideoOutput::Dropped { pts_ns }));
        }
        let annex_b = stream.to_vec();
        let keyframe = frame_type == FrameType::IDR;
        let sets = nal::parameter_sets_in(&annex_b, NalLayout::AnnexB, false);
        let mut new_format = None;
        if !sets.is_empty() && format.as_ref().is_none_or(|(known, _)| *known != sets) {
            let fps = (self.settings.fps_num, self.settings.fps_den);
            let made = VideoFormat::new(
                VideoCodec::H264,
                self.settings.width,
                self.settings.height,
                fps,
                sets.clone(),
                self.settings.color,
            )?;
            let made = Arc::new(made);
            *format = Some((sets, made.clone()));
            new_format = Some(made);
        }
        let data = nal::to_sample(&annex_b, NalLayout::AnnexB, false);
        Ok(Some(VideoOutput::Packet(EncodedVideo {
            data,
            pts_ns,
            dts_ns: pts_ns,
            keyframe,
            codec: VideoCodec::H264,
            format: new_format,
        })))
    }

    /// The next frame is a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe_wanted.store(true, Ordering::Release);
    }

    /// The average bitrate from the next frame on, and the ceiling (1.5× unless set) with it.
    pub fn set_bitrate(&self, kbps: u32) {
        self.bitrate_kbps.store(kbps, Ordering::Release);
        let ceiling = self.settings.max_bitrate_kbps.unwrap_or(kbps * 3 / 2).max(kbps);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        set_bitrate_option(&mut inner.encoder, ENCODER_OPTION_MAX_BITRATE, ceiling.saturating_mul(1000));
        set_bitrate_option(&mut inner.encoder, ENCODER_OPTION_BITRATE, kbps.saturating_mul(1000));
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps.load(Ordering::Acquire)
    }

    /// Pictures encoded so far.
    pub fn packets(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }
}

impl VideoEncode for H264Encoder {
    fn settings(&self) -> &VideoSettings {
        H264Encoder::settings(self)
    }

    fn request_keyframe(&self) {
        H264Encoder::request_keyframe(self)
    }

    fn set_bitrate(&self, kbps: u32) {
        H264Encoder::set_bitrate(self, kbps)
    }

    fn bitrate_kbps(&self) -> u32 {
        H264Encoder::bitrate_kbps(self)
    }

    fn using_hardware(&self) -> bool {
        false
    }

    fn packets(&self) -> u64 {
        H264Encoder::packets(self)
    }

    /// Nothing waits: every frame is encoded before `encode` returns.
    fn flush(&self) {}
}

fn check_plane(plane: &[u8], stride: usize, row_bytes: usize, rows: usize) -> Result<(), Error> {
    if stride < row_bytes || plane.len() < stride * rows {
        return Err(Error::Invalid("a frame's plane is smaller than its size and stride"));
    }
    Ok(())
}

/// BGRA to limited- or full-range I420 with the colour's matrix (BT.709, BT.2020 or BT.601),
/// chroma averaged over each 2×2 block.
#[allow(clippy::too_many_arguments)]
fn bgra_to_i420(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    color: &Color,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let (kr, kb) = match color.matrix {
        9 => (0.2627f32, 0.0593f32),
        5 | 6 => (0.299, 0.114),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let (y_scale, y_offset, c_scale) =
        if color.full_range { (255.0f32, 0.0f32, 255.0f32) } else { (219.0, 16.0, 224.0) };
    let luma = |b: f32, g: f32, r: f32| kr * r + kg * g + kb * b;
    for row in 0..h {
        let src = &data[row * stride..row * stride + w * 4];
        for x in 0..w {
            let p = &src[x * 4..x * 4 + 4];
            let l = luma(p[0] as f32, p[1] as f32, p[2] as f32) / 255.0;
            y[row * w + x] = (l * y_scale + y_offset).round().clamp(0.0, 255.0) as u8;
        }
    }
    let cw = w / 2;
    for row in 0..h / 2 {
        for x in 0..cw {
            let (mut b, mut g, mut r) = (0.0f32, 0.0f32, 0.0f32);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let at = (row * 2 + dy) * stride + (x * 2 + dx) * 4;
                b += data[at] as f32;
                g += data[at + 1] as f32;
                r += data[at + 2] as f32;
            }
            let (b, g, r) = (b / (4.0 * 255.0), g / (4.0 * 255.0), r / (4.0 * 255.0));
            let l = luma(b, g, r);
            let cb = (b - l) / (2.0 * (1.0 - kb));
            let cr = (r - l) / (2.0 * (1.0 - kr));
            u[row * cw + x] = (cb * c_scale + 128.0).round().clamp(0.0, 255.0) as u8;
            v[row * cw + x] = (cr * c_scale + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}
