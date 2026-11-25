//! Media Foundation: cameras and capture cards, their formats, a source reader per capture, and
//! word of devices coming and going.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Security::Authorization::AppCapabilityAccess::{AppCapability, AppCapabilityAccessStatus};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_0,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CM_Unregister_Notification, CR_SUCCESS,
    HCMNOTIFICATION,
};
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::KernelStreaming::{KSCATEGORY_VIDEO, KSCATEGORY_VIDEO_CAMERA};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaSource,
    IMFMediaType, IMFMediaTypeHandler, IMFSample, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_HW_MFT_FAILED_START_STREAMING, MF_E_SHUTDOWN,
    MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_RATE_RANGE_MAX,
    MF_MT_FRAME_RATE_RANGE_MIN, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateDeviceSource, MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources,
    MFMediaType_Video, MFNominalRange_0_255, MFSTARTUP_FULL, MFStartup, MFVideoFormat_NV12, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, HSTRING, Interface, PWSTR};

use super::gpu::{Gpu, Image, Memory, Out, Publisher};
use super::{com, fail, hns_ns, host_now_ns};
use crate::camera::{Access, Camera, CameraKind, Config, DeviceEvent, Format, FpsRange, choose};
use crate::{Error, Frame, PixelFormat, VideoSink};

/// Starts Media Foundation for the process (once; it stays started).
fn startup() -> Result<(), Error> {
    static STARTED: OnceLock<Result<(), Error>> = OnceLock::new();
    com();
    STARTED
        .get_or_init(|| {
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| fail("starting Media Foundation", e))
        })
        .clone()
}

/// Windows' camera privacy setting for this app ("Let desktop apps access your camera").
pub fn access() -> Access {
    com();
    let status = AppCapability::Create(&HSTRING::from("webcam")).and_then(|c| c.CheckAccess());
    match status {
        Ok(AppCapabilityAccessStatus::DeniedByUser) => Access::Denied,
        Ok(AppCapabilityAccessStatus::DeniedBySystem) => Access::Restricted,
        Ok(AppCapabilityAccessStatus::UserPromptRequired) => Access::NotAsked,
        _ => Access::Allowed,
    }
}

fn attributes(count: u32) -> Result<IMFAttributes, Error> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, count) }.map_err(|e| fail("making Media Foundation attributes", e))?;
    attributes.ok_or_else(|| Error::Failed("making Media Foundation attributes".into()))
}

fn string(activate: &IMFActivate, key: &GUID) -> String {
    let mut value = PWSTR::null();
    let mut len = 0u32;
    if unsafe { activate.GetAllocatedString(key, &mut value, &mut len) }.is_err() || value.is_null() {
        return String::new();
    }
    let text = unsafe { value.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(value.0 as *const _)) };
    text
}

/// The video capture devices there are now.
fn devices() -> Vec<IMFActivate> {
    if startup().is_err() {
        return Vec::new();
    }
    let Ok(attributes) = attributes(1) else { return Vec::new() };
    if unsafe {
        attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
    }
    .is_err()
    {
        return Vec::new();
    }
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe { MFEnumDeviceSources(&attributes, &mut list, &mut count) }.is_err() || list.is_null() {
        return Vec::new();
    }
    let devices = (0..count as usize).filter_map(|i| unsafe { std::ptr::read(list.add(i)) }).collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    devices
}

/// What a camera is, from its symbolic link (`\\?\usb#vid_046d&pid_085e…`).
fn camera_of(activate: &IMFActivate) -> Camera {
    let id = string(activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK);
    let name = string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
    let lower = id.to_ascii_lowercase();
    let field = |key: &str| {
        lower
            .split(['#', '&'])
            .find_map(|part| part.strip_prefix(key))
            .map(|v| v.to_ascii_uppercase())
            .unwrap_or_default()
    };
    let (vendor, product) = (field("vid_"), field("pid_"));
    let kind = if lower.contains("usb#") || lower.contains("thunderbolt") || lower.contains("pci#") {
        CameraKind::External
    } else {
        CameraKind::Other
    };
    Camera {
        id,
        name,
        model: if product.is_empty() { String::new() } else { format!("PID {product}") },
        manufacturer: if vendor.is_empty() { String::new() } else { format!("VID {vendor}") },
        kind,
        muxed: false,
        in_use: false,
        suspended: false,
    }
}

pub fn cameras() -> Vec<Camera> {
    devices().iter().map(camera_of).collect()
}

