//! Games' present modes, read from Windows' event tracing the way frame-timing tools read them,
//! without touching the game: a real-time session with the DirectX graphics kernel's and DXGI's
//! providers, filtered to the few events that say how a process's frames reach the screen.
//!
//! Which process logs which event tells the modes apart:
//!
//! - a flip logged by the game's own process (`Flip_Info`, `FlipMultiPlaneOverlay_Info`) is a
//!   legacy flip: the game owns the display (exclusive fullscreen) and other apps can't see its
//!   frames through the compositor;
//! - a present history token (`PresentHistory_Start`, `PresentHistoryDetailed_Start`) is the flip
//!   model, which the compositor composes or promotes to an independent flip: both capturable;
//! - a blit (`Blit_Info`) is a windowed bitblt present the compositor composes.
//!
//! Composed and independent flip aren't told apart here (that needs the window manager's token
//! events): both are captured the same way.
//!
//! Starting a session needs administrator rights or the "Performance Log Users" group;
//! [`Presents::start`] says so when it's refused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;

use parking_lot::Mutex;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID, EVENT_RECORD,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0, EVENT_TRACE_LOGFILEW_1,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW, TRACE_LEVEL_INFORMATION,
    WNODE_FLAG_TRACED_GUID,
};
use windows::core::{GUID, PCWSTR, PWSTR};

use super::{host_now_ns, qpc_ns, wide};

/// How a game's frames reach the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    /// A windowed blit the compositor composes.
    Composed,
    /// The flip model: composed by the compositor, or an independent flip it lets through
    /// (every DirectX 12 game, most modern ones). Windows Graphics Capture sees these.
    Flip,
    /// The game owns the display (exclusive fullscreen): only Desktop Duplication may see it.
    LegacyFlip,
    /// Nothing seen from the game in the last second (it's paused, minimised, or not
    /// presenting through DirectX's kernel).
    Unknown,
}

/// The DirectX graphics kernel's provider (`Microsoft-Windows-DxgKrnl`).
const DXGKRNL: GUID = GUID::from_u128(0x802ec45a_1e99_4b83_9920_87c98277ba9d);
/// DXGI's provider (`Microsoft-Windows-DXGI`).
const DXGI: GUID = GUID::from_u128(0xca11c036_0102_4a2d_a6ad_f03cfed5d3c9);
/// This session's own id.
const SESSION_GUID: GUID = GUID::from_u128(0x6c1a3e2f_5b7d_4c1e_9a2f_4c3b2a1d0e9f);
const SESSION_NAME: &str = "Lightair present modes";

// DxgKrnl's event ids
const BLIT_INFO: u16 = 166;
const FLIP_INFO: u16 = 168;
const PRESENT_HISTORY_START: u16 = 171;
const PRESENT_HISTORY_DETAILED_START: u16 = 215;
const FLIP_MPO_INFO: u16 = 252;
// DXGI's
const PRESENT_START: u16 = 42;

/// DxgKrnl's keywords: `Base` and its performance keyword.
const DXGKRNL_KEYWORDS: u64 = 0x1 | 0x8000_0000_0000_0000;

/// A process watched, and when (host ns) each kind of present was last seen from it.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    pid: u32,
    legacy: i64,
    flip: i64,
    blit: i64,
    any: i64,
}

#[derive(Default)]
struct Shared {
    watched: Mutex<Vec<Seen>>,
}

/// A running event tracing session that watches games present.
pub(crate) struct Presents {
    shared: Arc<Shared>,
    session: CONTROLTRACE_HANDLE,
    trace: PROCESSTRACE_HANDLE,
    stopped: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

unsafe impl Send for Presents {}
unsafe impl Sync for Presents {}

/// Why present modes can't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Windows wants administrator rights or the "Performance Log Users" group for it.
    Rights,
    Failed(String),
}

/// Properties for `StartTrace`/`ControlTrace`, with room for the session's name after them.
fn properties() -> Vec<u64> {
    let name = wide(SESSION_NAME);
    let bytes = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2 + 2048;
    let mut buffer = vec![0u64; bytes.div_ceil(8)];
    let props = buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    unsafe {
        (*props).Wnode.BufferSize = bytes as u32;
        (*props).Wnode.Guid = SESSION_GUID;
        // 1: event times on the performance counter
        (*props).Wnode.ClientContext = 1;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).FlushTimer = 1;
        (*props).BufferSize = 64;
        (*props).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    }
    buffer
}

fn stop_session(session: CONTROLTRACE_HANDLE) {
    let mut props = properties();
    let name = wide(SESSION_NAME);
    let by_name = if session.Value == 0 { PCWSTR(name.as_ptr()) } else { PCWSTR::null() };
    let _ = unsafe {
        ControlTraceW(session, by_name, props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES, EVENT_TRACE_CONTROL_STOP)
    };
}

/// Enables `provider` for `ids` only (all its events where the system can't filter by id).
fn enable(session: CONTROLTRACE_HANDLE, provider: &GUID, keywords: u64, ids: &[u16]) -> bool {
    // EVENT_FILTER_EVENT_ID: FilterIn, Reserved, Count, then the ids
    let mut filter: Vec<u8> = vec![1, 0];
    filter.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    for id in ids {
        filter.extend_from_slice(&id.to_le_bytes());
    }
    let mut descriptor = EVENT_FILTER_DESCRIPTOR {
        Ptr: filter.as_ptr() as u64,
        Size: filter.len() as u32,
        Type: EVENT_FILTER_TYPE_EVENT_ID,
    };
    let parameters = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableFilterDesc: &mut descriptor,
        FilterDescCount: 1,
        ..Default::default()
    };
    let level = TRACE_LEVEL_INFORMATION as u8;
    let code = EVENT_CONTROL_CODE_ENABLE_PROVIDER.0;
    let filtered = unsafe { EnableTraceEx2(session, provider, code, level, keywords, 0, 0, Some(&parameters)) };
    filtered == ERROR_SUCCESS
        || unsafe { EnableTraceEx2(session, provider, code, level, keywords, 0, 0, None) } == ERROR_SUCCESS
}

