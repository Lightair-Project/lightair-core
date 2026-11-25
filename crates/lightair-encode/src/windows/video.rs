//! H.264 and HEVC through the GPU's Media Foundation encoder: NVIDIA's (NVENC), AMD's (VCN /
//! AMF) and Intel's (Quick Sync) drivers each install one as an asynchronous hardware transform,
//! so one code path drives all three.
//!
//! The encoder owns a small pool of Direct3D 11 textures, shared with the compositor through NT
//! handles, and a shared fence. The compositor takes a free slot ([`VideoEncoder::acquire`]),
//! draws into it on its own device (Direct3D 11 or 12), signals the fence and hands the slot to
//! [`VideoEncoder::encode_slot`] with the fence value: the encoder waits for the value and gives
//! the very texture to the transform, wrapped in a DXGI surface buffer: no copy, no trip
//! through memory. The slot comes free again when the transform's picture for it comes out.
//! BGRA input is converted to NV12 on the GPU's video processor first when the transform only
//! takes NV12.
//!
//! The transform's events (it asks for input, it has output) are handled on the encoder's own
//! thread, which calls the sink given to [`VideoEncoder::new`] with each picture as
//! length-prefixed NAL units without parameter sets, and the stream's format (parameter sets,
//! `avcC` / `hvcC`) whenever it changes: the same packets as the Apple encoder makes, so the
//! engine and the recording writers don't care which made them.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext4, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NOTACCEPTING, MF_E_SHUTDOWN, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample,
    MFMediaType_Video, MFNominalRange_0_255, MFNominalRange_16_235, MFSampleExtension_CleanPoint,
    MFSampleExtension_DecodeTimestamp, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_P010,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoPrimaries_BT2020, MFVideoTransFunc_709,
    MFVideoTransFunc_2084, MFVideoTransFunc_HLG, MFVideoTransferMatrix_BT709, MFVideoTransferMatrix_BT2020_10,
    eAVEncCommonRateControlMode_CBR, eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_Base,
    eAVEncH264VProfile_High, eAVEncH264VProfile_Main, eAVEncH265VProfile_Main_420_8, eAVEncH265VProfile_Main_420_10,
};
use windows::core::{GUID, HRESULT, Interface};

use super::blit::Blit;
use super::d3d::{self, Converter, Fence, Pool};
use super::mf::{self, EncoderInfo, Runtime, Vendor, pack, platform, set_bool, set_u32};
use crate::Error;
use crate::encoder::{
    EncodedVideo, Hardware, InputFormat, VideoEncode, VideoOutput, VideoSettings, VideoSink, frame_ns, keyframe_frames,
};
use crate::format::{H264Profile, VideoCodec, VideoFormat};
use crate::nal::{self, NalLayout};

/// How the encoder spends its bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateControl {
    /// Constant bitrate: what live platforms want.
    Cbr,
    /// Variable bitrate around the average, held under the ceiling (`max_bitrate_kbps`, else
    /// 1.5× the average): better pictures for a file.
    Vbr,
}

/// What the Windows encoder takes beyond the shared settings.
#[derive(Debug, Clone)]
pub struct Options {
    /// The device the input textures live on and the encoder works on. `None`: a new device on
    /// the default adapter. Give the compositor's adapter's device on machines with two GPUs,
    /// so the shared textures never cross between them.
    pub device: Option<ID3D11Device>,
    /// `None`: CBR for a stream (no reordering), VBR for a recording.
    pub rate_control: Option<RateControl>,
    /// Input textures in the pool (at least 2).
    pub pool_size: u32,
    /// Try this maker's encoder first; `None`: the device's GPU's.
    pub vendor: Option<Vendor>,
}

impl Default for Options {
    fn default() -> Self {
        Options { device: None, rate_control: None, pool_size: 4, vendor: None }
    }
}

/// A free input texture, handed to the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputSlot {
    pub index: u32,
    /// The texture's NT handle (`HANDLE` as an integer), for the compositor's
    /// `OpenSharedResource1` (Direct3D 11) or `OpenSharedHandle` (Direct3D 12). The encoder
    /// keeps it; the compositor opens it once per slot and doesn't close it.
    pub handle: u64,
}

/// The hardware encoders this machine has for `codec`, best first.
pub fn hardware_encoders(codec: VideoCodec) -> Result<Vec<EncoderInfo>, Error> {
    let _runtime = Runtime::start()?;
    let Some(subtype) = subtype(codec) else {
        return Ok(Vec::new());
    };
    Ok(find(subtype)?.into_iter().map(|(_, info)| info).collect())
}

fn subtype(codec: VideoCodec) -> Option<GUID> {
    match codec {
        VideoCodec::H264 => Some(MFVideoFormat_H264),
        VideoCodec::Hevc { .. } => Some(MFVideoFormat_HEVC),
        VideoCodec::ProRes(_) => None,
    }
}

fn find(subtype: GUID) -> Result<Vec<(IMFActivate, EncoderInfo)>, Error> {
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
    mf::enumerate(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        None,
        Some(output),
    )
}

/// Whether a failure means the GPU went away (removed, reset, driver updated).
fn device_lost(code: HRESULT) -> bool {
    code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET
}

fn failure(what: &'static str, e: windows::core::Error) -> Error {
    if device_lost(e.code()) { Error::Invalidated } else { Error::Platform { what, status: e.code().0 } }
}

/// One frame given to the transform or waiting to be.
struct Pending {
    sample: IMFSample,
    hns: i64,
    keyframe: bool,
}

