//! AVFoundation: cameras and capture cards, their formats, sessions capturing them, and word
//! of devices coming and going.

use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceFormat, AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceTypeBuiltInWideAngleCamera,
    AVCaptureDeviceTypeContinuityCamera, AVCaptureDeviceTypeDeskViewCamera, AVCaptureDeviceTypeExternal,
    AVCaptureDeviceWasConnectedNotification, AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput,
    AVCaptureSession, AVCaptureSessionErrorKey, AVCaptureSessionRuntimeErrorNotification, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeMuxed, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSObject, NSObjectProtocol,
    NSString,
};
use parking_lot::Mutex;

use super::{Clock, error_text, frame_of};
use crate::camera::{Access, Camera, CameraKind, Config, DeviceEvent, Format, FpsRange, choose};
use crate::{Error, VideoSink};

fn video() -> &'static NSString {
    // (always there on macOS; the binding is optional for older systems)
    unsafe { AVMediaTypeVideo }.expect("AVMediaTypeVideo")
}

pub fn access() -> Access {
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(video()) } {
        AVAuthorizationStatus::Authorized => Access::Allowed,
        AVAuthorizationStatus::Denied => Access::Denied,
        AVAuthorizationStatus::Restricted => Access::Restricted,
        _ => Access::NotAsked,
    }
}

/// Asks the user for the camera if they haven't been asked, and waits for the answer.
fn ask_access() -> Result<(), Error> {
    match access() {
        Access::Allowed => Ok(()),
        Access::Denied => Err(Error::Permission("camera access is denied".into())),
        Access::Restricted => Err(Error::Permission("camera access is restricted".into())),
        Access::NotAsked => {
            // (the user may take their time over the prompt)
            let (tx, rx) = std::sync::mpsc::channel();
            let block = RcBlock::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video(), &block) };
            match rx.recv_timeout(std::time::Duration::from_secs(120)) {
                Ok(true) => Ok(()),
                Ok(false) => Err(Error::Permission("camera access was denied".into())),
                Err(_) => Err(Error::Permission("camera access wasn't answered".into())),
            }
        }
    }
}

/// Every camera and capture card connected now.
fn devices() -> Retained<NSArray<AVCaptureDevice>> {
    unsafe {
        let types = NSArray::from_slice(&[
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            AVCaptureDeviceTypeExternal,
            AVCaptureDeviceTypeContinuityCamera,
            AVCaptureDeviceTypeDeskViewCamera,
        ]);
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &types,
            None,
            AVCaptureDevicePosition::Unspecified,
        )
        .devices()
    }
}

fn camera_of(device: &AVCaptureDevice) -> Camera {
    unsafe {
        let kind = device.deviceType();
        let kind: &NSString = &kind;
        let kind = if kind == AVCaptureDeviceTypeBuiltInWideAngleCamera {
            CameraKind::BuiltIn
        } else if kind == AVCaptureDeviceTypeContinuityCamera {
            CameraKind::Continuity
        } else if kind == AVCaptureDeviceTypeDeskViewCamera {
            CameraKind::DeskView
        } else if kind == AVCaptureDeviceTypeExternal {
            CameraKind::External
        } else {
            CameraKind::Other
        };
        Camera {
            id: device.uniqueID().to_string(),
            name: device.localizedName().to_string(),
            model: device.modelID().to_string(),
            manufacturer: device.manufacturer().to_string(),
            kind,
            muxed: AVMediaTypeMuxed.is_some_and(|m| device.hasMediaType(m)),
            in_use: device.isInUseByAnotherApplication(),
            suspended: device.isSuspended(),
        }
    }
}

pub fn cameras() -> Vec<Camera> {
    devices().iter().map(|d| camera_of(&d)).collect()
}

fn device(id: &str) -> Result<Retained<AVCaptureDevice>, Error> {
    unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)) }
        .ok_or_else(|| Error::NotFound(format!("camera {id}")))
}

fn format_of(index: usize, format: &AVCaptureDeviceFormat) -> Format {
    unsafe {
        let description = format.formatDescription();
        let size = CMVideoFormatDescriptionGetDimensions(&description);
        let fps = format
            .videoSupportedFrameRateRanges()
            .iter()
            .map(|r| FpsRange { min: r.minFrameRate(), max: r.maxFrameRate() })
            .collect();
        Format {
            index,
            size: (size.width.max(0) as u32, size.height.max(0) as u32),
            fourcc: description.media_sub_type(),
            fps,
        }
    }
}

