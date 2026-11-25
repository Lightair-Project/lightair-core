//! Core Audio process taps: the computer's sound (everything but Lightair) or one app's, read
//! through a private aggregate device whose I/O callback hands the samples over.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress, CATapDescription,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsNonInterleaved,
};
use objc2_core_foundation::{CFDictionary, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use parking_lot::{Condvar, Mutex};

use super::{now_ns, timebase};
use crate::sound::{SoundApp, Target};
use crate::{Error, Sound, SoundSink};

// ------------------------------------------------------------------ Core Audio properties

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM: AudioObjectID = 1;
const GLOBAL: u32 = fourcc(b"glob");
const TRANSLATE_PID: u32 = fourcc(b"id2p");
const PROCESS_LIST: u32 = fourcc(b"prs#");
const PROCESS_PID: u32 = fourcc(b"ppid");
const PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid");
const PROCESS_RUNNING_OUTPUT: u32 = fourcc(b"piro");
const TAP_UID: u32 = fourcc(b"tuid");
const TAP_FORMAT: u32 = fourcc(b"tfmt");
const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const DEVICE_UID: u32 = fourcc(b"uid ");

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: GLOBAL, mElement: 0 }
}

/// A property that's one plain value of type `T`.
///
/// # Safety
/// `T` must be the property's type.
unsafe fn get<T: Copy>(object: AudioObjectID, selector: u32, qualifier: Option<&u32>) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    let (qs, q) = qualifier.map_or((0, std::ptr::null()), |q| (4, q as *const u32 as *const c_void));
    let mut a = address(selector);
    let rc = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut a),
            qs,
            q,
            NonNull::from(&mut size),
            NonNull::new_unchecked(value.as_mut_ptr().cast()),
        )
    };
    (rc == 0 && size as usize == std::mem::size_of::<T>()).then(|| unsafe { value.assume_init() })
}

/// A property that's a CFString (one reference handed over), as text.
fn get_string(object: AudioObjectID, selector: u32) -> Option<String> {
    let raw: *const CFString = unsafe { get(object, selector, None)? };
    let s = NonNull::new(raw as *mut CFString)?;
    let s = unsafe { objc2_core_foundation::CFRetained::from_raw(s) };
    Some(s.to_string())
}

/// Every audio process object Core Audio knows.
fn processes() -> Vec<AudioObjectID> {
    let mut a = address(PROCESS_LIST);
    let mut size = 0u32;
    let rc = unsafe {
        AudioObjectGetPropertyDataSize(SYSTEM, NonNull::from(&mut a), 0, std::ptr::null(), NonNull::from(&mut size))
    };
    if rc != 0 || size == 0 {
        return Vec::new();
    }
    let mut list = vec![0 as AudioObjectID; size as usize / 4];
    let rc = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM,
            NonNull::from(&mut a),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(list.as_mut_ptr().cast()),
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    list.truncate(size as usize / 4);
    list
}

/// Whether a process's bundle id is `bundle`'s, or one of its helpers' (`bundle.helper…`).
fn belongs(id: &str, bundle: &str) -> bool {
    id == bundle || id.strip_prefix(bundle).is_some_and(|rest| rest.starts_with('.'))
}

/// The process objects of the app with this bundle id (its helpers included).
fn app_processes(bundle: &str) -> Vec<AudioObjectID> {
    processes().into_iter().filter(|&p| get_string(p, PROCESS_BUNDLE_ID).is_some_and(|b| belongs(&b, bundle))).collect()
}

// ------------------------------------------------------------------ what there is

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}

/// `RTLD_DEFAULT`: every image loaded.
const DEFAULT: *mut c_void = -2isize as *mut c_void;

type CreateTap = unsafe extern "C" fn(description: *mut AnyObject, tap: *mut AudioObjectID) -> i32;
type DestroyTap = unsafe extern "C" fn(tap: AudioObjectID) -> i32;

/// The tap functions, looked up as the process runs (they're macOS 14.2's: linking them would
/// keep Lightair from opening on 14.0 and 14.1).
fn tap_functions() -> Option<(CreateTap, DestroyTap)> {
    fn find(name: &CStr) -> Option<*mut c_void> {
        let f = unsafe { dlsym(DEFAULT, name.as_ptr()) };
        (!f.is_null()).then_some(f)
    }
    let create = find(c"AudioHardwareCreateProcessTap")?;
    let destroy = find(c"AudioHardwareDestroyProcessTap")?;
    Some(unsafe {
        (std::mem::transmute::<*mut c_void, CreateTap>(create), std::mem::transmute::<*mut c_void, DestroyTap>(destroy))
    })
}

