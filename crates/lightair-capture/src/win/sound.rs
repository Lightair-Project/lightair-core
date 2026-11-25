//! WASAPI loopback: the sound the default output plays, or one process tree's (process loopback,
//! Windows 10 2004 and later), read on a thread of the capture's own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, Ref, implement, w};

use super::{Owned, com, contract, exe_of, fail, hns_ns, host_now_ns, roots_of, same_exe};
use crate::sound::{SoundApp, Target};
use crate::{Error, Sound, SoundSink};

/// Loopback of the default output works on every Windows Lightair runs on; one app's sound (and
/// leaving Lightair's own out of the computer's) needs Windows 10 2004.
pub fn supported() -> bool {
    true
}

fn enumerator() -> Result<IMMDeviceEnumerator, Error> {
    com();
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| fail("opening the audio devices", e))
}

pub fn apps() -> Vec<SoundApp> {
    let Ok(enumerator) = enumerator() else { return Vec::new() };
    let Ok(outputs) = (unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }) else {
        return Vec::new();
    };
    let own = std::process::id();
    let mut apps: Vec<SoundApp> = Vec::new();
    for i in 0..unsafe { outputs.GetCount() }.unwrap_or(0) {
        let Ok(device) = (unsafe { outputs.Item(i) }) else { continue };
        let Ok(manager) = (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) }) else { continue };
        let Ok(sessions) = (unsafe { manager.GetSessionEnumerator() }) else { continue };
        for s in 0..unsafe { sessions.GetCount() }.unwrap_or(0) {
            let Ok(session) = (unsafe { sessions.GetSession(s) }) else { continue };
            let Ok(session) = session.cast::<IAudioSessionControl2>() else { continue };
            let pid = unsafe { session.GetProcessId() }.unwrap_or(0);
            // 0 is the system's own sounds
            if pid == 0 || pid == own {
                continue;
            }
            let Some(exe) = exe_of(pid) else { continue };
            let playing = unsafe { session.GetState() }.is_ok_and(|s| s == AudioSessionStateActive);
            match apps.iter_mut().find(|a| same_exe(&a.bundle, &exe)) {
                Some(app) => app.playing |= playing,
                None => apps.push(SoundApp { bundle: exe, pid: pid as i32, playing }),
            }
        }
    }
    apps
}

// ------------------------------------------------------------------ opening a loopback

/// Tells a waiting thread that `ActivateAudioInterfaceAsync` has finished.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<Option<mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        if let Some(tx) = self.0.lock().take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

/// An audio client on the process loopback device: process `pid`'s tree, or everything but it.
fn process_client(pid: u32, include: bool) -> Result<IAudioClient, Error> {
    if !contract(10) {
        return Err(Error::Unsupported("an app's sound needs Windows 10 2004 or later".into()));
    }
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if include {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    let variant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &mut params as *mut _ as *mut u8,
                    },
                },
            }),
        },
    };
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(Mutex::new(Some(tx))).into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&variant), &handler)
    }
    .map_err(|e| fail("opening the process loopback", e))?;
    rx.recv_timeout(Duration::from_secs(8))
        .map_err(|_| Error::Failed("opening the process loopback: no answer".into()))?;
    let mut result = windows::core::HRESULT(0);
    let mut client = None;
    unsafe { operation.GetActivateResult(&mut result, &mut client) }
        .map_err(|e| fail("opening the process loopback", e))?;
    result.ok().map_err(|e| fail("opening the process loopback", e))?;
    client
        .ok_or_else(|| Error::Failed("opening the process loopback: nothing opened".into()))?
        .cast()
        .map_err(|e| fail("opening the process loopback", e))
}

/// Interleaved 32-bit float, `channels` at `rate`.
fn float_format(rate: u32, channels: u16) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * channels as u32 * 4,
        nBlockAlign: channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// How a client's samples are laid out.
#[derive(Debug, Clone, Copy)]
struct Layout {
    rate: u32,
    channels: u32,
    /// 32-bit float, or signed integers of this many bits a container (16, 24, 32).
    float: bool,
    bits: u16,
}

fn layout_of(format: &WAVEFORMATEX) -> Layout {
    let tag = format.wFormatTag as u32;
    let float = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = unsafe { &*(format as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE) };
        let sub = ext.SubFormat;
        sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };
    Layout { rate: format.nSamplesPerSec, channels: format.nChannels as u32, float, bits: format.wBitsPerSample }
}

