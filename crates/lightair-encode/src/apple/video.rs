//! A VideoToolbox compression session.
//!
//! The session makes its own pool of IOSurface-backed pixel buffers in the input format asked
//! for (BGRA, NV12 or P010); the renderer takes a buffer from [`VideoEncoder::pixel_buffer_pool`],
//! draws into it through a Metal texture over the same memory, and hands it back to
//! [`VideoEncoder::encode`]: no copy between the picture and the encoder. Encoded pictures
//! come back on VideoToolbox's own thread through the sink given to [`VideoEncoder::new`], as
//! length-prefixed NAL units (as the engine's packets expect) with the stream's format
//! (parameter sets and decoder configuration record) whenever it changes.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMSampleAttachmentKey_NotSync, kCMTimeInvalid,
    kCMVideoCodecType_AppleProRes422, kCMVideoCodecType_AppleProRes422HQ, kCMVideoCodecType_AppleProRes422LT,
    kCMVideoCodecType_AppleProRes422Proxy, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferPool, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_2100_HLG, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionCopyProperty, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTInvalidSessionErr, kVTProfileLevel_H264_Baseline_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_H264_Main_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTProfileLevel_HEVC_Main10_AutoLevel,
};
use parking_lot::Mutex;

use crate::Error;
use crate::format::{Color, H264Profile, Packet, ProRes, VideoCodec, VideoFormat};
use crate::nal::NalLayout;

/// The pixel buffers the session's pool holds, and so the format the renderer's last pass
/// writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputFormat {
    /// 8-bit BGRA: what the engine's encoder targets draw today (the encoder converts).
    Bgra,
    /// 8-bit 4:2:0 YCbCr, two planes: the hardware encoders' own format (no conversion).
    Nv12,
    /// 10-bit 4:2:0 YCbCr, two planes: for HEVC Main 10 and HDR.
    P010,
}

impl InputFormat {
    fn pixel_format(self, full_range: bool) -> u32 {
        match (self, full_range) {
            (InputFormat::Bgra, _) => kCVPixelFormatType_32BGRA,
            (InputFormat::Nv12, false) => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            (InputFormat::Nv12, true) => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            (InputFormat::P010, false) => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            (InputFormat::P010, true) => kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
        }
    }
}

/// Whether a software encoder will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hardware {
    /// The media engine when there is one, else software.
    Prefer,
    /// The media engine or nothing (macOS; iOS encoders are always hardware).
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
        }
    }

    /// A recording's: still realtime (the program is live), reordering allowed, a keyframe
    /// every 2 s.
    pub fn recording(codec: VideoCodec, width: u32, height: u32, fps: u32, kbps: u32) -> Self {
        VideoSettings { allow_reordering: true, ..VideoSettings::streaming(codec, width, height, fps, kbps) }
    }

    fn fps(&self) -> f64 {
        self.fps_num.max(1) as f64 / self.fps_den.max(1) as f64
    }
}

/// One encoded picture.
#[derive(Debug, Clone)]
pub struct EncodedVideo {
    /// NAL units with 4-byte lengths (H.264, HEVC), or the picture (ProRes).
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

/// What the session sends back, on VideoToolbox's thread.
#[derive(Debug)]
pub enum VideoOutput {
    Packet(EncodedVideo),
    /// The encoder dropped the frame shown at `pts_ns`.
    Dropped {
        pts_ns: i64,
    },
    /// The encoder failed a frame (`status`); [`Error::Invalidated`] when the session was taken
    /// away.
    Failed(Error),
}

pub type VideoSink = Box<dyn FnMut(VideoOutput) + Send>;

/// What the output callback reaches through its reference.
struct Shared {
    sink: Mutex<VideoSink>,
    settings: VideoSettings,
    /// The format description last seen (by address) and the format made from it.
    format: Mutex<Option<(usize, Arc<VideoFormat>)>>,
    packets: AtomicU64,
}

pub struct VideoEncoder {
    session: CFRetained<VTCompressionSession>,
    shared: Arc<Shared>,
    bitrate_kbps: AtomicU32,
    keyframe_wanted: AtomicBool,
}

// VideoToolbox sessions take frames and property changes from any thread.
unsafe impl Send for VideoEncoder {}
unsafe impl Sync for VideoEncoder {}

fn codec_type(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => kCMVideoCodecType_H264,
        VideoCodec::Hevc { .. } => kCMVideoCodecType_HEVC,
        VideoCodec::ProRes(ProRes::Proxy) => kCMVideoCodecType_AppleProRes422Proxy,
        VideoCodec::ProRes(ProRes::Lt) => kCMVideoCodecType_AppleProRes422LT,
        VideoCodec::ProRes(ProRes::Standard) => kCMVideoCodecType_AppleProRes422,
        VideoCodec::ProRes(ProRes::Hq) => kCMVideoCodecType_AppleProRes422HQ,
    }
}

