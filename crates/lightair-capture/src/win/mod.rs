//! Windows: Windows Graphics Capture, Desktop Duplication, Media Foundation, WASAPI and event
//! tracing, and what they share (COM, the clock, processes, Direct3D 11).

pub mod camera;
mod duplication;
pub mod etw;
pub mod fso;
pub mod game;
pub mod gpu;
pub mod screen;
pub mod sound;
mod wgc;

use std::cell::Cell;

use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{HSTRING, PWSTR};

use crate::Error;

/// The host clock now, in nanoseconds: the performance counter (`QueryPerformanceCounter`),
/// which every Windows capture here stamps its frames and sound on. The engine's clock on
/// Windows must read the same counter.
pub fn host_now_ns() -> i64 {
    let mut now = 0i64;
    let _ = unsafe { QueryPerformanceCounter(&mut now) };
    qpc_ns(now)
}

/// The performance counter's ticks a second.
fn qpc_frequency() -> i64 {
    static FREQUENCY: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut f = 0i64;
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

/// Performance counter ticks as nanoseconds.
pub(crate) fn qpc_ns(ticks: i64) -> i64 {
    (ticks as i128 * 1_000_000_000 / qpc_frequency() as i128) as i64
}

/// A time in the system's 100-nanosecond units that's derived from the performance counter
/// (Windows Graphics Capture's `SystemRelativeTime`, WASAPI's QPC position, Media Foundation's
/// system time) as nanoseconds.
pub(crate) fn hns_ns(hns: i64) -> i64 {
    hns.saturating_mul(100)
}

thread_local! {
    static COM: Cell<bool> = const { Cell::new(false) };
}

/// Joins this thread to COM's multithreaded apartment (once a thread; a thread already in a
/// single-threaded apartment stays there, which every API used here also works from).
pub(crate) fn com() {
    COM.with(|done| {
        if !done.get() {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
                done.set(true);
            }
        }
    });
}

/// A Windows error as one of the crate's, saying what was being done.
pub(crate) fn fail(what: &str, error: windows::core::Error) -> Error {
    if error.code() == E_ACCESSDENIED {
        Error::Permission(format!("{what}: {}", error.message()))
    } else {
        Error::Failed(format!("{what}: {} ({:#010x})", error.message(), error.code().0))
    }
}

/// Whether this Windows has the given major version of the Universal API contract: 8 is
/// Windows 10 1903, 10 is 2004 (build 19041), 14 is Windows 11 21H2, 15 is 22H2, 19 is 24H2.
pub(crate) fn contract(major: u16) -> bool {
    windows::Foundation::Metadata::ApiInformation::IsApiContractPresentByMajor(
        &HSTRING::from("Windows.Foundation.UniversalApiContract"),
        major,
    )
    .unwrap_or(false)
}

/// UTF-16 with a terminating zero, for Win32 calls.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Text from a zero-terminated (or full) UTF-16 buffer.
pub(crate) fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// A handle closed when dropped.
pub(crate) struct Owned(pub HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// A kernel handle is a number any thread may use.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

/// A running process.
#[derive(Debug, Clone)]
pub(crate) struct Process {
    pub pid: u32,
    pub parent: u32,
    /// Its executable's full path, where this process may read it (empty otherwise).
    pub exe: String,
}

/// A process's executable, as a full path.
pub(crate) fn exe_of(pid: u32) -> Option<String> {
    let process = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    unsafe { QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len) }.ok()?;
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Every running process, with its executable's path where it can be read.
pub(crate) fn processes() -> Vec<Process> {
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let snapshot = Owned(snapshot);
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut out = Vec::new();
    if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_err() {
        return out;
    }
    loop {
        let pid = entry.th32ProcessID;
        if pid != 0 {
            out.push(Process {
                pid,
                parent: entry.th32ParentProcessID,
                exe: exe_of(pid).unwrap_or_else(|| from_wide(&entry.szExeFile)),
            });
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    out
}

/// Whether two executables are the same: the same full path (any case), or, where either is
/// only a file name, the same file name.
pub(crate) fn same_exe(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    let name = |s: &str| s.rsplit(['\\', '/']).next().unwrap_or(s).to_ascii_lowercase();
    let full = |s: &str| s.contains(['\\', '/']);
    (!full(a) || !full(b)) && name(a) == name(b)
}

/// The processes running `exe` whose parent isn't also running it (a browser's or launcher's
/// first process, not its helpers), lowest id first.
pub(crate) fn roots_of(exe: &str) -> Vec<u32> {
    let all = processes();
    let mut roots: Vec<u32> = all
        .iter()
        .filter(|p| same_exe(&p.exe, exe))
        .filter(|p| !all.iter().any(|q| q.pid == p.parent && same_exe(&q.exe, exe)))
        .map(|p| p.pid)
        .collect();
    roots.sort_unstable();
    roots
}

/// A file's name without its folder or extension ("game" for `C:\Games\game.exe`).
pub(crate) fn stem(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name).to_string()
}

/// Where a backend hands its frames: the host's sink, or the Game source's relay that also
/// watches what arrives.
pub(crate) trait Deliver: Send + Sync + 'static {
    fn frame(&self, frame: crate::Frame);
    fn stopped(&self, reason: &str);
}

/// The host's sink, called one frame at a time, told it stopped once at most.
pub(crate) struct Sink {
    sink: parking_lot::Mutex<Box<dyn crate::VideoSink>>,
    stopped: std::sync::atomic::AtomicBool,
}

impl Sink {
    pub fn new(sink: Box<dyn crate::VideoSink>) -> std::sync::Arc<Sink> {
        std::sync::Arc::new(Sink { sink: parking_lot::Mutex::new(sink), stopped: false.into() })
    }
}

impl Deliver for Sink {
    fn frame(&self, frame: crate::Frame) {
        if !self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            self.sink.lock().frame(frame);
        }
    }

    fn stopped(&self, reason: &str) {
        if !self.stopped.swap(true, std::sync::atomic::Ordering::Relaxed) {
            self.sink.lock().stopped(reason);
        }
    }
}

/// Frames a second as the time between frames, in nanoseconds (none for 0).
pub(crate) fn interval_ns(fps: u32) -> i64 {
    if fps == 0 { 0 } else { 1_000_000_000 / fps as i64 }
}