pub fn supported() -> bool {
    AnyClass::get(c"CATapDescription").is_some() && tap_functions().is_some()
}

pub fn apps() -> Vec<SoundApp> {
    let own = std::process::id() as i32;
    let mut apps: Vec<SoundApp> = Vec::new();
    for p in processes() {
        let Some(bundle) = get_string(p, PROCESS_BUNDLE_ID).filter(|b| !b.is_empty()) else {
            continue;
        };
        let pid: i32 = unsafe { get(p, PROCESS_PID, None) }.unwrap_or(0);
        if pid == own {
            continue;
        }
        let playing = unsafe { get::<u32>(p, PROCESS_RUNNING_OUTPUT, None) }.unwrap_or(0) != 0;
        // a helper is folded into the app whose bundle id begins its own
        if let Some(app) = apps.iter_mut().find(|a| belongs(&bundle, &a.bundle) || belongs(&a.bundle, &bundle)) {
            if belongs(&app.bundle, &bundle) && app.bundle != bundle {
                app.bundle = bundle;
                app.pid = pid;
            }
            app.playing |= playing;
            continue;
        }
        apps.push(SoundApp { bundle, pid, playing });
    }
    apps.sort_by(|a, b| a.bundle.cmp(&b.bundle));
    apps
}

// ------------------------------------------------------------------ taps

/// What the capture's taps share: the sink, and the format of the tap running now.
struct Shared {
    sink: Mutex<Box<dyn SoundSink>>,
    /// (rate << 32 | channels) of the tap running now; 0 for none.
    format: AtomicU64,
}

/// What a tap's I/O callback reads.
struct Reader {
    shared: Arc<Shared>,
    format: AudioStreamBasicDescription,
    /// Planar sound interleaved, kept between callbacks (no allocation once warm).
    scratch: Mutex<Vec<f32>>,
}

/// A running tap: the tap, its aggregate device and I/O callback.
struct Tap {
    tap: AudioObjectID,
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    destroy: DestroyTap,
    /// Kept alive while the device runs.
    _reader: Box<Reader>,
}

// Core Audio's objects are ids, usable from any thread.
unsafe impl Send for Tap {}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.device, self.proc_id);
            AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            AudioHardwareDestroyAggregateDevice(self.device);
            (self.destroy)(self.tap);
        }
        self._reader.shared.format.store(0, Ordering::Relaxed);
    }
}

/// An I/O callback as Core Audio calls it: any of its pointers may be null.
type RawIoProc = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
) -> i32;

/// An I/O callback as the bindings declare it.
type TypedIoProc = unsafe extern "C-unwind" fn(
    AudioObjectID,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    *mut c_void,
) -> i32;

/// The tap's samples to the sink (Core Audio's real-time I/O thread). A panic in the sink
/// aborts rather than unwinding into Core Audio.
unsafe extern "C" fn read(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> i32 {
    let Some(r) = (unsafe { (client as *const Reader).as_ref() }) else {
        return 0;
    };
    let Some(list) = (unsafe { input.as_ref() }) else {
        return 0;
    };
    if list.mNumberBuffers == 0 {
        return 0;
    }
    let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };
    let channels = r.format.mChannelsPerFrame.max(1);
    let planar = r.format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let first = &buffers[0];
    if first.mData.is_null() {
        return 0;
    }
    let frames = if planar { first.mDataByteSize / 4 } else { first.mDataByteSize / 4 / channels } as usize;
    if frames == 0 {
        return 0;
    }
    let host = unsafe { input_time.as_ref() }.map_or(0, |t| t.mHostTime);
    let host_ns = if host > 0 {
        let (n, d) = timebase();
        (host as u128 * n as u128 / d as u128) as i64
    } else {
        now_ns()
    };
    // (a sink being replaced or a second callback at the same moment loses this block)
    let Some(mut sink) = r.shared.sink.try_lock() else {
        return 0;
    };
    let rate = r.format.mSampleRate as u32;
    if planar && buffers.len() > 1 {
        let Some(mut scratch) = r.scratch.try_lock() else {
            return 0;
        };
        let n = buffers.len().min(channels as usize);
        scratch.clear();
        scratch.resize(frames * n, 0.0);
        for (c, b) in buffers[..n].iter().enumerate() {
            if b.mData.is_null() {
                continue;
            }
            let plane = unsafe { std::slice::from_raw_parts(b.mData as *const f32, (b.mDataByteSize / 4) as usize) };
            for (i, s) in plane.iter().take(frames).enumerate() {
                scratch[i * n + c] = *s;
            }
        }
        sink.sound(&Sound { samples: &scratch, channels: n as u32, rate, host_ns });
    } else {
        let channels = if planar { 1 } else { channels };
        let samples = unsafe { std::slice::from_raw_parts(first.mData as *const f32, frames * channels as usize) };
        sink.sound(&Sound { samples, channels, rate, host_ns });
    }
    0
}