/// Opens the device whose symbolic link is `id` (this doesn't start it streaming).
fn open(id: &str) -> Result<IMFMediaSource, Error> {
    startup()?;
    let attributes = attributes(2)?;
    unsafe {
        attributes
            .SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
            .map_err(|e| fail("opening the camera", e))?;
        attributes
            .SetString(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, &HSTRING::from(id))
            .map_err(|e| fail("opening the camera", e))?;
    }
    unsafe { MFCreateDeviceSource(&attributes) }.map_err(|e| {
        if e.code() == E_ACCESSDENIED {
            Error::Permission("the camera (Windows' camera privacy setting)".into())
        } else if cameras().iter().any(|c| c.id.eq_ignore_ascii_case(id)) {
            fail("opening the camera", e)
        } else {
            Error::NotFound(id.into())
        }
    })
}

/// The device's first video stream's media type handler and its native types.
fn native_types(source: &IMFMediaSource) -> Result<(IMFMediaTypeHandler, Vec<IMFMediaType>), Error> {
    let e = |e| fail("reading the camera's formats", e);
    let presentation = unsafe { source.CreatePresentationDescriptor() }.map_err(e)?;
    let mut selected = windows::core::BOOL(0);
    let mut stream = None;
    unsafe { presentation.GetStreamDescriptorByIndex(0, &mut selected, &mut stream) }.map_err(e)?;
    let stream = stream.ok_or_else(|| Error::Failed("the camera has no video stream".into()))?;
    let handler = unsafe { stream.GetMediaTypeHandler() }.map_err(e)?;
    let count = unsafe { handler.GetMediaTypeCount() }.map_err(e)?;
    let types = (0..count).filter_map(|i| unsafe { handler.GetMediaTypeByIndex(i) }.ok()).collect();
    Ok((handler, types))
}

/// A pair packed in a 64-bit attribute (size: width, height; rate: numerator, denominator).
fn pair(media: &IMFMediaType, key: &GUID) -> Option<(u32, u32)> {
    let v = unsafe { media.GetUINT64(key) }.ok()?;
    Some(((v >> 32) as u32, v as u32))
}

fn rate(media: &IMFMediaType, key: &GUID) -> Option<f64> {
    let (n, d) = pair(media, key)?;
    (d != 0).then(|| n as f64 / d as f64)
}

/// A media subtype as a four-character code (most subtypes are a FOURCC in the GUID's first
/// field; the 32- and 24-bit RGB ones are Direct3D format numbers).
fn fourcc_of(subtype: &GUID) -> u32 {
    match subtype.data1 {
        20 => u32::from_be_bytes(*b"RGB3"),
        21 | 22 => u32::from_be_bytes(*b"BGRA"),
        n => u32::from_be_bytes(n.to_le_bytes()),
    }
}