fn formats_of(device: &AVCaptureDevice) -> Vec<Format> {
    unsafe { device.formats() }.iter().enumerate().map(|(i, f)| format_of(i, &f)).collect()
}

pub fn formats(id: &str) -> Result<Vec<Format>, Error> {
    let device = device(id)?;
    Ok(formats_of(&device))
}

/// The frame duration for `fps` in a format: the range's own where `fps` is its fastest (so
/// 29.97 stays exact), else 1/fps.
fn frame_duration(format: &AVCaptureDeviceFormat, fps: f64) -> Option<CMTime> {
    let ranges = unsafe { format.videoSupportedFrameRateRanges() };
    let range = ranges.iter().find(|r| unsafe { fps >= r.minFrameRate() - 0.01 && fps <= r.maxFrameRate() + 0.01 })?;
    unsafe {
        if (range.maxFrameRate() - fps).abs() < 0.01 {
            return Some(range.minFrameDuration());
        }
        if (range.minFrameRate() - fps).abs() < 0.01 {
            return Some(range.maxFrameDuration());
        }
    }
    Some(CMTime { value: 1000, timescale: (fps * 1000.0).round() as i32, flags: CMTimeFlags::Valid, epoch: 0 })
}

// ------------------------------------------------------------------ the session's output

pub struct Ivars {
    sink: Mutex<Box<dyn VideoSink>>,
    clock: OnceLock<Clock>,
    alive: AtomicBool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Output doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Output {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            let ivars = self.ivars();
            if !ivars.alive.load(Ordering::Acquire) {
                return;
            }
            if let Some(frame) = frame_of(sample, ivars.clock.get()) {
                ivars.sink.lock().frame(frame);
            }
        }
    }
);

impl Output {
    fn new(sink: Box<dyn VideoSink>) -> Retained<Output> {
        let this = Output::alloc().set_ivars(Ivars {
            sink: Mutex::new(sink),
            clock: OnceLock::new(),
            alive: AtomicBool::new(true),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn stopped(&self, reason: &str) {
        let ivars = self.ivars();
        if ivars.alive.swap(false, Ordering::AcqRel) {
            ivars.sink.lock().stopped(reason);
        }
    }
}

/// A notification observer, removed when dropped.
struct Observer(Retained<ProtocolObject<dyn NSObjectProtocol>>);

impl Observer {
    fn add(name: &NSString, object: Option<&AnyObject>, f: impl Fn(&NSNotification) + 'static) -> Observer {
        let block = RcBlock::new(move |n: NonNull<NSNotification>| f(unsafe { n.as_ref() }));
        Observer(unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(name),
                object,
                None,
                &block,
            )
        })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(self.0.as_ref()) };
    }
}

// ------------------------------------------------------------------ sessions

pub struct Session {
    session: Retained<AVCaptureSession>,
    output: Retained<Output>,
    _video: Retained<AVCaptureVideoDataOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    _observers: Vec<Observer>,
    format: Format,
    fps: f64,
}

// AVFoundation's sessions may be started and stopped from any thread; the output's state is
// behind a lock and atomics, and the observers only hold the output.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

/// What the video output is asked to make: `fourcc` pixels in IOSurfaces Metal can use.
fn video_settings(fourcc: u32) -> Retained<NSDictionary<NSString, AnyObject>> {
    unsafe {
        let format: &NSString = &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
        let iosurface: &NSString = &*(kCVPixelBufferIOSurfacePropertiesKey as *const _ as *const NSString);
        let metal: &NSString = &*(kCVPixelBufferMetalCompatibilityKey as *const _ as *const NSString);
        let empty = NSDictionary::<NSString, AnyObject>::new();
        let number = NSNumber::new_u32(fourcc);
        let yes = NSNumber::new_bool(true);
        let values: [&AnyObject; 3] = [&number, &empty, &yes];
        NSDictionary::from_slices(&[format, iosurface, metal], &values)
    }
}