fn number(v: i64) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(NSNumber::new_i64(v))).into()
}

fn text(s: &str) -> Retained<AnyObject> {
    Retained::into_super(NSString::from_str(s)).into()
}

fn dictionary(pairs: &[(&str, Retained<AnyObject>)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|(_, v)| &**v).collect();
    NSDictionary::from_slices(&keys, &values)
}

fn array(items: Vec<Retained<AnyObject>>) -> Retained<AnyObject> {
    Retained::into_super(NSArray::from_retained_slice(&items)).into()
}

/// Makes a tap of everything but Lightair (`processes` none, unless `include_own`) or of these
/// processes, and starts reading it.
fn start_tap(shared: &Arc<Shared>, processes: Option<&[AudioObjectID]>, include_own: bool) -> Result<Tap, Error> {
    let (create, destroy) = tap_functions().ok_or_else(|| Error::Unsupported("process taps need macOS 14.2".into()))?;
    if AnyClass::get(c"CATapDescription").is_none() {
        return Err(Error::Unsupported("process taps need macOS 14.2".into()));
    }
    objc2::rc::autoreleasepool(|_| unsafe {
        let description = match processes {
            Some(list) => {
                let ids: Vec<Retained<NSNumber>> = list.iter().map(|&p| NSNumber::new_u32(p)).collect();
                CATapDescription::initStereoMixdownOfProcesses(
                    CATapDescription::alloc(),
                    &NSArray::from_retained_slice(&ids),
                )
            }
            None => {
                let own = if include_own {
                    None
                } else {
                    get::<AudioObjectID>(SYSTEM, TRANSLATE_PID, Some(&std::process::id())).filter(|&o| o != 0)
                };
                let ids: Vec<Retained<NSNumber>> = own.map(NSNumber::new_u32).into_iter().collect();
                CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &NSArray::from_retained_slice(&ids),
                )
            }
        };
        description.setPrivate(true);
        let mut tap: AudioObjectID = 0;
        let rc = create(Retained::as_ptr(&description) as *mut AnyObject, &mut tap);
        if rc != 0 {
            return Err(Error::Failed(format!("making the tap: {rc}")));
        }
        let fail = |what: &str, rc: i32| {
            destroy(tap);
            Err(Error::Failed(format!("{what}: {rc}")))
        };
        let Some(tap_uid) = get_string(tap, TAP_UID) else {
            return fail("the tap's id", -1);
        };
        let Some(format) = get::<AudioStreamBasicDescription>(tap, TAP_FORMAT, None) else {
            return fail("the tap's format", -1);
        };
        let output = get::<AudioObjectID>(SYSTEM, DEFAULT_OUTPUT, None).and_then(|d| get_string(d, DEVICE_UID));
        let uid = format!("app.lightair.capture.tap.{}.{}", std::process::id(), tap);
        let mut pairs = vec![
            ("name", text("Lightair sound capture")),
            ("uid", text(&uid)),
            ("private", number(1)),
            ("stacked", number(0)),
            ("tapautostart", number(1)),
            (
                "taps",
                array(vec![Retained::into_super(dictionary(&[("uid", text(&tap_uid)), ("drift", number(1))])).into()]),
            ),
        ];
        // the default output's clock drives the aggregate device
        if let Some(out) = &output {
            pairs.push(("master", text(out)));
            pairs.push(("subdevices", array(vec![Retained::into_super(dictionary(&[("uid", text(out))])).into()])));
        }
        let description = dictionary(&pairs);
        // (an NSDictionary is a CFDictionary, toll-free)
        let cf: &CFDictionary = &*(Retained::as_ptr(&description) as *const CFDictionary);
        let mut device: AudioObjectID = 0;
        let rc = AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut device));
        if rc != 0 {
            return fail("making the aggregate device", rc);
        }
        let reader =
            Box::new(Reader { shared: shared.clone(), format, scratch: Mutex::new(Vec::with_capacity(16_384)) });
        let mut proc_id: AudioDeviceIOProcID = None;
        let rc = AudioDeviceCreateIOProcID(
            device,
            // (the same function; Core Audio's pointers seen as possibly null)
            Some(std::mem::transmute::<RawIoProc, TypedIoProc>(read as RawIoProc)),
            &*reader as *const Reader as *mut c_void,
            NonNull::from(&mut proc_id),
        );
        if rc != 0 {
            AudioHardwareDestroyAggregateDevice(device);
            return fail("the I/O callback", rc);
        }
        let running = Tap { tap, device, proc_id, destroy, _reader: reader };
        let rc = AudioDeviceStart(device, proc_id);
        if rc != 0 {
            drop(running);
            return Err(Error::Failed(format!("starting the tap: {rc}")));
        }
        shared.format.store((format.mSampleRate as u64) << 32 | format.mChannelsPerFrame as u64, Ordering::Relaxed);
        Ok(running)
    })
}