unsafe extern "system" fn record(event: *mut EVENT_RECORD) {
    let event = unsafe { &*event };
    let shared = event.UserContext as *const Shared;
    if shared.is_null() {
        return;
    }
    let shared = unsafe { &*shared };
    let header = &event.EventHeader;
    let id = header.EventDescriptor.Id;
    let kind = if header.ProviderId == DXGKRNL {
        match id {
            FLIP_INFO | FLIP_MPO_INFO => 0,
            PRESENT_HISTORY_START | PRESENT_HISTORY_DETAILED_START => 1,
            BLIT_INFO => 2,
            _ => return,
        }
    } else if header.ProviderId == DXGI && id == PRESENT_START {
        3
    } else {
        return;
    };
    let mut watched = shared.watched.lock();
    let Some(seen) = watched.iter_mut().find(|s| s.pid == header.ProcessId) else { return };
    let at = qpc_ns(header.TimeStamp);
    match kind {
        0 => seen.legacy = at,
        1 => seen.flip = at,
        2 => seen.blit = at,
        _ => {}
    }
    seen.any = at;
}

static RUNNING: Mutex<Weak<Presents>> = Mutex::new(Weak::new());

impl Presents {
    /// The process's session: the one running, or a new one.
    pub fn shared() -> Result<Arc<Presents>, Refused> {
        let mut running = RUNNING.lock();
        if let Some(presents) = running.upgrade() {
            return Ok(presents);
        }
        let presents = Arc::new(Presents::start()?);
        *running = Arc::downgrade(&presents);
        Ok(presents)
    }

    fn start() -> Result<Presents, Refused> {
        let name = wide(SESSION_NAME);
        let mut session = CONTROLTRACE_HANDLE::default();
        let mut props = properties();
        let mut started = unsafe {
            StartTraceW(&mut session, PCWSTR(name.as_ptr()), props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES)
        };
        if started == ERROR_ALREADY_EXISTS {
            // left over from a Lightair that didn't stop it: stop it and start again
            stop_session(CONTROLTRACE_HANDLE::default());
            let mut props = properties();
            started = unsafe {
                StartTraceW(&mut session, PCWSTR(name.as_ptr()), props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES)
            };
        }
        if started == ERROR_ACCESS_DENIED {
            return Err(Refused::Rights);
        }
        if started != ERROR_SUCCESS {
            return Err(Refused::Failed(format!("starting event tracing: error {}", started.0)));
        }
        let kernel = enable(
            session,
            &DXGKRNL,
            DXGKRNL_KEYWORDS,
            &[BLIT_INFO, FLIP_INFO, PRESENT_HISTORY_START, PRESENT_HISTORY_DETAILED_START, FLIP_MPO_INFO],
        );
        enable(session, &DXGI, 0, &[PRESENT_START]);
        if !kernel {
            stop_session(session);
            return Err(Refused::Failed("the graphics kernel's events can't be enabled".into()));
        }

        let shared = Arc::new(Shared::default());
        let mut name = wide(SESSION_NAME);
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_mut_ptr()),
            Anonymous1: EVENT_TRACE_LOGFILEW_0 {
                ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
            },
            Anonymous2: EVENT_TRACE_LOGFILEW_1 { EventRecordCallback: Some(record) },
            Context: Arc::as_ptr(&shared) as *mut _,
            ..Default::default()
        };
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == u64::MAX {
            stop_session(session);
            return Err(Refused::Failed("opening the event trace".into()));
        }
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-present-modes".into())
            .spawn(move || {
                let _keep = thread_shared;
                let _ = unsafe { ProcessTrace(&[trace], None, None) };
            })
            .map_err(|e| Refused::Failed(format!("starting the event trace thread: {e}")))?;
        Ok(Presents { shared, session, trace, stopped: AtomicBool::new(false), thread: Mutex::new(Some(thread)) })
    }

    /// Watches process `pid` (and stops watching others not in `keep`).
    pub fn watch(&self, pid: u32, keep: &[u32]) {
        let mut watched = self.shared.watched.lock();
        watched.retain(|s| s.pid == pid || keep.contains(&s.pid));
        if !watched.iter().any(|s| s.pid == pid) {
            watched.push(Seen { pid, ..Default::default() });
        }
    }

    /// How `pid` presented in the last second.
    pub fn mode(&self, pid: u32) -> PresentMode {
        let watched = self.shared.watched.lock();
        let Some(seen) = watched.iter().find(|s| s.pid == pid) else { return PresentMode::Unknown };
        let recent = |t: i64| t != 0 && host_now_ns() - t < 1_000_000_000;
        // the newest kind wins: a game switching modes shows both for a moment
        let newest = [
            (seen.legacy, PresentMode::LegacyFlip),
            (seen.flip, PresentMode::Flip),
            (seen.blit, PresentMode::Composed),
        ]
        .into_iter()
        .filter(|(t, _)| recent(*t))
        .max_by_key(|(t, _)| *t);
        match newest {
            Some((_, mode)) => mode,
            None => PresentMode::Unknown,
        }
    }

    /// Whether the session still runs.
    pub fn running(&self) -> bool {
        !self.stopped.load(Ordering::Relaxed) && self.thread.lock().as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Presents {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        stop_session(self.session);
        let _ = unsafe { CloseTrace(self.trace) };
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
    }
}