fn format_of(index: usize, media: &IMFMediaType) -> Format {
    let size = pair(media, &MF_MT_FRAME_SIZE).unwrap_or((0, 0));
    let subtype = unsafe { media.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
    let nominal = rate(media, &MF_MT_FRAME_RATE).unwrap_or(0.0);
    let min = rate(media, &MF_MT_FRAME_RATE_RANGE_MIN).unwrap_or(nominal);
    let max = rate(media, &MF_MT_FRAME_RATE_RANGE_MAX).unwrap_or(nominal);
    Format { index, size, fourcc: fourcc_of(&subtype), fps: vec![FpsRange { min: min.min(max), max: max.max(min) }] }
}

pub fn formats(id: &str) -> Result<Vec<Format>, Error> {
    let source = open(id)?;
    let listed =
        native_types(&source).map(|(_, types)| types.iter().enumerate().map(|(i, t)| format_of(i, t)).collect());
    let _ = unsafe { source.Shutdown() };
    listed
}

/// A frame rate as the ratio Media Foundation takes: the NTSC rates (29.97…) exact.
fn ratio(fps: f64) -> (u32, u32) {
    for n in [24u32, 30, 48, 60, 120, 240] {
        let ntsc = n as f64 * 1000.0 / 1001.0;
        if (fps - ntsc).abs() < 0.005 {
            return (n * 1000, 1001);
        }
    }
    if (fps - fps.round()).abs() < 0.005 {
        return (fps.round() as u32, 1);
    }
    ((fps * 1000.0).round() as u32, 1000)
}

/// Objects a capture's thread uses.
struct Reader {
    reader: IMFSourceReader,
    /// The picture as read: its size, stride in memory (bytes), format, and whether NV12 is full
    /// range.
    size: (u32, u32),
    stride: usize,
    format: PixelFormat,
    /// The device the reader hands textures out on, when it does: the capture's copies must be
    /// made on the same one.
    gpu: Option<Arc<Gpu>>,
}

// Media Foundation's source reader and sources are free-threaded.
unsafe impl Send for Reader {}

struct Source(IMFMediaSource);
unsafe impl Send for Source {}
unsafe impl Sync for Source {}

/// A running camera capture.
pub struct Session {
    format: Format,
    fps: f64,
    stop: Arc<AtomicBool>,
    source: Arc<Source>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    pub fn start(id: &str, config: &Config, sink: Box<dyn VideoSink>) -> Result<Session, Error> {
        if access() == Access::Denied {
            return Err(Error::Permission("the camera (Windows' camera privacy setting)".into()));
        }
        let source = open(id)?;
        let started = Session::configure(&source, config);
        let (reader, format, fps) = match started {
            Ok(r) => r,
            Err(e) => {
                let _ = unsafe { source.Shutdown() };
                return Err(e);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let source = Arc::new(Source(source));
        let thread_stop = stop.clone();
        let thread_source = source.clone();
        let depth = 5;
        let thread = std::thread::Builder::new()
            .name("lightair-camera".into())
            .spawn(move || {
                let _keep = thread_source;
                read(reader, sink, thread_stop, depth)
            })
            .map_err(|e| Error::Failed(format!("starting the camera thread: {e}")))?;
        Ok(Session { format, fps, stop, source, thread: Some(thread) })
    }

    /// Sets the device's format and frame rate, and makes the reader that turns its pictures
    /// into the pixel format asked for (decoding MJPEG and converting on the GPU where it can).
    fn configure(source: &IMFMediaSource, config: &Config) -> Result<(Reader, Format, f64), Error> {
        let (handler, types) = native_types(source)?;
        let formats: Vec<Format> = types.iter().enumerate().map(|(i, t)| format_of(i, t)).collect();
        let index = choose(&formats, config).ok_or_else(|| Error::NotFound("a format the camera runs in".into()))?;
        let format = formats[index].clone();
        let native = &types[index];
        let fps = match config.fps {
            Some(fps) if format.runs_at(fps) => fps,
            _ => format.max_fps(),
        };
        // the range's own ratio when it's one of its ends, so 29.97 stays exact
        let ends = [MF_MT_FRAME_RATE_RANGE_MAX, MF_MT_FRAME_RATE_RANGE_MIN, MF_MT_FRAME_RATE];
        let exact = ends.iter().find_map(|key| {
            let (n, d) = pair(native, key)?;
            (d != 0 && (n as f64 / d as f64 - fps).abs() < 0.01).then_some((n, d))
        });
        let (n, d) = exact.unwrap_or_else(|| ratio(fps));
        unsafe {
            let _ = native.SetUINT64(&MF_MT_FRAME_RATE, ((n as u64) << 32) | d as u64);
            handler.SetCurrentMediaType(native).map_err(|e| {
                if e.code() == E_ACCESSDENIED {
                    Error::Permission("the camera".into())
                } else {
                    fail("setting the camera's format", e)
                }
            })?;
        }

        let gpu = Gpu::engine().ok().filter(|g| g.video);
        let (reader, gpu) = match gpu.as_ref().and_then(|g| make_reader(source, Some(g)).ok()) {
            Some(reader) => (reader, gpu),
            None => (make_reader(source, None)?, None),
        };

        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let wanted = match config.pixel_format {
            PixelFormat::Nv12Video | PixelFormat::Nv12Full => MFVideoFormat_NV12,
            _ => MFVideoFormat_RGB32,
        };
        let output = unsafe { MFCreateMediaType() }.map_err(|e| fail("setting the camera's output", e))?;
        unsafe {
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| fail("setting the camera's output", e))?;
            output.SetGUID(&MF_MT_SUBTYPE, &wanted).map_err(|e| fail("setting the camera's output", e))?;
            reader.SetCurrentMediaType(stream, None, &output).map_err(|e| fail("setting the camera's output", e))?;
        }
        let current =
            unsafe { reader.GetCurrentMediaType(stream) }.map_err(|e| fail("reading the camera's output", e))?;
        let size = pair(&current, &MF_MT_FRAME_SIZE).unwrap_or(format.size);
        let nv12 = wanted == MFVideoFormat_NV12;
        let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map(|s| (s as i32).unsigned_abs() as usize)
            .unwrap_or(if nv12 { size.0 as usize } else { size.0 as usize * 4 });
        let full =
            unsafe { current.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) }.is_ok_and(|r| r == MFNominalRange_0_255.0 as u32);
        let pixel = match (nv12, full) {
            (false, _) => PixelFormat::Bgra,
            (true, true) => PixelFormat::Nv12Full,
            (true, false) => PixelFormat::Nv12Video,
        };
        Ok((Reader { reader, size, stride, format: pixel, gpu }, format, fps))
    }

    pub fn format(&self) -> &Format {
        &self.format
    }

    pub fn fps(&self) -> f64 {
        self.fps
    }
}

/// A source reader for `source`, handing out Direct3D 11 textures on `gpu` when given.
fn make_reader(source: &IMFMediaSource, gpu: Option<&Arc<Gpu>>) -> Result<IMFSourceReader, Error> {
    let attributes = attributes(3)?;
    unsafe {
        let _ = attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1);
        let _ = attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1);
        if let Some(gpu) = gpu {
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|e| fail("sharing the Direct3D device", e))?;
            let manager = manager.ok_or_else(|| Error::Failed("sharing the Direct3D device".into()))?;
            manager.ResetDevice(&gpu.device, token).map_err(|e| fail("sharing the Direct3D device", e))?;
            attributes
                .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)
                .map_err(|e| fail("sharing the Direct3D device", e))?;
        }
        MFCreateSourceReaderFromMediaSource(source, &attributes).map_err(|e| fail("reading the camera", e))
    }
}

