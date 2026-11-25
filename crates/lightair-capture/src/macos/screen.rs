//! ScreenCaptureKit: displays, windows and apps, and streams of them.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFRetained, CFUUID, CGRect};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoContentRect, SCStreamFrameInfoContentScale,
    SCStreamFrameInfoScaleFactor, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use parking_lot::Mutex;

use super::{Clock, answer, error_text, frame_of};
use crate::screen::{App, Config, Display, Shareable, Target, Window};
use crate::{Error, Rect, VideoSink};

// ------------------------------------------------------------------ Core Graphics

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut c_void);
    fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut CGRect) -> bool;
}

#[link(name = "ColorSync", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *mut CFUUID;
}

/// ScreenCaptureKit's "the user declined" (`SCStreamErrorUserDeclined`).
const USER_DECLINED: isize = -3801;

pub fn access() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// A display's UUID as text (what settings store).
fn display_uuid(id: u32) -> String {
    let raw = unsafe { CGDisplayCreateUUIDFromDisplayID(id) };
    let Some(uuid) = NonNull::new(raw).map(|p| unsafe { CFRetained::from_raw(p) }) else {
        return String::new();
    };
    CFUUID::new_string(None, Some(&uuid)).map(|s| s.to_string()).unwrap_or_default()
}

/// The displays' names by id, from AppKit's screens (main thread only; empty elsewhere or
/// without AppKit loaded).
fn display_names() -> Vec<(u32, String)> {
    if !super::on_main_thread() {
        return Vec::new();
    }
    let Some(class) = AnyClass::get(c"NSScreen") else {
        return Vec::new();
    };
    objc2::rc::autoreleasepool(|_| unsafe {
        let screens: Option<Retained<NSArray<AnyObject>>> = msg_send![class, screens];
        let Some(screens) = screens else {
            return Vec::new();
        };
        let key = super::ns("NSScreenNumber");
        screens
            .iter()
            .filter_map(|screen| {
                let description: Retained<NSDictionary<AnyObject, AnyObject>> = msg_send![&*screen, deviceDescription];
                let number = description.objectForKey(&key)?;
                let number = number.downcast::<NSNumber>().ok()?;
                let name: Option<Retained<objc2_foundation::NSString>> = msg_send![&*screen, localizedName];
                Some((number.unsignedIntValue(), name?.to_string()))
            })
            .collect()
    })
}

pub fn displays() -> Vec<Display> {
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    let main = unsafe { CGMainDisplayID() };
    let names = display_names();
    ids[..count as usize]
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let bounds = unsafe { CGDisplayBounds(id) };
            let pixel_size = unsafe {
                let mode = CGDisplayCopyDisplayMode(id);
                if mode.is_null() {
                    (bounds.size.width as u32, bounds.size.height as u32)
                } else {
                    let size = (CGDisplayModeGetPixelWidth(mode) as u32, CGDisplayModeGetPixelHeight(mode) as u32);
                    CGDisplayModeRelease(mode);
                    size
                }
            };
            let built_in = unsafe { CGDisplayIsBuiltin(id) } != 0;
            let name = names
                .iter()
                .find(|(n, _)| *n == id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| if built_in { "Built-in Display".into() } else { format!("Display {}", i + 1) });
            Display {
                id,
                uuid: display_uuid(id),
                name,
                size: (bounds.size.width as u32, bounds.size.height as u32),
                pixel_size,
                origin: (bounds.origin.x, bounds.origin.y),
                main: id == main,
                built_in,
            }
        })
        .collect()
}

// ------------------------------------------------------------------ shareable content

/// What ScreenCaptureKit can capture now (`on_screen`: only windows on screen).
fn content(on_screen: bool) -> Result<Retained<SCShareableContent>, Error> {
    let got = answer("the shareable content", |tx| {
        let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
            let got = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(content),
                None => Err(unsafe { error.as_ref() }
                    .map(|e| (e.code(), error_text(e)))
                    .unwrap_or((0, "no content".into()))),
            };
            let _ = tx.send(got);
        });
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, on_screen, &block,
            )
        };
    })?;
    got.map_err(|(code, text)| {
        if code == USER_DECLINED || !access() {
            Error::Permission(format!("screen recording: {text}"))
        } else {
            Error::Failed(text)
        }
    })
}