/// Initialises `client` for loopback capture: interleaved float stereo at the device's rate
/// (Windows converts), or the device's own format where it won't.
fn initialise(client: &IAudioClient, process: bool) -> Result<Layout, Error> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let convert = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // 20 ms of buffer: the capture thread wakes on every packet
    let buffer = 200_000i64;
    if process {
        // the process loopback device has no format of its own: it converts to the one given
        let format = float_format(48_000, 2);
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags | convert, buffer, 0, &format, None) }
            .map_err(|e| fail("starting the process loopback", e))?;
        return Ok(layout_of(&format));
    }
    let mix = unsafe { client.GetMixFormat() }.map_err(|e| fail("reading the output's format", e))?;
    let own = unsafe { *mix };
    let stereo = float_format(own.nSamplesPerSec, 2);
    let done = match unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags | convert, buffer, 0, &stereo, None) } {
        Ok(()) => Ok(layout_of(&stereo)),
        Err(_) => unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer, 0, mix, None) }
            .map(|()| layout_of(unsafe { &*mix }))
            .map_err(|e| fail("starting the loopback", e)),
    };
    unsafe { CoTaskMemFree(Some(mix as *const _)) };
    done
}

/// The default output device and its id.
fn default_output() -> Result<(IMMDevice, String), Error> {
    let device = unsafe { enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole) }
        .map_err(|e| fail("finding the default output", e))?;
    let id = unsafe { device.GetId() }
        .map(|p| {
            let id = unsafe { p.to_string() }.unwrap_or_default();
            unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
            id
        })
        .unwrap_or_default();
    Ok((device, id))
}

/// What a capture is of, once resolved.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    /// The default output (its id, to notice when the default changes).
    Output(String),
    /// Process loopback: this process's tree, or everything but it.
    Process { pid: u32, include: bool },
}

/// A started loopback: the client, its capture service, its event and layout.
struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Owned,
    layout: Layout,
    source: Source,
}

fn open(source: &Source) -> Result<Loopback, Error> {
    let (client, process) = match source {
        Source::Output(_) => {
            let (device, _) = default_output()?;
            let client = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
                .map_err(|e| fail("opening the output for loopback", e))?;
            (client, false)
        }
        Source::Process { pid, include } => (process_client(*pid, *include)?, true),
    };
    let layout = initialise(&client, process)?;
    let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| fail("making an event", e))?;
    let event = Owned(event);
    unsafe { client.SetEventHandle(event.0) }.map_err(|e| fail("starting the loopback", e))?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| fail("starting the loopback", e))?;
    unsafe { client.Start() }.map_err(|e| fail("starting the loopback", e))?;
    Ok(Loopback { client, capture, event, layout, source: source.clone() })
}

/// What the capture's thread should capture now: `None` while an app isn't running.
fn resolve(target: &Target) -> Result<Option<Source>, Error> {
    match target {
        Target::System { include_own } => {
            if !include_own && contract(10) {
                Ok(Some(Source::Process { pid: std::process::id(), include: false }))
            } else {
                Ok(Some(Source::Output(default_output()?.1)))
            }
        }
        Target::App(exe) => {
            if !contract(10) {
                return Err(Error::Unsupported("an app's sound needs Windows 10 2004 or later".into()));
            }
            Ok(roots_of(exe).first().map(|&pid| Source::Process { pid, include: true }))
        }
    }
}

// ------------------------------------------------------------------ the capture