fn ns(t: CMTime) -> Option<i64> {
    let flags = t.flags;
    let (value, scale) = (t.value, t.timescale);
    (flags.contains(CMTimeFlags::Valid) && scale > 0).then(|| (value as i128 * 1_000_000_000 / scale as i128) as i64)
}

fn time(ns: i64) -> CMTime {
    CMTime { value: ns, timescale: 1_000_000_000, flags: CMTimeFlags::Valid, epoch: 0 }
}

fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    unsafe { VTSessionSetProperty(session, key, Some(value)) }
}

impl VideoEncoder {
    /// Opens a session for `settings`; encoded pictures go to `sink` on VideoToolbox's thread.
    pub fn new(settings: VideoSettings, sink: VideoSink) -> Result<VideoEncoder, Error> {
        let shared = Arc::new(Shared {
            sink: Mutex::new(sink),
            settings: settings.clone(),
            format: Mutex::new(None),
            packets: AtomicU64::new(0),
        });
        let session = make_session(&settings, &shared, settings.bitrate_kbps)?;
        Ok(VideoEncoder {
            session,
            shared,
            bitrate_kbps: AtomicU32::new(settings.bitrate_kbps),
            keyframe_wanted: AtomicBool::new(true),
        })
    }

    pub fn settings(&self) -> &VideoSettings {
        &self.shared.settings
    }

    /// The pool the session takes its input from: IOSurface-backed, Metal-compatible buffers
    /// of the settings' size and input format.
    pub fn pixel_buffer_pool(&self) -> Option<CFRetained<CVPixelBufferPool>> {
        unsafe { self.session.pixel_buffer_pool() }
    }

    /// The pool as a `CVPixelBufferPoolRef` with one reference given to the caller: the
    /// engine's `EncoderTarget::PixelBufferPool` handle.
    pub fn pool_handle(&self) -> Option<u64> {
        let pool = self.pixel_buffer_pool()?;
        Some(CFRetained::into_raw(pool).as_ptr() as usize as u64)
    }