/// The texture a sample's buffer is in, and which of its array slices.
fn texture_of(buffer: &IMFMediaBuffer) -> Option<(ID3D11Texture2D, u32)> {
    let dxgi: IMFDXGIBuffer = buffer.cast().ok()?;
    let mut texture: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture) }.ok()?;
    if texture.is_null() {
        return None;
    }
    let texture = unsafe { ID3D11Texture2D::from_raw(texture) };
    let slice = unsafe { dxgi.GetSubresourceIndex() }.unwrap_or(0);
    Some((texture, slice))
}

/// A sample's pixels copied out of memory, rows packed to `row` bytes.
fn memory_of(buffer: &IMFMediaBuffer, reader: &Reader) -> Option<Memory> {
    let (w, h) = (reader.size.0 as usize, reader.size.1 as usize);
    let (row, rows) = match reader.format {
        PixelFormat::Bgra => (w * 4, h),
        _ => (w, h + h / 2),
    };
    let mut data = Vec::with_capacity(row * rows);
    if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        unsafe { two_d.Lock2D(&mut start, &mut pitch) }.ok()?;
        // a negative pitch is a bottom-up picture: its first row is at `start` all the same
        for r in 0..rows {
            let line = unsafe { std::slice::from_raw_parts(start.offset(r as isize * pitch as isize), row) };
            data.extend_from_slice(line);
        }
        let _ = unsafe { two_d.Unlock2D() };
    } else {
        let mut start: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut start, None, Some(&mut len)) }.ok()?;
        let stride = reader.stride.max(row);
        for r in 0..rows {
            if (r + 1) * stride > len as usize && r * stride + row > len as usize {
                break;
            }
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(start.add(r * stride), row) });
        }
        let _ = unsafe { buffer.Unlock() };
    }
    (data.len() == row * rows).then_some(Memory { data, stride: row })
}