/// A running loopback capture.
pub struct Capture {
    stop: Arc<AtomicBool>,
    format: Arc<Mutex<Option<(u32, u32)>>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    pub fn start(target: &Target, sink: Box<dyn SoundSink>) -> Result<Capture, Error> {
        com();
        let first = resolve(target)?;
        // the computer's sound starts here, so its failure is returned
        let first = match first {
            Some(source) => Some(open(&source)?),
            None => None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let format = Arc::new(Mutex::new(first.as_ref().map(|l| (l.layout.rate, l.layout.channels))));
        let target = target.clone();
        let (thread_stop, thread_format) = (stop.clone(), format.clone());
        let first = first.map(Sendable);
        let thread = std::thread::Builder::new()
            .name("lightair-loopback".into())
            .spawn(move || run(target, first.map(|s| s.0), sink, thread_stop, thread_format))
            .map_err(|e| Error::Failed(format!("starting the loopback thread: {e}")))?;
        Ok(Capture { stop, format, thread: Some(thread) })
    }

    pub fn format(&self) -> Option<(u32, u32)> {
        *self.format.lock()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A loopback handed to the thread that reads it (WASAPI's objects are free-threaded).
struct Sendable(Loopback);
unsafe impl Send for Sendable {}

/// How often the thread looks again at what it should capture (the app's processes, the
/// default output).
const LOOK_AGAIN: Duration = Duration::from_secs(2);

fn run(
    target: Target,
    mut current: Option<Loopback>,
    mut sink: Box<dyn SoundSink>,
    stop: Arc<AtomicBool>,
    format: Arc<Mutex<Option<(u32, u32)>>>,
) {
    com();
    let mut task = 0u32;
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }.ok();
    let mut samples: Vec<f32> = Vec::with_capacity(48_000 * 2);
    let mut looked = Instant::now();
    let mut failures = 0u32;
    while !stop.load(Ordering::Relaxed) {
        // follow the app (it quit, came back) or the default output (it changed)
        if looked.elapsed() >= LOOK_AGAIN || current.is_none() {
            looked = Instant::now();
            let wanted = resolve(&target).ok().flatten();
            if wanted.as_ref() != current.as_ref().map(|l| &l.source) {
                if let Some(old) = current.take() {
                    let _ = unsafe { old.client.Stop() };
                }
                current = match wanted.as_ref().map(open) {
                    Some(Ok(l)) => {
                        failures = 0;
                        Some(l)
                    }
                    Some(Err(e)) => {
                        failures += 1;
                        if failures == 5 {
                            tracing::warn!("capture: loopback: {e}");
                        }
                        None
                    }
                    None => None,
                };
                *format.lock() = current.as_ref().map(|l| (l.layout.rate, l.layout.channels));
            }
            if current.is_none() {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        }
        let Some(loopback) = current.as_ref() else { continue };
        // woken by each packet; every 100 ms at least, for the stop flag and outputs that don't
        // signal while silent
        unsafe { WaitForSingleObject(loopback.event.0, 100) };
        match drain(loopback, &mut samples, sink.as_mut()) {
            Ok(()) => {}
            Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                // the output went away or changed format: open what's wanted now
                current = None;
                *format.lock() = None;
            }
            Err(e) => {
                sink.stopped(&format!("the sound capture stopped: {}", e.message()));
                break;
            }
        }
    }
    if let Some(loopback) = current {
        let _ = unsafe { loopback.client.Stop() };
    }
    if let Some(handle) = mmcss {
        let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
    }
}

/// Hands every packet waiting in `loopback` to the sink.
fn drain(loopback: &Loopback, samples: &mut Vec<f32>, sink: &mut dyn SoundSink) -> windows::core::Result<()> {
    let layout = loopback.layout;
    let channels = layout.channels.max(1) as usize;
    loop {
        if unsafe { loopback.capture.GetNextPacketSize() }? == 0 {
            return Ok(());
        }
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        let mut qpc = 0u64;
        unsafe { loopback.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }?;
        let count = frames as usize * channels;
        samples.clear();
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
            samples.resize(count, 0.0);
        } else {
            convert(data, count, layout, samples);
        }
        unsafe { loopback.capture.ReleaseBuffer(frames) }?;
        if frames == 0 {
            continue;
        }
        let host_ns = if qpc != 0 { hns_ns(qpc as i64) } else { host_now_ns() };
        sink.sound(&Sound { samples, channels: layout.channels, rate: layout.rate, host_ns });
    }
}

/// `count` samples at `data` in `layout`'s format, as floats.
fn convert(data: *const u8, count: usize, layout: Layout, out: &mut Vec<f32>) {
    unsafe {
        match (layout.float, layout.bits) {
            (true, _) => out.extend_from_slice(std::slice::from_raw_parts(data as *const f32, count)),
            (false, 16) => {
                out.extend(std::slice::from_raw_parts(data as *const i16, count).iter().map(|&s| s as f32 / 32_768.0))
            }
            (false, 24) => out.extend(std::slice::from_raw_parts(data, count * 3).as_chunks::<3>().0.iter().map(|b| {
                let s = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                s as f32 / 8_388_608.0
            })),
            (false, 32) => out.extend(
                std::slice::from_raw_parts(data as *const i32, count).iter().map(|&s| s as f32 / 2_147_483_648.0),
            ),
            _ => out.resize(count, 0.0),
        }
    }
}