/// One frame inside the encoder, until its picture comes out.
struct InFlight {
    hns: i64,
    pts_ns: i64,
    slot: Option<u32>,
    converted: Option<u32>,
}

#[derive(Default)]
struct State {
    /// Inputs the transform asked for and hasn't had.
    credits: u32,
    /// Frames waiting for the transform to ask.
    queue: VecDeque<Pending>,
    in_flight: VecDeque<InFlight>,
    /// Presentation times in the order frames went in: the decode times when the transform
    /// doesn't say.
    order: VecDeque<i64>,
    draining: bool,
}

/// The transform and what it was set up with.
struct Mft {
    activate: IMFActivate,
    transform: IMFTransform,
    codec: ICodecAPI,
    events: IMFMediaEventGenerator,
    input_id: u32,
    output_id: u32,
    info: EncoderInfo,
    /// B-frames between references (0 without reordering).
    bframes: u32,
    rate_control: RateControl,
}

/// One running transform and its event thread; a reopen makes a new one.
struct Pipeline {
    mft: Mft,
    converter: Option<Converter>,
    state: Mutex<State>,
    changed: Condvar,
    /// Whether the transform hands out its own output samples, and the size to allocate when
    /// not.
    provides_samples: AtomicBool,
    output_size: AtomicU32,
    stop: AtomicBool,
    exited: AtomicBool,
}

/// The parameter sets last seen and the format made from them.
type KnownFormat = Option<(Vec<Vec<u8>>, Arc<VideoFormat>)>;

/// What stays across reopens.
struct Shared {
    sink: Mutex<VideoSink>,
    settings: VideoSettings,
    format: Mutex<KnownFormat>,
    packets: AtomicU64,
    pool: Pool,
}

// Media Foundation transforms and Direct3D 11 devices made multithread-protected are
// free-threaded; every call that must not overlap is made under `Pipeline::state`.
unsafe impl Send for Pipeline {}
unsafe impl Sync for Pipeline {}
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// [`VideoEncoder::encode_copy`]'s state.
#[derive(Default)]
struct Copier {
    blit: Option<Blit>,
    context: Option<ID3D11DeviceContext4>,
    /// The slot holding the last picture copied in.
    last: Option<u32>,
    /// The last value signalled on the fence.
    value: u64,
}

/// Frames that may wait for the transform before the oldest is dropped.
const MAX_QUEUE: usize = 8;

pub struct VideoEncoder {
    shared: Arc<Shared>,
    pipeline: Arc<Pipeline>,
    thread: Option<JoinHandle<()>>,
    device: ID3D11Device,
    manager: IMFDXGIDeviceManager,
    fence: Fence,
    options: Options,
    bitrate_kbps: AtomicU32,
    keyframe_wanted: AtomicBool,
    copier: Mutex<Copier>,
    _runtime: Runtime,
}

// See `Pipeline`: the device, manager and fence are free-threaded.
unsafe impl Send for VideoEncoder {}
unsafe impl Sync for VideoEncoder {}

impl VideoEncoder {
    /// Opens the GPU's encoder for `settings` on a new device on the default adapter; encoded
    /// pictures go to `sink` on the encoder's thread.
    pub fn new(settings: VideoSettings, sink: VideoSink) -> Result<VideoEncoder, Error> {
        VideoEncoder::with_options(settings, Options::default(), sink)
    }

    /// [`VideoEncoder::new`] with a device, rate control, pool size or maker of choice.
    ///
    /// There is no software fallback here: with no hardware encoder for the codec this fails
    /// with [`Error::Unsupported`] (for `Hardware::Prefer` too), and the host uses
    /// `software::H264Encoder` instead.
    pub fn with_options(settings: VideoSettings, options: Options, sink: VideoSink) -> Result<VideoEncoder, Error> {
        if subtype(settings.codec).is_none() {
            return Err(Error::Unsupported(format!("Windows has no {} encoder", settings.codec.name())));
        }
        if !settings.width.is_multiple_of(2)
            || !settings.height.is_multiple_of(2)
            || settings.width == 0
            || settings.height == 0
        {
            return Err(Error::Unsupported(format!("{}×{} isn't an even size", settings.width, settings.height)));
        }
        let runtime = Runtime::start()?;
        let device = match &options.device {
            Some(d) => d.clone(),
            None => d3d::create_device()?,
        };
        d3d::protect(&device)?;
        let mut token = 0u32;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(platform("Media Foundation wouldn't make a device manager"))?;
        let manager = manager.ok_or(Error::Unsupported("no device manager".into()))?;
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(platform("Media Foundation wouldn't take the Direct3D 11 device"))?;
        let pool = Pool::new(&device, &settings, options.pool_size)?;
        let fence = Fence::new(&device)?;
        let shared = Arc::new(Shared {
            sink: Mutex::new(sink),
            settings,
            format: Mutex::new(None),
            packets: AtomicU64::new(0),
            pool,
        });
        let kbps = shared.settings.bitrate_kbps;
        let (pipeline, thread) = start(&shared, &device, &manager, &options, kbps)?;
        Ok(VideoEncoder {
            shared,
            pipeline,
            thread: Some(thread),
            device,
            manager,
            fence,
            options,
            bitrate_kbps: AtomicU32::new(kbps),
            keyframe_wanted: AtomicBool::new(true),
            copier: Mutex::new(Copier::default()),
            _runtime: runtime,
        })
    }

    pub fn settings(&self) -> &VideoSettings {
        &self.shared.settings
    }

    /// The device the input textures live on.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Which encoder is running (its name and maker).
    pub fn info(&self) -> EncoderInfo {
        self.pipeline.mft.info.clone()
    }