fn own_pid() -> i32 {
    std::process::id() as i32
}

fn app_of(app: &SCRunningApplication) -> App {
    unsafe {
        App {
            bundle: app.bundleIdentifier().to_string(),
            name: app.applicationName().to_string(),
            pid: app.processID(),
        }
    }
}

fn window_of(window: &SCWindow) -> Window {
    unsafe {
        let owner = window.owningApplication();
        let frame = window.frame();
        Window {
            id: window.windowID(),
            title: window.title().map(|t| t.to_string()).unwrap_or_default(),
            app: owner.as_ref().map(|a| a.bundleIdentifier().to_string()).filter(|b| !b.is_empty()),
            app_name: owner.as_ref().map(|a| a.applicationName().to_string()),
            pid: owner.as_ref().map(|a| a.processID()),
            size: (frame.size.width as u32, frame.size.height as u32),
            on_screen: window.isOnScreen(),
            layer: window.windowLayer() as i64,
        }
    }
}

pub fn shareable() -> Result<Shareable, Error> {
    let content = content(true)?;
    let own = own_pid();
    let windows = unsafe { content.windows() }.iter().map(|w| window_of(&w)).filter(|w| w.pid != Some(own)).collect();
    let mut apps: Vec<App> = unsafe { content.applications() }
        .iter()
        .map(|a| app_of(&a))
        .filter(|a| a.pid != own && !a.bundle.is_empty())
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(Shareable { displays: displays(), windows, apps })
}

// ------------------------------------------------------------------ the stream's output

pub struct Ivars {
    sink: Mutex<Box<dyn VideoSink>>,
    /// The stream's clock, once it's made (its frames' times are on it).
    clock: OnceLock<Clock>,
    /// False once the handle is dropped: frames still in flight are let go.
    alive: AtomicBool,
    /// The last frame's size, packed (width << 32 | height).
    size: AtomicU64,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Output doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                self.picture(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.stopped(&error_text(error));
        }
    }
);