    /// Encodes `buffer`, shown at `pts_ns`; a keyframe when `keyframe` or one was asked for.
    pub fn encode(&self, buffer: &CVPixelBuffer, pts_ns: i64, keyframe: bool) -> Result<(), Error> {
        let force = keyframe | self.keyframe_wanted.swap(false, Ordering::AcqRel);
        let options: Option<CFRetained<CFDictionary<CFString, CFType>>> = force.then(|| {
            let keys: [&CFString; 1] = unsafe { [kVTEncodeFrameOptionKey_ForceKeyFrame] };
            let values: [&CFType; 1] = [CFBoolean::new(true).as_ref()];
            CFDictionary::from_slices(&keys, &values)
        });
        let mut flags = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                buffer,
                time(pts_ns),
                kCMTimeInvalid,
                options.as_deref().map(|d| d.as_opaque()),
                ptr::null_mut(),
                &mut flags,
            )
        };
        match status {
            0 => Ok(()),
            s if s == kVTInvalidSessionErr => {
                self.keyframe_wanted.store(true, Ordering::Release);
                Err(Error::Invalidated)
            }
            s => {
                if force {
                    self.keyframe_wanted.store(true, Ordering::Release);
                }
                Err(Error::Platform { what: "the video encoder refused a frame", status: s })
            }
        }
    }

    /// [`VideoEncoder::encode`] for a `CVPixelBufferRef` whose one reference the caller gives
    /// up (what the engine's `next_video_frame` hands out); released after.
    ///
    /// # Safety
    /// `buffer` must be a `CVPixelBufferRef` (or 0) the caller owns one reference to.
    pub unsafe fn encode_owned(&self, buffer: usize, pts_ns: i64, keyframe: bool) -> Result<(), Error> {
        let Some(ptr) = NonNull::new(buffer as *mut CVPixelBuffer) else {
            return Ok(());
        };
        let buffer = unsafe { CFRetained::from_raw(ptr) };
        self.encode(&buffer, pts_ns, keyframe)
    }

    /// The next frame is a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe_wanted.store(true, Ordering::Release);
    }

    /// The average bitrate from the next frame on (the ceiling follows it).
    pub fn set_bitrate(&self, kbps: u32) {
        self.bitrate_kbps.store(kbps, Ordering::Release);
        apply_bitrate(&self.session, &self.shared.settings, kbps);
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps.load(Ordering::Acquire)
    }

    /// Whether the media engine encodes (rather than software).
    pub fn using_hardware(&self) -> bool {
        let mut out: *const CFType = ptr::null();
        let status = unsafe {
            VTSessionCopyProperty(
                &self.session,
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                None,
                (&mut out as *mut *const CFType).cast(),
            )
        };
        if status != 0 || out.is_null() {
            // iOS answers nothing: its encoders are the media engine's
            return cfg!(target_os = "ios");
        }
        let value = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out as *mut CFType)) };
        value.downcast_ref::<CFBoolean>().is_some_and(|b| b.as_bool())
    }

    /// Pictures encoded so far.
    pub fn packets(&self) -> u64 {
        self.shared.packets.load(Ordering::Relaxed)
    }

    /// Encodes every frame still inside the session; their pictures reach the sink before
    /// this returns.
    pub fn flush(&self) {
        unsafe { self.session.complete_frames(kCMTimeInvalid) };
    }

    /// A new session in place of one that was invalidated (iOS, back from the background), at
    /// the bitrate now, starting on a keyframe. Its pool is new: renderers take it again.
    pub fn reopen(&mut self) -> Result<(), Error> {
        let session = make_session(&self.shared.settings, &self.shared, self.bitrate_kbps())?;
        let old = std::mem::replace(&mut self.session, session);
        unsafe {
            old.complete_frames(kCMTimeInvalid);
            old.invalidate();
        }
        self.keyframe_wanted.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}

fn apply_bitrate(session: &VTCompressionSession, settings: &VideoSettings, kbps: u32) {
    if matches!(settings.codec, VideoCodec::ProRes(_)) {
        return;
    }
    let bps = CFNumber::new_i64(kbps as i64 * 1000);
    unsafe { set(session, kVTCompressionPropertyKey_AverageBitRate, &bps) };
    let ceiling = settings.max_bitrate_kbps.unwrap_or(kbps * 3 / 2).max(kbps) as i64;
    let bytes = CFNumber::new_i64(ceiling * 1000 / 8);
    let seconds = CFNumber::new_f64(1.0);
    let limits = CFArray::<CFNumber>::from_objects(&[&*bytes, &*seconds]);
    // not every encoder takes a ceiling; the average still holds
    let _ = unsafe { set(session, kVTCompressionPropertyKey_DataRateLimits, limits.as_ref()) };
}