    /// The rate control in use.
    pub fn rate_control(&self) -> RateControl {
        self.pipeline.mft.rate_control
    }

    /// The shared fence's NT handle (as an integer): the compositor opens it once
    /// (`OpenSharedFence` / `OpenSharedHandle`) and signals it when a picture is finished.
    pub fn fence_handle(&self) -> u64 {
        self.fence.handle.0 as usize as u64
    }

    /// Every input texture's NT handle, by slot index: the compositor opens each once.
    pub fn slot_handles(&self) -> Vec<u64> {
        self.shared.pool.slots.iter().map(|s| s.handle.0 as usize as u64).collect()
    }

    /// A free input texture to draw the next picture into, or `None` while every one is
    /// inside the encoder (drop the frame).
    pub fn acquire(&self) -> Option<InputSlot> {
        let index = self.shared.pool.acquire()?;
        let handle = self.shared.pool.slots[index as usize].handle.0 as usize as u64;
        Some(InputSlot { index, handle })
    }

    /// Gives back a slot taken with [`VideoEncoder::acquire`] without encoding it.
    pub fn release(&self, index: u32) {
        self.shared.pool.release(index);
    }

    /// Encodes the picture in slot `index` (taken with [`VideoEncoder::acquire`]), shown at
    /// `pts_ns`, once the shared fence reaches `fence_value` (0: already finished); a keyframe
    /// when `keyframe` or one was asked for. The slot comes free when its picture comes out.
    pub fn encode_slot(&self, index: u32, fence_value: u64, pts_ns: i64, keyframe: bool) -> Result<(), Error> {
        let Some(slot) = self.shared.pool.slots.get(index as usize) else {
            return Err(Error::Invalid("no such input slot"));
        };
        if !self.fence.wait(fence_value, 1000) {
            self.shared.pool.release(index);
            return Err(Error::Platform { what: "the compositor's picture never finished", status: 0 });
        }
        let input_view = match &self.pipeline.converter {
            Some(converter) => {
                let mut view = slot.input_view.lock();
                if view.is_none() {
                    *view = Some(converter.input_view(&slot.texture)?);
                }
                view.clone()
            }
            None => None,
        };
        let result = self.submit(&slot.texture, input_view, Some(index), pts_ns, keyframe);
        if result.is_err() {
            self.shared.pool.release(index);
        }
        result
    }

    /// Encodes `texture`, a finished picture on [`VideoEncoder::device`] in the settings' size
    /// and input format, shown at `pts_ns`. The caller keeps it unchanged until the picture
    /// for `pts_ns` comes out of the sink.
    pub fn encode_texture(&self, texture: &ID3D11Texture2D, pts_ns: i64, keyframe: bool) -> Result<(), Error> {
        let input_view = match &self.pipeline.converter {
            Some(converter) => Some(converter.input_view(texture)?),
            None => None,
        };
        self.submit(texture, input_view, None, pts_ns, keyframe)
    }

    /// Encodes a copy of `source`, a finished BGRA picture on [`VideoEncoder::device`] of any
    /// size (stretched to the encoder's), shown at `pts_ns`; `None`: the last picture copied in
    /// again. The copy is made on the GPU into a free input slot and waited for, so the caller
    /// may draw into `source` again once this returns. `Ok(false)`: no picture yet, nothing
    /// encoded (a keyframe asked for comes with the next one).
    ///
    /// Don't mix it with [`VideoEncoder::acquire`] on one encoder: it keeps its last picture in
    /// a slot and signals the shared fence itself.
    pub fn encode_copy(&self, source: Option<&ID3D11Texture2D>, pts_ns: i64, keyframe: bool) -> Result<bool, Error> {
        let mut copier = self.copier.lock();
        let c = &mut *copier;
        let wanted = |e: Error| {
            if keyframe {
                self.request_keyframe();
            }
            e
        };
        if source.is_none() && c.last.is_none() {
            if keyframe {
                self.request_keyframe();
            }
            return Ok(false);
        }
        let Some(index) = self.shared.pool.acquire() else {
            return Err(wanted(Error::Unsupported("every input slot is inside the encoder".into())));
        };
        let value = c.value + 1;
        let copied = self.copy_into(c, source, index).and_then(|context| self.fence.signal(&context, value));
        if let Err(e) = copied {
            self.shared.pool.release(index);
            return Err(wanted(e));
        }
        c.value = value;
        c.last = Some(index);
        drop(copier);
        self.encode_slot(index, value, pts_ns, keyframe).map_err(wanted)?;
        Ok(true)
    }

    /// Copies `source` (or the last picture) into slot `index`; returns the context to signal on.
    fn copy_into(
        &self,
        c: &mut Copier,
        source: Option<&ID3D11Texture2D>,
        index: u32,
    ) -> Result<ID3D11DeviceContext4, Error> {
        let what = "the encoder's device has no immediate context";
        let context = match &c.context {
            Some(context) => context.clone(),
            None => {
                let immediate = unsafe { self.device.GetImmediateContext() }.map_err(platform(what))?;
                let context: ID3D11DeviceContext4 = immediate.cast().map_err(platform(what))?;
                c.context = Some(context.clone());
                context
            }
        };
        let slots = &self.shared.pool.slots;
        let dst = &slots[index as usize].texture;
        match (source, c.last) {
            (Some(src), _) => {
                if c.blit.is_none() {
                    c.blit = Some(Blit::new(&self.device)?);
                }
                if let Some(blit) = c.blit.as_mut() {
                    blit.copy(src, dst, &self.shared.settings.color)?;
                }
            }
            // the slot that holds it already, else a copy of that slot
            (None, Some(last)) if last != index => unsafe { context.CopyResource(dst, &slots[last as usize].texture) },
            _ => {}
        }
        Ok(context)
    }