impl Output {
    fn new(sink: Box<dyn VideoSink>) -> Retained<Output> {
        let this = Output::alloc().set_ivars(Ivars {
            sink: Mutex::new(sink),
            clock: OnceLock::new(),
            alive: AtomicBool::new(true),
            size: AtomicU64::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn stopped(&self, reason: &str) {
        let ivars = self.ivars();
        if ivars.alive.swap(false, Ordering::AcqRel) {
            ivars.sink.lock().stopped(reason);
        }
    }

    fn picture(&self, sample: &CMSampleBuffer) {
        let ivars = self.ivars();
        if !ivars.alive.load(Ordering::Acquire) {
            return;
        }
        let info = FrameInfo::of(sample);
        match info.status {
            Some(SCFrameStatus::Complete) | None => {}
            Some(SCFrameStatus::Stopped) => return self.stopped("the capture ended"),
            // nothing changed, the display is asleep, or the stream is starting: no picture
            Some(_) => return,
        }
        let Some(mut frame) = frame_of(sample, ivars.clock.get()) else {
            return;
        };
        frame.content = info.content;
        frame.content_scale = info.content_scale;
        frame.scale = info.scale;
        ivars.size.store((frame.size.0 as u64) << 32 | frame.size.1 as u64, Ordering::Relaxed);
        ivars.sink.lock().frame(frame);
    }
}

/// What ScreenCaptureKit says about a frame (its attachments).
struct FrameInfo {
    status: Option<SCFrameStatus>,
    content: Option<Rect>,
    content_scale: Option<f64>,
    scale: Option<f64>,
}

impl FrameInfo {
    fn of(sample: &CMSampleBuffer) -> FrameInfo {
        let mut info = FrameInfo { status: None, content: None, content_scale: None, scale: None };
        let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
            return info;
        };
        // (a CFArray of CFDictionaries is an NSArray of NSDictionaries, toll-free)
        let attachments: &NSArray<NSDictionary<AnyObject, AnyObject>> =
            unsafe { &*(CFRetained::as_ptr(&attachments).as_ptr() as *const _) };
        let Some(first) = attachments.firstObject() else {
            return info;
        };
        let number =
            |key: &objc2_foundation::NSString| first.objectForKey(key).and_then(|v| v.downcast::<NSNumber>().ok());
        unsafe {
            info.status = number(SCStreamFrameInfoStatus).map(|n| SCFrameStatus(n.integerValue()));
            info.scale = number(SCStreamFrameInfoScaleFactor).map(|n| n.doubleValue());
            info.content_scale = number(SCStreamFrameInfoContentScale).map(|n| n.doubleValue());
            if let Some(rect) = first.objectForKey(SCStreamFrameInfoContentRect) {
                let mut r = CGRect::default();
                let dict = Retained::as_ptr(&rect) as *const c_void;
                if CGRectMakeWithDictionaryRepresentation(dict, &mut r) {
                    info.content =
                        Some(Rect { x: r.origin.x, y: r.origin.y, width: r.size.width, height: r.size.height });
                }
            }
        }
        info
    }
}

// ------------------------------------------------------------------ streams

/// The display a target names: by UUID, or by id as decimal text.
fn find_display(content: &SCShareableContent, wanted: &str) -> Option<Retained<SCDisplay>> {
    let by_id = wanted.parse::<u32>().ok();
    unsafe { content.displays() }.iter().find(|d| {
        let id = unsafe { d.displayID() };
        by_id == Some(id) || display_uuid(id).eq_ignore_ascii_case(wanted)
    })
}

fn main_display(content: &SCShareableContent) -> Option<Retained<SCDisplay>> {
    let main = unsafe { CGMainDisplayID() };
    let displays = unsafe { content.displays() };
    displays.iter().find(|d| unsafe { d.displayID() } == main).or_else(|| displays.firstObject())
}

/// How much of `a` lies within `b`.
fn overlap(a: CGRect, b: CGRect) -> f64 {
    let w = (a.origin.x + a.size.width).min(b.origin.x + b.size.width) - a.origin.x.max(b.origin.x);
    let h = (a.origin.y + a.size.height).min(b.origin.y + b.size.height) - a.origin.y.max(b.origin.y);
    w.max(0.0) * h.max(0.0)
}

/// The content filter for a target.
fn filter(target: &Target, exclude_own: bool) -> Result<Retained<SCContentFilter>, Error> {
    match target {
        Target::Display(wanted) => {
            let content = content(true)?;
            let display = find_display(&content, wanted).ok_or_else(|| Error::NotFound(format!("display {wanted}")))?;
            let own: Vec<Retained<SCRunningApplication>> = if exclude_own {
                unsafe { content.applications() }.iter().filter(|a| unsafe { a.processID() } == own_pid()).collect()
            } else {
                Vec::new()
            };
            let own = NSArray::from_retained_slice(&own);
            let none = NSArray::<SCWindow>::new();
            Ok(unsafe {
                SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &own,
                    &none,
                )
            })
        }
        Target::Window(id) => {
            // (every window, not only those on screen: one on another Space is still there)
            let content = content(false)?;
            let window = unsafe { content.windows() }
                .iter()
                .find(|w| unsafe { w.windowID() } == *id)
                .ok_or_else(|| Error::NotFound(format!("window {id}")))?;
            Ok(unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window) })
        }
        Target::App { bundle, display } => {
            let content = content(true)?;
            let apps: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
                .iter()
                .filter(|a| unsafe { a.bundleIdentifier() }.to_string() == *bundle)
                .collect();
            if apps.is_empty() {
                return Err(Error::NotFound(format!("app {bundle}")));
            }
            let display = match display {
                Some(wanted) => {
                    find_display(&content, wanted).ok_or_else(|| Error::NotFound(format!("display {wanted}")))?
                }
                // the display that shows most of the app's windows
                None => {
                    let pids: Vec<i32> = apps.iter().map(|a| unsafe { a.processID() }).collect();
                    let frames: Vec<CGRect> = unsafe { content.windows() }
                        .iter()
                        .filter(|w| unsafe {
                            w.windowLayer() == 0 && w.owningApplication().is_some_and(|a| pids.contains(&a.processID()))
                        })
                        .map(|w| unsafe { w.frame() })
                        .collect();
                    let shown = |d: &SCDisplay| {
                        let bounds = unsafe { d.frame() };
                        frames.iter().map(|f| overlap(*f, bounds)).sum::<f64>()
                    };
                    unsafe { content.displays() }
                        .iter()
                        .max_by(|a, b| shown(a).total_cmp(&shown(b)))
                        .filter(|d| shown(d) > 0.0)
                        .or_else(|| main_display(&content))
                        .ok_or_else(|| Error::NotFound("a display".into()))?
                }
            };
            let apps = NSArray::from_retained_slice(&apps);
            let none = NSArray::<SCWindow>::new();
            Ok(unsafe {
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &apps,
                    &none,
                )
            })
        }
    }
}