fn make_session(
    settings: &VideoSettings,
    shared: &Arc<Shared>,
    kbps: u32,
) -> Result<CFRetained<VTCompressionSession>, Error> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let format = CFNumber::new_i32(settings.input.pixel_format(settings.color.full_range) as i32);
    let width = CFNumber::new_i32(settings.width as i32);
    let height = CFNumber::new_i32(settings.height as i32);
    let keys: [&CFString; 5] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        ]
    };
    let values: [&CFType; 5] =
        [format.as_ref(), width.as_ref(), height.as_ref(), empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attributes = CFDictionary::from_slices(&keys, &values);
    let specification = encoder_specification(settings.hardware);
    let mut out: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            None,
            settings.width as i32,
            settings.height as i32,
            codec_type(settings.codec),
            specification.as_deref().map(|d| d.as_opaque()),
            Some(attributes.as_opaque()),
            None,
            Some(output),
            Arc::as_ptr(shared) as *mut c_void,
            NonNull::from(&mut out),
        )
    };
    let Some(raw) = NonNull::new(out).filter(|_| status == 0) else {
        return Err(Error::Unsupported(format!(
            "this Mac can't encode {} at {}×{} (status {status})",
            settings.codec.name(),
            settings.width,
            settings.height
        )));
    };
    let session = unsafe { CFRetained::from_raw(raw) };
    let s = &*session;
    let yes = CFBoolean::new(true);
    let no = CFBoolean::new(false);
    unsafe {
        set(s, kVTCompressionPropertyKey_RealTime, if settings.realtime { yes } else { no });
        set(s, kVTCompressionPropertyKey_AllowFrameReordering, if settings.allow_reordering { yes } else { no });
        let profile = match settings.codec {
            VideoCodec::H264 => Some(match settings.h264_profile {
                H264Profile::Baseline => kVTProfileLevel_H264_Baseline_AutoLevel,
                H264Profile::Main => kVTProfileLevel_H264_Main_AutoLevel,
                H264Profile::High => kVTProfileLevel_H264_High_AutoLevel,
            }),
            VideoCodec::Hevc { main10: false } => Some(kVTProfileLevel_HEVC_Main_AutoLevel),
            VideoCodec::Hevc { main10: true } => Some(kVTProfileLevel_HEVC_Main10_AutoLevel),
            VideoCodec::ProRes(_) => None,
        };
        if let Some(profile) = profile {
            let status = set(s, kVTCompressionPropertyKey_ProfileLevel, profile);
            if status != 0 {
                session.invalidate();
                return Err(Error::Unsupported(format!(
                    "this Mac's {} encoder doesn't take that profile (status {status})",
                    settings.codec.name()
                )));
            }
        }
        let fps = CFNumber::new_f64(settings.fps());
        set(s, kVTCompressionPropertyKey_ExpectedFrameRate, &fps);
        if !matches!(settings.codec, VideoCodec::ProRes(_)) {
            let interval_s = settings.keyframe_interval_s.max(0.1) as f64;
            let frames = CFNumber::new_i32((interval_s * settings.fps()).round().max(1.0) as i32);
            let seconds = CFNumber::new_f64(interval_s);
            set(s, kVTCompressionPropertyKey_MaxKeyFrameInterval, &frames);
            set(s, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &seconds);
        }
        let (primaries, transfer, matrix) = color_keys(&settings.color);
        set(s, kVTCompressionPropertyKey_ColorPrimaries, primaries);
        set(s, kVTCompressionPropertyKey_TransferFunction, transfer);
        set(s, kVTCompressionPropertyKey_YCbCrMatrix, matrix);
    }
    apply_bitrate(s, settings, kbps);
    let status = unsafe { session.prepare_to_encode_frames() };
    if status != 0 {
        unsafe { session.invalidate() };
        return Err(Error::Platform { what: "the video encoder wouldn't start", status });
    }
    Ok(session)
}

#[cfg(target_os = "macos")]
fn encoder_specification(hardware: Hardware) -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    use objc2_video_toolbox::{
        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
    };
    let key = unsafe {
        match hardware {
            Hardware::Prefer => kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
            Hardware::Require => kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
        }
    };
    let values: [&CFType; 1] = [CFBoolean::new(true).as_ref()];
    Some(CFDictionary::from_slices(&[key], &values))
}

#[cfg(not(target_os = "macos"))]
fn encoder_specification(_hardware: Hardware) -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    None
}

/// CoreVideo's names for the colour code points the encoder tags its stream with.
fn color_keys(c: &Color) -> (&'static CFString, &'static CFString, &'static CFString) {
    unsafe {
        let primaries = match c.primaries {
            9 => kCVImageBufferColorPrimaries_ITU_R_2020,
            _ => kCVImageBufferColorPrimaries_ITU_R_709_2,
        };
        let transfer = match c.transfer {
            16 => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            18 => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            _ => kCVImageBufferTransferFunction_ITU_R_709_2,
        };
        let matrix = match c.matrix {
            9 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
            _ => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
        };
        (primaries, transfer, matrix)
    }
}