    fn submit(
        &self,
        texture: &ID3D11Texture2D,
        input_view: Option<windows::Win32::Graphics::Direct3D11::ID3D11VideoProcessorInputView>,
        slot: Option<u32>,
        pts_ns: i64,
        keyframe: bool,
    ) -> Result<(), Error> {
        let pipe = &self.pipeline;
        let force = keyframe | self.keyframe_wanted.swap(false, Ordering::AcqRel);
        let restore = |e: Error| {
            if force {
                self.keyframe_wanted.store(true, Ordering::Release);
            }
            e
        };
        let (texture, converted) = match (&pipe.converter, input_view) {
            (Some(converter), Some(view)) => {
                let (index, nv12) = converter.convert(&view).map_err(restore)?;
                (nv12, Some(index))
            }
            _ => (texture.clone(), None),
        };
        let hns = pts_ns.div_euclid(100);
        let duration = frame_ns(&self.shared.settings) / 100;
        let sample = match texture_sample(&texture, hns, duration) {
            Ok(s) => s,
            Err(e) => {
                if let (Some(c), Some(i)) = (&pipe.converter, converted) {
                    c.release(i);
                }
                return Err(restore(e));
            }
        };
        let mut dropped = Vec::new();
        let result = {
            let mut st = pipe.state.lock();
            st.in_flight.push_back(InFlight { hns, pts_ns, slot, converted });
            st.order.push_back(pts_ns);
            let pending = Pending { sample, hns, keyframe: force };
            if st.credits > 0 {
                st.credits -= 1;
                match pipe.process_input(&mut st, pending) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        // the slot is the caller's to release; the conversion texture is ours
                        if let Some(f) = take_in_flight(&mut st, hns)
                            && let (Some(c), Some(i)) = (&pipe.converter, f.converted)
                        {
                            c.release(i);
                        }
                        Err(restore(e))
                    }
                }
            } else {
                st.queue.push_back(pending);
                while st.queue.len() > MAX_QUEUE {
                    let Some(old) = st.queue.pop_front() else { break };
                    if let Some(f) = take_in_flight(&mut st, old.hns) {
                        if old.keyframe {
                            self.keyframe_wanted.store(true, Ordering::Release);
                        }
                        dropped.push(self.free(f));
                    }
                }
                Ok(())
            }
        };
        if !dropped.is_empty() {
            let mut sink = self.shared.sink.lock();
            for pts_ns in dropped {
                sink(VideoOutput::Dropped { pts_ns });
            }
        }
        result
    }

    /// Frees what a frame held; returns its time.
    fn free(&self, f: InFlight) -> i64 {
        release_in_flight(&self.shared, &self.pipeline, &f);
        f.pts_ns
    }

    /// The next frame is a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe_wanted.store(true, Ordering::Release);
    }

    /// The average bitrate from the next frame on (with VBR, the ceiling follows it).
    pub fn set_bitrate(&self, kbps: u32) {
        self.bitrate_kbps.store(kbps, Ordering::Release);
        let _st = self.pipeline.state.lock();
        apply_bitrate(&self.pipeline.mft.codec, &self.shared.settings, self.pipeline.mft.rate_control, kbps);
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps.load(Ordering::Acquire)
    }

    /// Always true: this backend only runs the GPUs' encoders.
    pub fn using_hardware(&self) -> bool {
        true
    }

    /// Pictures encoded so far.
    pub fn packets(&self) -> u64 {
        self.shared.packets.load(Ordering::Relaxed)
    }

    /// Encodes every frame still inside the encoder; their pictures reach the sink before this
    /// returns. Never call it from the sink.
    pub fn flush(&self) {
        let pipe = &self.pipeline;
        let deadline = Instant::now() + Duration::from_secs(2);
        {
            let mut st = pipe.state.lock();
            while !st.queue.is_empty() {
                if pipe.changed.wait_until(&mut st, deadline).timed_out() {
                    break;
                }
            }
            st.draining = true;
        }
        let t = &pipe.mft.transform;
        let drained = unsafe {
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .and_then(|_| t.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut st = pipe.state.lock();
        if drained.is_ok() {
            while st.draining {
                if pipe.changed.wait_until(&mut st, deadline).timed_out() {
                    break;
                }
            }
        }
        st.draining = false;
        // after a drain the transform asks for input afresh
        st.credits = 0;
        let _ = unsafe { t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) };
    }

    /// A new transform in place of one that failed, on the same device and pool, at the
    /// bitrate now, starting on a keyframe. After [`Error::Invalidated`] (the GPU was removed
    /// or reset) the device is gone too: make a new `VideoEncoder` instead.
    pub fn reopen(&mut self) -> Result<(), Error> {
        let (pipeline, thread) = start(&self.shared, &self.device, &self.manager, &self.options, self.bitrate_kbps())?;
        let old = std::mem::replace(&mut self.pipeline, pipeline);
        let old_thread = self.thread.replace(thread);
        stop(&self.shared, &old, old_thread);
        self.keyframe_wanted.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        let thread = self.thread.take();
        stop(&self.shared, &self.pipeline, thread);
    }
}

impl VideoEncode for VideoEncoder {
    fn settings(&self) -> &VideoSettings {
        VideoEncoder::settings(self)
    }