/// The filter's content at full resolution, in pixels.
fn native_size(filter: &SCContentFilter) -> (u32, u32) {
    let (rect, scale) = unsafe { (filter.contentRect(), filter.pointPixelScale() as f64) };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    (((rect.size.width * scale).round() as u32).max(2), ((rect.size.height * scale).round() as u32).max(2))
}

fn configuration(config: &Config, size: (u32, u32)) -> Retained<SCStreamConfiguration> {
    unsafe {
        let c = SCStreamConfiguration::new();
        // even sizes: 4:2:0 formats need them, and encoders prefer them
        c.setWidth((size.0.max(2) & !1) as usize);
        c.setHeight((size.1.max(2) & !1) as usize);
        c.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: config.fps.clamp(1, 240) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        c.setPixelFormat(config.format.fourcc());
        if config.format == crate::PixelFormat::Bgra {
            c.setColorSpaceName(kCGColorSpaceSRGB);
        }
        c.setShowsCursor(config.cursor);
        c.setQueueDepth(config.queue_depth.clamp(3, 8) as isize);
        c.setScalesToFit(true);
        c.setPreservesAspectRatio(true);
        c.setCapturesAudio(false);
        c
    }
}

pub struct Stream {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    /// The size asked for (when the config gives none, the target's own when started).
    size: (u32, u32),
}

// ScreenCaptureKit's streams may be configured and stopped from any thread; the output's state
// is behind locks and atomics.
unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

/// Waits for a stream call's completion handler: `Ok` or the error's text.
fn completion(what: &str, call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), Error> {
    let got = answer(what, |tx| {
        let block = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send(unsafe { error.as_ref() }.map(|e| (e.code(), error_text(e))));
        });
        call(&block);
    })?;
    match got {
        None => Ok(()),
        Some((code, text)) if code == USER_DECLINED => Err(Error::Permission(format!("screen recording: {text}"))),
        Some((_, text)) => Err(Error::Failed(format!("{what}: {text}"))),
    }
}

impl Stream {
    pub fn start(target: &Target, config: &Config, sink: Box<dyn VideoSink>) -> Result<Stream, Error> {
        let filter = filter(target, config.exclude_own)?;
        let size = config.size.unwrap_or_else(|| native_size(&filter));
        let configuration = configuration(config, size);
        let output = Output::new(sink);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        if let Some(clock) = unsafe { stream.synchronizationClock() } {
            let _ = output.ivars().clock.set(Clock(clock));
        }
        let queue = DispatchQueue::new("app.lightair.capture.screen", None);
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| Error::Failed(format!("the stream's output: {}", error_text(&e))))?;
        completion("starting the capture", |done| unsafe { stream.startCaptureWithCompletionHandler(Some(done)) })?;
        Ok(Stream { stream, output, _queue: queue, size })
    }

    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        let configuration = configuration(config, config.size.unwrap_or(self.size));
        completion("changing the capture", |done| unsafe {
            self.stream.updateConfiguration_completionHandler(&configuration, Some(done))
        })
    }

    pub fn size(&self) -> (u32, u32) {
        match self.output.ivars().size.load(Ordering::Relaxed) {
            0 => self.size,
            packed => ((packed >> 32) as u32, packed as u32),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.output.ivars().alive.store(false, Ordering::Release);
        // stopping finishes on ScreenCaptureKit's own time; the stream and its output are kept
        // until it has
        let keep = (self.stream.clone(), self.output.clone());
        let done = RcBlock::new(move |_: *mut NSError| {
            let _ = &keep;
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
    }
}