/// The capture's thread: reads samples until stopped (or the device goes), hands each over.
fn read(reader: Reader, mut sink: Box<dyn VideoSink>, stop: Arc<AtomicBool>, depth: u32) {
    com();
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut publisher = reader.gpu.clone().map(|gpu| Publisher::new(gpu, depth));
    // the device's clock to the host's: none when its times are already the system's (the
    // performance counter), else fixed at the first sample
    let mut offset: Option<i64> = None;
    while !stop.load(Ordering::Relaxed) {
        let mut flags = 0u32;
        let mut time = 0i64;
        let mut sample: Option<IMFSample> = None;
        let read =
            unsafe { reader.reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)) };
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = read {
            let reason = match e.code() {
                MF_E_SHUTDOWN => return,
                MF_E_VIDEO_RECORDING_DEVICE_INVALIDATED => "the camera was disconnected".to_string(),
                MF_E_HW_MFT_FAILED_START_STREAMING => "the camera is in use by another app".to_string(),
                E_ACCESSDENIED => "camera access was turned off".to_string(),
                _ => format!("the camera stopped: {}", e.message()),
            };
            sink.stopped(&reason);
            return;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            sink.stopped("the camera was disconnected");
            return;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            sink.stopped("the camera stopped sending pictures");
            return;
        }
        let Some(sample) = sample else { continue };
        let Ok(buffer) = (unsafe { sample.GetBufferByIndex(0) }) else { continue };
        let device_ns = hns_ns(time);
        let now = host_now_ns();
        let shift = *offset.get_or_insert(if (now - device_ns).abs() < 5_000_000_000 { 0 } else { now - device_ns });
        let host_ns = device_ns + shift;

        // a texture whose copy fails (every shared texture still held) is dropped, not read back
        let on_gpu = match (texture_of(&buffer), publisher.as_mut()) {
            (Some((texture, slice)), Some(publisher)) => {
                let out = Out { size: None, format: reader.format };
                let Some(published) = publisher.publish(&texture, slice, None, &out) else { continue };
                Some(published)
            }
            _ => None,
        };
        let frame = match on_gpu {
            Some(p) => Frame {
                image: p.image,
                host_ns,
                size: p.size,
                format: p.format,
                content: None,
                content_scale: None,
                scale: None,
            },
            None => {
                let Some(memory) = memory_of(&buffer, &reader) else { continue };
                Frame {
                    image: Image::Memory(memory),
                    host_ns,
                    size: reader.size,
                    format: reader.format,
                    content: None,
                    content_scale: None,
                    scale: None,
                }
            }
        };
        sink.frame(frame);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // ends a read that's waiting for the device
        let _ = unsafe { self.source.0.Shutdown() };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ------------------------------------------------------------------ devices coming and going

/// Hears video devices come and go (Configuration Manager notifications for cameras and for
/// video capture devices in general, which capture cards are), and tells the callback which
/// camera it was by comparing Media Foundation's list before and after.
pub struct Watch {
    notifications: Vec<HCMNOTIFICATION>,
    context: *mut mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

unsafe extern "system" fn changed(
    _: HCMNOTIFICATION,
    context: *const std::ffi::c_void,
    _: CM_NOTIFY_ACTION,
    _: *const CM_NOTIFY_EVENT_DATA,
    _: u32,
) -> u32 {
    let tx = unsafe { &*(context as *const mpsc::Sender<()>) };
    let _ = tx.send(());
    0
}

impl Watch {
    pub fn new(f: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Watch {
        let (tx, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("lightair-camera-watch".into())
            .spawn(move || {
                com();
                let list = || cameras().into_iter().map(|c| (c.id, c.name)).collect::<Vec<_>>();
                let mut known = list();
                while rx.recv().is_ok() {
                    // one plug-in sends several notifications: let them settle, then compare
                    std::thread::sleep(Duration::from_millis(300));
                    while rx.try_recv().is_ok() {}
                    let now = list();
                    for (id, name) in &now {
                        if !known.iter().any(|(k, _)| k.eq_ignore_ascii_case(id)) {
                            f(DeviceEvent::Connected { id: id.clone(), name: name.clone() });
                        }
                    }
                    for (id, name) in &known {
                        if !now.iter().any(|(k, _)| k.eq_ignore_ascii_case(id)) {
                            f(DeviceEvent::Disconnected { id: id.clone(), name: name.clone() });
                        }
                    }
                    known = now;
                }
            })
            .ok();
        let context = Box::into_raw(Box::new(tx));
        let mut notifications = Vec::new();
        for class in [KSCATEGORY_VIDEO_CAMERA, KSCATEGORY_VIDEO] {
            let filter = CM_NOTIFY_FILTER {
                cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
                Flags: 0,
                FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
                Reserved: 0,
                u: CM_NOTIFY_FILTER_0 { DeviceInterface: CM_NOTIFY_FILTER_0_0 { ClassGuid: class } },
            };
            let mut handle = HCMNOTIFICATION::default();
            let done =
                unsafe { CM_Register_Notification(&filter, Some(context as *const _), Some(changed), &mut handle) };
            if done == CR_SUCCESS {
                notifications.push(handle);
            }
        }
        Watch { notifications, context, thread }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // unregistering waits for callbacks in flight, so the sender is free to go after it
        for handle in self.notifications.drain(..) {
            let _ = unsafe { CM_Unregister_Notification(handle) };
        }
        drop(unsafe { Box::from_raw(self.context) });
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