    fn request_keyframe(&self) {
        VideoEncoder::request_keyframe(self)
    }

    fn set_bitrate(&self, kbps: u32) {
        VideoEncoder::set_bitrate(self, kbps)
    }

    fn bitrate_kbps(&self) -> u32 {
        VideoEncoder::bitrate_kbps(self)
    }

    fn using_hardware(&self) -> bool {
        VideoEncoder::using_hardware(self)
    }

    fn packets(&self) -> u64 {
        VideoEncoder::packets(self)
    }

    fn flush(&self) {
        VideoEncoder::flush(self)
    }
}

fn take_in_flight(st: &mut State, hns: i64) -> Option<InFlight> {
    let at = st.in_flight.iter().position(|f| f.hns == hns)?;
    let f = st.in_flight.remove(at)?;
    if let Some(o) = st.order.iter().position(|&p| p == f.pts_ns) {
        st.order.remove(o);
    }
    Some(f)
}

fn release_in_flight(shared: &Shared, pipe: &Pipeline, f: &InFlight) {
    if let Some(slot) = f.slot {
        shared.pool.release(slot);
    }
    if let (Some(c), Some(i)) = (&pipe.converter, f.converted) {
        c.release(i);
    }
}

/// A sample whose one buffer is `texture` itself.
fn texture_sample(texture: &ID3D11Texture2D, hns: i64, duration_hns: i64) -> Result<IMFSample, Error> {
    let what = "Media Foundation couldn't wrap the picture for the encoder";
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false).map_err(platform(what))?;
        // some encoders read the buffer's length before its picture
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>()
            && let Ok(len) = two_d.GetContiguousLength()
        {
            let _ = buffer.SetCurrentLength(len);
        }
        let sample = MFCreateSample().map_err(platform(what))?;
        sample.AddBuffer(&buffer).map_err(platform(what))?;
        sample.SetSampleTime(hns).map_err(platform(what))?;
        sample.SetSampleDuration(duration_hns.max(1)).map_err(platform(what))?;
        Ok(sample)
    }
}

/// The ceiling for `kbps`: the settings' own, else 1.5× the average.
fn ceiling_kbps(settings: &VideoSettings, kbps: u32) -> u32 {
    settings.max_bitrate_kbps.unwrap_or(kbps * 3 / 2).max(kbps)
}

fn apply_bitrate(codec: &ICodecAPI, settings: &VideoSettings, rate_control: RateControl, kbps: u32) {
    let bps = kbps.saturating_mul(1000);
    set_u32(codec, &CODECAPI_AVEncCommonMeanBitRate, bps);
    match rate_control {
        // a one-second buffer: the rate holds over any second
        RateControl::Cbr => {
            set_u32(codec, &CODECAPI_AVEncCommonBufferSize, bps);
        }
        RateControl::Vbr => {
            let ceiling = ceiling_kbps(settings, kbps).saturating_mul(1000);
            set_u32(codec, &CODECAPI_AVEncCommonMaxBitRate, ceiling);
            set_u32(codec, &CODECAPI_AVEncCommonBufferSize, ceiling);
        }
    }
}

/// Finds and starts an encoder and its event thread.
fn start(
    shared: &Arc<Shared>,
    device: &ID3D11Device,
    manager: &IMFDXGIDeviceManager,
    options: &Options,
    kbps: u32,
) -> Result<(Arc<Pipeline>, JoinHandle<()>), Error> {
    let settings = &shared.settings;
    let subtype = subtype(settings.codec).ok_or(Error::Unsupported("no such encoder".into()))?;
    let mut candidates = find(subtype)?;
    if candidates.is_empty() {
        let fallback = match settings.hardware {
            Hardware::Prefer => " (use the software encoder)",
            Hardware::Require => "",
        };
        return Err(Error::Unsupported(format!("this PC has no hardware {} encoder{fallback}", settings.codec.name())));
    }
    // the device's GPU's encoder first (or the maker asked for): the textures stay on one GPU
    let wanted = options.vendor.or_else(|| d3d::vendor(device));
    candidates.sort_by_key(|(_, info)| if info.vendor.is_some() && info.vendor == wanted { 0 } else { 1 });
    let mut last = None;
    for (activate, info) in candidates {
        match configure(&activate, info, settings, device, manager, options, kbps) {
            Ok((mft, converter)) => {
                let pipeline = Arc::new(Pipeline {
                    mft,
                    converter,
                    state: Mutex::new(State::default()),
                    changed: Condvar::new(),
                    provides_samples: AtomicBool::new(true),
                    output_size: AtomicU32::new(0),
                    stop: AtomicBool::new(false),
                    exited: AtomicBool::new(false),
                });
                pipeline.read_output_info();
                let (s, p) = (shared.clone(), pipeline.clone());
                let thread = std::thread::Builder::new()
                    .name("lightair-video-encoder".into())
                    .spawn(move || run(s, p))
                    .map_err(Error::Io)?;
                return Ok((pipeline, thread));
            }
            Err(e) => {
                tracing::warn!(error = %e, "a hardware encoder didn't take the settings; trying the next");
                let _ = unsafe { activate.ShutdownObject() };
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or(Error::Unsupported("no hardware encoder took the settings".into())))
}

fn media_type(major: &GUID, subtype: &GUID) -> Result<IMFMediaType, Error> {
    let what = "Media Foundation couldn't describe the stream";
    unsafe {
        let t = MFCreateMediaType().map_err(platform(what))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, major).map_err(platform(what))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype).map_err(platform(what))?;
        Ok(t)
    }
}

