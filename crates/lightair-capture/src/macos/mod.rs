//! macOS: ScreenCaptureKit, AVFoundation and Core Audio, and what they share.

pub mod camera;
pub mod screen;
pub mod sound;

use std::sync::mpsc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_core_foundation::CFType;
use objc2_core_media::{CMClock, CMSampleBuffer, CMSyncConvertTime, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use objc2_foundation::{NSError, NSString};

use crate::{Error, Frame, PixelFormat};

/// How long a call into the system may take to answer before it's given up on.
const ANSWER: Duration = Duration::from_secs(8);

/// A Core Media clock, shared with the capture queue that converts times on it.
pub(crate) struct Clock(pub Retained<CMClock>);

// Core Media's clocks are immutable references read from any thread.
unsafe impl Send for Clock {}
unsafe impl Sync for Clock {}

/// A time on `clock` (the host clock when `None`) as host-clock nanoseconds.
pub(crate) fn host_ns(time: CMTime, clock: Option<&Clock>) -> Option<i64> {
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 {
        return None;
    }
    let time = match clock {
        Some(clock) => {
            let host = unsafe { CMClock::host_time_clock() };
            let from: &CFType = &clock.0;
            let to: &CFType = &host;
            if from == to { time } else { unsafe { CMSyncConvertTime(time, from, to) } }
        }
        None => time,
    };
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 {
        return None;
    }
    Some((time.value as i128 * 1_000_000_000 / time.timescale as i128) as i64)
}

/// The sample buffer's picture as a frame: its pixel buffer if it's in an IOSurface, stamped
/// with its presentation time on the host clock (the time it arrived when it has none).
pub(crate) fn frame_of(sample: &CMSampleBuffer, clock: Option<&Clock>) -> Option<Frame> {
    let buffer = unsafe { sample.image_buffer() }?;
    CVPixelBufferGetIOSurface(Some(&buffer))?;
    let format = PixelFormat::from_fourcc(CVPixelBufferGetPixelFormatType(&buffer))?;
    let size = (CVPixelBufferGetWidth(&buffer) as u32, CVPixelBufferGetHeight(&buffer) as u32);
    let host_ns = host_ns(unsafe { sample.presentation_time_stamp() }, clock).unwrap_or_else(now_ns);
    Some(Frame { buffer, host_ns, size, format, content: None, content_scale: None, scale: None })
}

/// The host clock now, in nanoseconds (`mach_absolute_time`, as the engine's clock reads it).
pub(crate) fn now_ns() -> i64 {
    let (n, d) = timebase();
    (unsafe { mach_absolute_time() } as u128 * n as u128 / d as u128) as i64
}

/// `mach_absolute_time`'s units to nanoseconds: (numerator, denominator).
pub(crate) fn timebase() -> (u32, u32) {
    static BASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| {
        let mut info = [0u32; 2];
        unsafe { mach_timebase_info(&mut info) };
        (info[0].max(1), info[1].max(1))
    })
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
    fn pthread_main_np() -> i32;
}

/// Whether this is the process's main thread.
pub(crate) fn on_main_thread() -> bool {
    unsafe { pthread_main_np() != 0 }
}

/// An error's description, with its domain and code.
pub(crate) fn error_text(error: &NSError) -> String {
    format!("{} ({} {})", error.localizedDescription(), error.domain(), error.code())
}

/// Runs `call`, which hands its answer to the sender it's given (from a completion handler),
/// and waits for that answer.
pub(crate) fn answer<T>(what: &str, call: impl FnOnce(mpsc::Sender<T>)) -> Result<T, Error> {
    let (tx, rx) = mpsc::channel();
    call(tx);
    rx.recv_timeout(ANSWER).map_err(|_| Error::Failed(format!("{what}: no answer")))
}

pub(crate) fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}