/// VideoToolbox's output callback: the picture and, when its format description is new, the
/// stream's format, to the sink.
unsafe extern "C-unwind" fn output(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // the session never outlives the encoder that holds this
    let shared = unsafe { &*(refcon as *const Shared) };
    let sample = unsafe { sample.as_ref() };
    // a panic must not unwind into VideoToolbox
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let event = if status != 0 {
            VideoOutput::Failed(if status == kVTInvalidSessionErr {
                Error::Invalidated
            } else {
                Error::Platform { what: "the video encoder failed a frame", status }
            })
        } else {
            match sample {
                None => VideoOutput::Dropped { pts_ns: -1 },
                Some(s) if flags.0 & 2 != 0 => {
                    VideoOutput::Dropped { pts_ns: ns(unsafe { s.presentation_time_stamp() }).unwrap_or(-1) }
                }
                Some(s) => match unsafe { read_sample(shared, s) } {
                    Some(p) => VideoOutput::Packet(p),
                    None => return,
                },
            }
        };
        if matches!(event, VideoOutput::Packet(_)) {
            shared.packets.fetch_add(1, Ordering::Relaxed);
        }
        (shared.sink.lock())(event);
    }));
}

unsafe fn read_sample(shared: &Shared, sample: &CMSampleBuffer) -> Option<EncodedVideo> {
    let keyframe = unsafe { !not_sync(sample) };
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    if len > 0 {
        let status = unsafe { block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr().cast())?) };
        if status != 0 {
            return None;
        }
    }
    let pts_ns = ns(unsafe { sample.presentation_time_stamp() })?;
    let dts_ns = ns(unsafe { sample.decode_time_stamp() }).unwrap_or(pts_ns);
    let format = unsafe { sample.format_description() }.and_then(|d| {
        let address = CFRetained::as_ptr(&d).as_ptr() as usize;
        let mut known = shared.format.lock();
        if known.as_ref().is_some_and(|(a, _)| *a == address) {
            return None;
        }
        let made = Arc::new(unsafe { video_format(&shared.settings, &d) }?);
        *known = Some((address, made.clone()));
        Some(made)
    });
    Some(EncodedVideo { data, pts_ns, dts_ns, keyframe, codec: shared.settings.codec, format })
}

/// Whether the sample's attachments say it isn't a sync sample.
unsafe fn not_sync(sample: &CMSampleBuffer) -> bool {
    let Some(array) = (unsafe { sample.sample_attachments_array(false) }) else {
        return false;
    };
    if array.count() == 0 {
        return false;
    }
    let dict = unsafe { array.value_at_index(0) } as *const CFDictionary;
    let Some(dict) = (unsafe { dict.as_ref() }) else {
        return false;
    };
    let key: *const CFString = unsafe { kCMSampleAttachmentKey_NotSync };
    let value = unsafe { dict.value(key.cast()) } as *const CFBoolean;
    unsafe { value.as_ref() }.is_some_and(|b| b.as_bool())
}

/// The stream's format from VideoToolbox's format description.
unsafe fn video_format(settings: &VideoSettings, d: &CMFormatDescription) -> Option<VideoFormat> {
    let fps = (settings.fps_num, settings.fps_den);
    let sets = match settings.codec {
        VideoCodec::H264 => unsafe { parameter_sets(d, CMVideoFormatDescriptionGetH264ParameterSetAtIndex) },
        VideoCodec::Hevc { .. } => unsafe { parameter_sets(d, CMVideoFormatDescriptionGetHEVCParameterSetAtIndex) },
        VideoCodec::ProRes(_) => Vec::new(),
    };
    match VideoFormat::new(settings.codec, settings.width, settings.height, fps, sets, settings.color) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(error = %e, "the encoder's parameter sets didn't parse");
            None
        }
    }
}

type ParameterSetAtIndex = unsafe extern "C-unwind" fn(
    &CMFormatDescription,
    usize,
    *mut *const u8,
    *mut usize,
    *mut usize,
    *mut std::ffi::c_int,
) -> i32;

unsafe fn parameter_sets(d: &CMFormatDescription, get: ParameterSetAtIndex) -> Vec<Vec<u8>> {
    let mut count = 0usize;
    let status = unsafe { get(d, 0, ptr::null_mut(), ptr::null_mut(), &mut count, ptr::null_mut()) };
    if status != 0 {
        return Vec::new();
    }
    (0..count)
        .filter_map(|i| {
            let mut p: *const u8 = ptr::null();
            let mut n = 0usize;
            let status = unsafe { get(d, i, &mut p, &mut n, ptr::null_mut(), ptr::null_mut()) };
            (status == 0 && !p.is_null()).then(|| unsafe { std::slice::from_raw_parts(p, n) }.to_vec())
        })
        .collect()
}