/// Size, rate, square pixels, progressive and the colour tags, on an input or output type.
fn describe_picture(t: &IMFMediaType, settings: &VideoSettings) -> Result<(), Error> {
    let what = "Media Foundation couldn't describe the picture";
    unsafe {
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(settings.width, settings.height)).map_err(platform(what))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(settings.fps_num.max(1), settings.fps_den.max(1)))
            .map_err(platform(what))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(platform(what))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(platform(what))?;
        let c = &settings.color;
        let primaries = if c.primaries == 9 { MFVideoPrimaries_BT2020 } else { MFVideoPrimaries_BT709 };
        let transfer = match c.transfer {
            16 => MFVideoTransFunc_2084,
            18 => MFVideoTransFunc_HLG,
            _ => MFVideoTransFunc_709,
        };
        let matrix = if c.matrix == 9 { MFVideoTransferMatrix_BT2020_10 } else { MFVideoTransferMatrix_BT709 };
        let range = if c.full_range { MFNominalRange_0_255 } else { MFNominalRange_16_235 };
        // colour tags are advice: an encoder that doesn't take one still encodes
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, primaries.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, transfer.0 as u32);
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, matrix.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, range.0 as u32);
    }
    Ok(())
}

fn input_subtype(input: InputFormat) -> GUID {
    match input {
        InputFormat::Bgra => MFVideoFormat_ARGB32,
        InputFormat::Nv12 => MFVideoFormat_NV12,
        InputFormat::P010 => MFVideoFormat_P010,
    }
}

/// Sets up one transform for `settings`; returns it and, when it only takes NV12 and the
/// input is BGRA, the converter.
fn configure(
    activate: &IMFActivate,
    info: EncoderInfo,
    settings: &VideoSettings,
    device: &ID3D11Device,
    manager: &IMFDXGIDeviceManager,
    options: &Options,
    kbps: u32,
) -> Result<(Mft, Option<Converter>), Error> {
    let transform: IMFTransform =
        unsafe { activate.ActivateObject() }.map_err(platform("the hardware encoder wouldn't start"))?;
    let attributes =
        unsafe { transform.GetAttributes() }.map_err(platform("the hardware encoder has no attributes"))?;
    unsafe {
        if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
            return Err(Error::Unsupported(format!("{} isn't an asynchronous encoder", info.name)));
        }
        // an asynchronous transform takes nothing else until it is unlocked
        attributes
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
            .map_err(platform("the hardware encoder wouldn't unlock"))?;
        if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
            return Err(Error::Unsupported(format!("{} can't take Direct3D 11 textures", info.name)));
        }
    }
    let live = settings.realtime && !settings.allow_reordering;
    unsafe {
        let _ = attributes.SetUINT32(&MF_LOW_LATENCY, live as u32);
    }
    let (input_id, output_id) = mf::stream_ids(&transform);
    let codec: ICodecAPI = transform.cast().map_err(platform("the hardware encoder has no codec properties"))?;
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .map_err(platform("the hardware encoder wouldn't take the Direct3D 11 device"))?;

    // properties the encoder reads when its types are set
    let rate_control =
        options.rate_control.unwrap_or(if settings.allow_reordering { RateControl::Vbr } else { RateControl::Cbr });
    let mode = match rate_control {
        RateControl::Cbr => eAVEncCommonRateControlMode_CBR,
        RateControl::Vbr => eAVEncCommonRateControlMode_PeakConstrainedVBR,
    };
    set_u32(&codec, &CODECAPI_AVEncCommonRateControlMode, mode.0 as u32);
    set_bool(&codec, &CODECAPI_AVLowLatencyMode, live);
    set_u32(&codec, &CODECAPI_AVEncMPVGOPSize, keyframe_frames(settings));
    let wanted_b = if settings.allow_reordering { 2 } else { 0 };
    let b_taken = set_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, wanted_b);
    apply_bitrate(&codec, settings, rate_control, kbps);

    // the output type first: an encoder offers its input types for the output it makes
    let subtype = subtype(settings.codec).ok_or(Error::Unsupported("no such encoder".into()))?;
    let output = media_type(&MFMediaType_Video, &subtype)?;
    describe_picture(&output, settings)?;
    let profile = match settings.codec {
        VideoCodec::H264 => match settings.h264_profile {
            H264Profile::Baseline => eAVEncH264VProfile_Base.0 as u32,
            H264Profile::Main => eAVEncH264VProfile_Main.0 as u32,
            H264Profile::High => eAVEncH264VProfile_High.0 as u32,
        },
        VideoCodec::Hevc { main10: false } => eAVEncH265VProfile_Main_420_8.0 as u32,
        VideoCodec::Hevc { main10: true } => eAVEncH265VProfile_Main_420_10.0 as u32,
        VideoCodec::ProRes(_) => 0,
    };
    unsafe {
        output
            .SetUINT32(&MF_MT_AVG_BITRATE, kbps.saturating_mul(1000))
            .map_err(platform("Media Foundation couldn't describe the stream"))?;
        output
            .SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
            .map_err(platform("Media Foundation couldn't describe the stream"))?;
        transform.SetOutputType(output_id, &output, 0).map_err(|e| {
            Error::Unsupported(format!(
                "{} can't make {} at {}×{} (0x{:08X})",
                info.name,
                settings.codec.name(),
                settings.width,
                settings.height,
                e.code().0
            ))
        })?;
    }

    // the input type: the settings' format, else NV12 through the GPU's video processor
    let input = media_type(&MFMediaType_Video, &input_subtype(settings.input))?;
    describe_picture(&input, settings)?;
    let mut converter = None;
    if let Err(e) = unsafe { transform.SetInputType(input_id, &input, 0) } {
        if settings.input != InputFormat::Bgra {
            return Err(Error::Unsupported(format!(
                "{} doesn't take {:?} pictures (0x{:08X})",
                info.name,
                settings.input,
                e.code().0
            )));
        }
        let nv12 = media_type(&MFMediaType_Video, &MFVideoFormat_NV12)?;
        describe_picture(&nv12, settings)?;
        unsafe { transform.SetInputType(input_id, &nv12, 0) }
            .map_err(platform("the hardware encoder takes neither BGRA nor NV12"))?;
        converter = Some(Converter::new(device, settings, options.pool_size.max(2) + MAX_QUEUE as u32)?);
    }

    let bframes = if !settings.allow_reordering || !b_taken {
        0
    } else {
        unsafe { codec.GetValue(&CODECAPI_AVEncMPVDefaultBPictureCount) }
            .ok()
            .and_then(|v| u32::try_from(&v).ok())
            .unwrap_or(wanted_b)
    };
    let events: IMFMediaEventGenerator = transform.cast().map_err(platform("the hardware encoder sends no events"))?;
    unsafe {
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(platform("the hardware encoder wouldn't start streaming"))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(platform("the hardware encoder wouldn't start streaming"))?;
    }
    tracing::info!(encoder = %info.name, ?rate_control, bframes, converting = converter.is_some(), "hardware video encoder ready");
    Ok((
        Mft { activate: activate.clone(), transform, codec, events, input_id, output_id, info, bframes, rate_control },
        converter,
    ))
}