// ------------------------------------------------------------------ captures

/// What the thread that follows an app shares with the handle.
struct Follow {
    quit: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

pub struct Capture {
    shared: Arc<Shared>,
    /// The computer's sound: one tap for as long as the capture runs.
    tap: Option<Tap>,
    /// One app's sound: a thread making the tap again as the app's processes change.
    follow: Option<(Arc<Follow>, std::thread::JoinHandle<()>)>,
}

impl Capture {
    pub fn start(target: &Target, sink: Box<dyn SoundSink>) -> Result<Capture, Error> {
        if !supported() {
            return Err(Error::Unsupported("process taps need macOS 14.2".into()));
        }
        let shared = Arc::new(Shared { sink: Mutex::new(sink), format: AtomicU64::new(0) });
        match target {
            Target::System { include_own } => {
                let tap = start_tap(&shared, None, *include_own)?;
                Ok(Capture { shared, tap: Some(tap), follow: None })
            }
            Target::App(bundle) => {
                let follow =
                    Arc::new(Follow { quit: AtomicBool::new(false), lock: Mutex::new(()), wake: Condvar::new() });
                let thread = {
                    let (shared, follow, bundle) = (shared.clone(), follow.clone(), bundle.clone());
                    std::thread::Builder::new()
                        .name("lightair-capture-app-sound".into())
                        .spawn(move || follow_app(&shared, &follow, &bundle))
                        .map_err(|e| Error::Failed(format!("the app sound thread: {e}")))?
                };
                Ok(Capture { shared, tap: None, follow: Some((follow, thread)) })
            }
        }
    }

    pub fn format(&self) -> Option<(u32, u32)> {
        match self.shared.format.load(Ordering::Relaxed) {
            0 => None,
            packed => Some(((packed >> 32) as u32, packed as u32)),
        }
    }
}

/// Keeps a tap of `bundle`'s processes running: makes it, and makes it again when they change
/// (the app quit, opened again, started a helper). Looks every two seconds.
fn follow_app(shared: &Arc<Shared>, follow: &Follow, bundle: &str) {
    let mut tap: Option<Tap> = None;
    let mut tapped: Vec<AudioObjectID> = Vec::new();
    let mut failed_for: Option<Vec<AudioObjectID>> = None;
    while !follow.quit.load(Ordering::Acquire) {
        let mut now = app_processes(bundle);
        now.sort_unstable();
        if now != tapped && failed_for.as_ref() != Some(&now) {
            tap = None;
            tapped.clear();
            if !now.is_empty() {
                match start_tap(shared, Some(&now), false) {
                    Ok(t) => {
                        tap = Some(t);
                        tapped = now;
                        failed_for = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, app = bundle, "app sound: no tap");
                        if matches!(e, Error::Permission(_)) {
                            shared.sink.lock().stopped(&e.to_string());
                            return;
                        }
                        failed_for = Some(now);
                    }
                }
            }
        }
        let mut guard = follow.lock.lock();
        if !follow.quit.load(Ordering::Acquire) {
            follow.wake.wait_for(&mut guard, Duration::from_secs(2));
        }
    }
    drop(tap);
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.tap.take());
        if let Some((follow, thread)) = self.follow.take() {
            follow.quit.store(true, Ordering::Release);
            {
                let _guard = follow.lock.lock();
                follow.wake.notify_all();
            }
            let _ = thread.join();
        }
    }
}