impl Session {
    pub fn start(id: &str, config: &Config, sink: Box<dyn VideoSink>) -> Result<Session, Error> {
        let device = device(id)?;
        let all = formats_of(&device);
        let chosen = choose(&all, config).ok_or_else(|| Error::Failed(format!("camera {id} lists no formats")))?;
        let format = all[chosen].clone();
        let av_format = unsafe { device.formats() }.objectAtIndex(format.index);
        let fps = config.fps.filter(|&f| format.runs_at(f)).unwrap_or_else(|| format.max_fps());

        ask_access()?;
        let input = unsafe { AVCaptureDeviceInput::initWithDevice_error(AVCaptureDeviceInput::alloc(), &device) }
            .map_err(|e| {
                let text = error_text(&e);
                if access() == Access::Allowed {
                    Error::Failed(format!("opening camera {id}: {text}"))
                } else {
                    Error::Permission(text)
                }
            })?;

        let session = unsafe { AVCaptureSession::new() };
        let output = Output::new(sink);
        let video = unsafe { AVCaptureVideoDataOutput::new() };
        let queue = DispatchQueue::new("app.lightair.capture.camera", None);
        unsafe {
            video.setAlwaysDiscardsLateVideoFrames(true);
            video.setVideoSettings(Some(&video_settings(config.pixel_format.fourcc())));
            video.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*output)), Some(&queue));

            session.beginConfiguration();
            if !session.canAddInput(&input) {
                session.commitConfiguration();
                return Err(Error::Failed(format!("camera {id} can't be added to a session")));
            }
            session.addInput(&input);
            if !session.canAddOutput(&video) {
                session.commitConfiguration();
                return Err(Error::Failed("the video output can't be added".into()));
            }
            session.addOutput(&video);
        }

        // the format and frame rate are set with the device locked, and it's kept locked until
        // the session runs, so starting doesn't put the session's own choice back
        let locked = unsafe { device.lockForConfiguration() }.is_ok();
        unsafe {
            if locked {
                device.setActiveFormat(&av_format);
                if let Some(duration) = frame_duration(&av_format, fps) {
                    device.setActiveVideoMinFrameDuration(duration);
                    device.setActiveVideoMaxFrameDuration(duration);
                }
            } else {
                tracing::warn!(camera = id, "couldn't lock the camera: its own format is used");
            }
            session.commitConfiguration();
        }

        let mut observers = Vec::new();
        {
            let out = output.clone();
            observers.push(Observer::add(
                unsafe { AVCaptureSessionRuntimeErrorNotification },
                Some(&session),
                move |n| {
                    let reason = n
                        .userInfo()
                        .and_then(|info| info.objectForKey(unsafe { AVCaptureSessionErrorKey }))
                        .and_then(|e| e.downcast::<NSError>().ok())
                        .map(|e| error_text(&e))
                        .unwrap_or_else(|| "the camera stopped".into());
                    out.stopped(&reason);
                },
            ));
            let out = output.clone();
            observers.push(Observer::add(
                unsafe { AVCaptureDeviceWasDisconnectedNotification },
                Some(&device),
                move |_| out.stopped("the camera was disconnected"),
            ));
        }

        unsafe {
            if let Some(clock) = session.synchronizationClock() {
                let _ = output.ivars().clock.set(Clock(clock));
            }
            session.startRunning();
            if locked {
                device.unlockForConfiguration();
            }
            if !session.isRunning() {
                output.ivars().alive.store(false, Ordering::Release);
                return Err(Error::Failed(format!("camera {id} didn't start")));
            }
        }
        Ok(Session { session, output, _video: video, _queue: queue, _observers: observers, format, fps })
    }

    pub fn format(&self) -> &Format {
        &self.format
    }

    pub fn fps(&self) -> f64 {
        self.fps
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.output.ivars().alive.store(false, Ordering::Release);
        // (blocks until the camera has stopped, so it can be opened again straight away)
        unsafe { self.session.stopRunning() };
    }
}

// ------------------------------------------------------------------ devices coming and going

pub struct Watch {
    _observers: [Observer; 2],
}

// The observers are only removed (from any thread) when the watch is dropped.
unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

fn event_device(n: &NSNotification) -> Option<(String, String)> {
    let device = n.object()?.downcast::<AVCaptureDevice>().ok()?;
    let has_video = unsafe { device.hasMediaType(video()) }
        || unsafe { AVMediaTypeMuxed }.is_some_and(|m| unsafe { device.hasMediaType(m) });
    has_video.then(|| unsafe { (device.uniqueID().to_string(), device.localizedName().to_string()) })
}

impl Watch {
    pub fn new(f: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Watch {
        let f = std::sync::Arc::new(f);
        let on_connect = f.clone();
        let connected = Observer::add(unsafe { AVCaptureDeviceWasConnectedNotification }, None, move |n| {
            if let Some((id, name)) = event_device(n) {
                on_connect(DeviceEvent::Connected { id, name });
            }
        });
        let disconnected = Observer::add(unsafe { AVCaptureDeviceWasDisconnectedNotification }, None, move |n| {
            if let Some((id, name)) = event_device(n) {
                f(DeviceEvent::Disconnected { id, name });
            }
        });
        Watch { _observers: [connected, disconnected] }
    }
}