impl Pipeline {
    fn read_output_info(&self) {
        if let Ok(info) = unsafe { self.mft.transform.GetOutputStreamInfo(self.mft.output_id) } {
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            self.provides_samples.store(provides, Ordering::Release);
            self.output_size.store(info.cbSize, Ordering::Release);
        }
    }

    /// Gives one frame to the transform (which asked for it), forcing a keyframe first when
    /// the frame wants one. Called with the state locked.
    fn process_input(&self, st: &mut State, pending: Pending) -> Result<(), Error> {
        let m = &self.mft;
        if pending.keyframe {
            set_u32(&m.codec, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        }
        match unsafe { m.transform.ProcessInput(m.input_id, &pending.sample, 0) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // not now after all: it goes first when the transform asks again
                st.queue.push_front(pending);
                Ok(())
            }
            Err(e) => Err(failure("the hardware encoder refused a frame", e)),
        }
    }

    /// The transform asked for a frame: the oldest waiting one, or a credit for the next.
    fn need_input(&self, shared: &Shared) {
        let mut failed = None;
        {
            let mut st = self.state.lock();
            match st.queue.pop_front() {
                Some(pending) => {
                    let hns = pending.hns;
                    if let Err(e) = self.process_input(&mut st, pending) {
                        if let Some(f) = take_in_flight(&mut st, hns) {
                            release_in_flight(shared, self, &f);
                        }
                        failed = Some(e);
                    }
                }
                None => st.credits += 1,
            }
            self.changed.notify_all();
        }
        if let Some(e) = failed {
            (shared.sink.lock())(VideoOutput::Failed(e));
        }
    }

    /// The transform has a picture: takes it out and sends it to the sink.
    fn have_output(&self, shared: &Shared) {
        let outputs = match self.take_output(shared) {
            Ok(o) => o,
            Err(e) => vec![VideoOutput::Failed(e)],
        };
        if outputs.is_empty() {
            return;
        }
        let mut sink = shared.sink.lock();
        for out in outputs {
            if matches!(out, VideoOutput::Packet(_)) {
                shared.packets.fetch_add(1, Ordering::Relaxed);
            }
            sink(out);
        }
    }

    fn take_output(&self, shared: &Shared) -> Result<Vec<VideoOutput>, Error> {
        let m = &self.mft;
        let given = if self.provides_samples.load(Ordering::Acquire) {
            None
        } else {
            let size = self.output_size.load(Ordering::Acquire);
            let fallback = shared.settings.width * shared.settings.height * 3 / 2;
            Some(mf::output_sample(size.max(fallback))?)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: m.output_id,
            pSample: ManuallyDrop::new(given),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = unsafe { m.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let [buffer] = buffers;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        if let Err(e) = result {
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                self.renegotiate()?;
                return Ok(Vec::new());
            }
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(Vec::new());
            }
            return Err(failure("the hardware encoder failed a frame", e));
        }
        let Some(sample) = sample else {
            return Ok(Vec::new());
        };
        self.read_sample(shared, &sample)
    }

    /// The transform changed its output: takes the type it now offers.
    fn renegotiate(&self) -> Result<(), Error> {
        let m = &self.mft;
        unsafe {
            let t = m
                .transform
                .GetOutputAvailableType(m.output_id, 0)
                .map_err(platform("the hardware encoder changed its output and offered nothing"))?;
            m.transform
                .SetOutputType(m.output_id, &t, 0)
                .map_err(platform("the hardware encoder changed its output and refused it"))?;
        }
        self.read_output_info();
        Ok(())
    }

    /// The parameter sets the transform's current output type carries (Annex B).
    fn sequence_header(&self) -> Vec<Vec<u8>> {
        let m = &self.mft;
        let Ok(t) = (unsafe { m.transform.GetOutputCurrentType(m.output_id) }) else {
            return Vec::new();
        };
        let Ok(size) = (unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
            return Vec::new();
        };
        let mut blob = vec![0u8; size as usize];
        if unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.is_err() {
            return Vec::new();
        }
        nal::split(&blob, NalLayout::AnnexB).into_iter().map(<[u8]>::to_vec).collect()
    }

    fn read_sample(&self, shared: &Shared, sample: &IMFSample) -> Result<Vec<VideoOutput>, Error> {
        let settings = &shared.settings;
        let hevc = matches!(settings.codec, VideoCodec::Hevc { .. });
        let bytes = mf::sample_bytes(sample)?;
        let hns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let dts_hns = unsafe { sample.GetUINT64(&MFSampleExtension_DecodeTimestamp) }.ok().map(|v| v as i64);
        let frame = frame_ns(settings);

        let mut out = Vec::new();
        let (pts_ns, order_dts) = {
            let mut st = self.state.lock();
            let found = st.in_flight.iter().position(|f| f.hns == hns).and_then(|at| st.in_flight.remove(at));
            let pts_ns = found.as_ref().map_or(hns * 100, |f| f.pts_ns);
            if let Some(f) = &found {
                release_in_flight(shared, self, f);
            }
            // frames the encoder skipped (older than a second before this one) are dropped
            while let Some(old) = st.in_flight.front() {
                if old.pts_ns >= pts_ns - 1_000_000_000 {
                    break;
                }
                let Some(old) = st.in_flight.pop_front() else { break };
                release_in_flight(shared, self, &old);
                if let Some(o) = st.order.iter().position(|&p| p == old.pts_ns) {
                    st.order.remove(o);
                }
                out.push(VideoOutput::Dropped { pts_ns: old.pts_ns });
            }
            (pts_ns, st.order.pop_front())
        };
        let dts_ns = match dts_hns {
            Some(d) => (d * 100).min(pts_ns),
            None if self.mft.bframes > 0 => order_dts.unwrap_or(pts_ns) - self.mft.bframes as i64 * frame,
            None => pts_ns,
        };

        let mut sets = nal::parameter_sets_in(&bytes, NalLayout::AnnexB, hevc);
        let mut format = shared.format.lock();
        if sets.is_empty() && keyframe && format.is_none() {
            sets = self.sequence_header();
        }
        let mut new_format = None;
        if !sets.is_empty() && format.as_ref().is_none_or(|(known, _)| *known != sets) {
            let fps = (settings.fps_num, settings.fps_den);
            match VideoFormat::new(settings.codec, settings.width, settings.height, fps, sets.clone(), settings.color) {
                Ok(f) => {
                    let f = Arc::new(f);
                    *format = Some((sets, f.clone()));
                    new_format = Some(f);
                }
                Err(e) => tracing::warn!(error = %e, "the encoder's parameter sets didn't parse"),
            }
        }
        drop(format);

        out.push(VideoOutput::Packet(EncodedVideo {
            data: nal::to_sample(&bytes, NalLayout::AnnexB, hevc),
            pts_ns,
            dts_ns,
            keyframe,
            codec: settings.codec,
            format: new_format,
        }));
        Ok(out)
    }
}

/// The event thread: the transform asks for input, has output, finishes draining.
fn run(shared: Arc<Shared>, pipe: Arc<Pipeline>) {
    mf::join_apartment();
    loop {
        if pipe.stop.load(Ordering::Acquire) {
            break;
        }
        let event = match unsafe { pipe.mft.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
            Ok(e) => e,
            Err(e) => {
                if !pipe.stop.load(Ordering::Acquire) && e.code() != MF_E_SHUTDOWN {
                    (shared.sink.lock())(VideoOutput::Failed(failure("the hardware encoder stopped", e)));
                }
                break;
            }
        };
        let kind = unsafe { event.GetType() }.unwrap_or(0);
        if let Ok(status) = unsafe { event.GetStatus() }
            && status.is_err()
        {
            let e = if device_lost(status) {
                Error::Invalidated
            } else {
                mf::status("the hardware encoder failed", status)
            };
            (shared.sink.lock())(VideoOutput::Failed(e));
            continue;
        }
        if kind == METransformNeedInput.0 as u32 {
            pipe.need_input(&shared);
        } else if kind == METransformHaveOutput.0 as u32 {
            pipe.have_output(&shared);
        } else if kind == METransformDrainComplete.0 as u32 {
            let mut st = pipe.state.lock();
            st.draining = false;
            pipe.changed.notify_all();
        }
    }
    pipe.exited.store(true, Ordering::Release);
}

/// Stops a pipeline: flushes the transform, shuts it down (which ends the event thread's
/// wait), joins the thread, and frees every frame it still held.
fn stop(shared: &Shared, pipe: &Arc<Pipeline>, thread: Option<JoinHandle<()>>) {
    pipe.stop.store(true, Ordering::Release);
    let m = &pipe.mft;
    unsafe {
        let _ = m.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        let _ = m.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        if let Ok(shutdown) = m.transform.cast::<IMFShutdown>() {
            let _ = shutdown.Shutdown();
        }
        let _ = m.activate.ShutdownObject();
    }
    if let Some(thread) = thread {
        // a transform whose shutdown doesn't end the wait leaves its thread behind rather than
        // hanging the caller
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pipe.exited.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if pipe.exited.load(Ordering::Acquire) {
            let _ = thread.join();
        } else {
            tracing::warn!("the hardware encoder's event thread didn't stop; leaving it");
        }
    }
    let mut st = pipe.state.lock();
    st.queue.clear();
    st.order.clear();
    for f in st.in_flight.drain(..) {
        release_in_flight(shared, pipe, &f);
    }
}
