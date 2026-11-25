//! Displays and windows: what there is (GDI, the display configuration, the window list), and
//! captures of them (Windows Graphics Capture, Desktop Duplication).

use std::sync::Arc;

use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};
use windows::core::{BOOL, PCWSTR};

use super::{Deliver, Sink, duplication, exe_of, from_wide, same_exe, stem, wgc};
use crate::screen::{App, Backend, Config, Display, Shareable, Target, Window};
use crate::{Error, VideoSink};

/// Windows lets every desktop app capture the screen: whether Windows Graphics Capture is there
/// (Windows 10 1903 and later) says whether captures work as they should.
pub fn access() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// A display as captures need it.
#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    pub handle: HMONITOR,
    pub display: Display,
    /// GDI's name for it (`\\.\DISPLAY1`).
    pub gdi: String,
    /// Where it is on the desktop, in pixels.
    pub rect: RECT,
}

// A monitor handle is a number any thread may use.
unsafe impl Send for Monitor {}
unsafe impl Sync for Monitor {}

/// What the display configuration says of each display, by GDI name: (friendly name, device
/// path, built in).
fn configuration() -> Vec<(String, String, String, bool)> {
    let mut paths_len = 0u32;
    let mut modes_len = 0u32;
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_len, &mut modes_len) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_len as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_len as usize];
    let read = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths_len,
            paths.as_mut_ptr(),
            &mut modes_len,
            modes.as_mut_ptr(),
            None,
        )
    };
    if read != ERROR_SUCCESS {
        return Vec::new();
    }
    paths.truncate(paths_len as usize);
    paths
        .iter()
        .filter_map(|path| {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
                return None;
            }
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            let (name, device, built_in) = if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == 0 {
                let technology = target.outputTechnology;
                (
                    from_wide(&target.monitorFriendlyDeviceName),
                    from_wide(&target.monitorDevicePath),
                    technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                        || technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                        || technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
                )
            } else {
                (String::new(), String::new(), false)
            };
            Some((from_wide(&source.viewGdiDeviceName), name, device, built_in))
        })
        .collect()
}

unsafe extern "system" fn add_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<HMONITOR>) };
    list.push(monitor);
    true.into()
}

/// The displays, with what captures need of them.
pub(crate) fn monitors() -> Vec<Monitor> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(add_monitor), LPARAM(&mut handles as *mut _ as isize)) };
    let config = configuration();
    let mut numbered = 0;
    handles
        .into_iter()
        .filter_map(|handle| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if !unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }.as_bool() {
                return None;
            }
            let gdi = from_wide(&info.szDevice);
            let rect = info.monitorInfo.rcMonitor;
            let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
            let gdi_wide = super::wide(&gdi);
            let pixel_size =
                if unsafe { EnumDisplaySettingsW(PCWSTR(gdi_wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) }
                    .as_bool()
                {
                    (mode.dmPelsWidth, mode.dmPelsHeight)
                } else {
                    ((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32)
                };
            let (mut dpi, mut dpi_y) = (96u32, 96u32);
            let _ = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) };
            let scale = dpi.max(1) as f64 / 96.0;
            let size = ((pixel_size.0 as f64 / scale).round() as u32, (pixel_size.1 as f64 / scale).round() as u32);
            // "\\.\DISPLAY3" is display 3 while it's connected
            let id = gdi.trim_start_matches(r"\\.\DISPLAY").parse().unwrap_or_else(|_| {
                numbered += 1;
                1000 + numbered
            });
            let found = config.iter().find(|(g, ..)| g.eq_ignore_ascii_case(&gdi));
            let (name, uuid, built_in) = match found {
                Some((_, name, device, built_in)) => (
                    if name.is_empty() { format!("Display {id}") } else { name.clone() },
                    if device.is_empty() { gdi.clone() } else { device.clone() },
                    *built_in,
                ),
                None => (format!("Display {id}"), gdi.clone(), false),
            };
            let display = Display {
                id,
                uuid,
                name,
                size,
                pixel_size,
                origin: (rect.left as f64, rect.top as f64),
                main: info.monitorInfo.dwFlags & 1 != 0,
                built_in,
            };
            Some(Monitor { handle, display, gdi, rect })
        })
        .collect()
}

/// The display a setting names: its device path (`uuid`), its number as text, or its GDI name.
pub(crate) fn find_monitor(wanted: &str) -> Option<Monitor> {
    let all = monitors();
    let wanted = wanted.trim();
    all.iter()
        .find(|m| m.display.uuid.eq_ignore_ascii_case(wanted))
        .or_else(|| all.iter().find(|m| m.display.id.to_string() == wanted || m.gdi.eq_ignore_ascii_case(wanted)))
        .cloned()
}

/// The display a window is on (most of it).
pub(crate) fn monitor_of(window: HWND) -> Option<Monitor> {
    let handle = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    monitors().into_iter().find(|m| m.handle == handle)
}

pub fn displays() -> Vec<Display> {
    monitors().into_iter().map(|m| m.display).collect()
}

/// A window's handle from its id (an `HWND`'s significant 32 bits, which is all a window handle
/// holds, even in a 64-bit process).
pub(crate) fn hwnd(id: u32) -> HWND {
    HWND(id as i32 as isize as *mut _)
}

/// A window's frame on the desktop, in pixels, without its invisible resize borders.
pub(crate) fn frame_of(window: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(rect)
}

fn cloaked(window: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe { DwmGetWindowAttribute(window, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4) }.is_ok()
        && cloaked != 0
}

unsafe extern "system" fn add_window(window: HWND, list: LPARAM) -> BOOL {
    let list = unsafe { &mut *(list.0 as *mut Vec<HWND>) };
    list.push(window);
    true.into()
}

/// A top-level window as listed: its handle and what a picker shows.
#[derive(Debug, Clone)]
pub(crate) struct Listed {
    pub hwnd: HWND,
    pub window: Window,
    /// Its executable's full path.
    pub exe: Option<String>,
}

unsafe impl Send for Listed {}

/// Every visible top-level window someone might capture, front to back: not a tool window, not
/// owned by another window (dialogs and pop-ups go with their owner), not cloaked (another
/// virtual desktop, a suspended app), not Lightair's own.
pub(crate) fn top_windows() -> Vec<Listed> {
    let mut handles: Vec<HWND> = Vec::new();
    let _ = unsafe { EnumWindows(Some(add_window), LPARAM(&mut handles as *mut _ as isize)) };
    let own = std::process::id();
    handles
        .into_iter()
        .filter_map(|hwnd| {
            if !unsafe { IsWindowVisible(hwnd) }.as_bool() || cloaked(hwnd) {
                return None;
            }
            let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 || unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid()) {
                return None;
            }
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid == own {
                return None;
            }
            let mut title = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
            let title = String::from_utf16_lossy(&title[..len]);
            let rect = frame_of(hwnd).unwrap_or_default();
            let exe = exe_of(pid);
            let window = Window {
                id: hwnd.0 as isize as u32,
                title,
                app: exe.clone(),
                app_name: exe.as_deref().map(stem),
                pid: Some(pid as i32),
                size: ((rect.right - rect.left).max(0) as u32, (rect.bottom - rect.top).max(0) as u32),
                on_screen: !unsafe { IsIconic(hwnd) }.as_bool(),
                layer: if ex & WS_EX_TOPMOST.0 != 0 { 1 } else { 0 },
            };
            Some(Listed { hwnd, window, exe })
        })
        .collect()
}

/// The largest on-screen window of the app running `exe`.
pub(crate) fn main_window_of(exe: &str) -> Option<Listed> {
    top_windows()
        .into_iter()
        .filter(|l| l.window.on_screen && l.exe.as_deref().is_some_and(|e| same_exe(e, exe)))
        .max_by_key(|l| l.window.size.0 as u64 * l.window.size.1 as u64)
}

pub fn shareable() -> Result<Shareable, Error> {
    let windows: Vec<Window> = top_windows().into_iter().map(|l| l.window).collect();
    let mut apps: Vec<App> = Vec::new();
    for window in &windows {
        let (Some(exe), Some(pid)) = (&window.app, window.pid) else { continue };
        if !apps.iter().any(|a| same_exe(&a.bundle, exe)) {
            apps.push(App { bundle: exe.clone(), name: window.app_name.clone().unwrap_or_default(), pid });
        }
    }
    Ok(Shareable { displays: displays(), windows, apps })
}

/// A running capture of a display, window or app.
pub enum Stream {
    Graphics(wgc::Capture),
    Duplication(duplication::Capture),
}

impl Stream {
    pub fn start(target: &Target, config: &Config, sink: Box<dyn VideoSink>) -> Result<Stream, Error> {
        let sink: Arc<dyn Deliver> = Sink::new(sink);
        Stream::start_into(target, config, sink)
    }

    pub(crate) fn start_into(target: &Target, config: &Config, sink: Arc<dyn Deliver>) -> Result<Stream, Error> {
        match target {
            Target::Display(wanted) => {
                let monitor = find_monitor(wanted).ok_or_else(|| Error::NotFound(format!("display {wanted}")))?;
                let graphics = || wgc::Capture::monitor(&monitor, config, sink.clone()).map(Stream::Graphics);
                let duplication =
                    || duplication::Capture::start(&monitor, None, config, sink.clone()).map(Stream::Duplication);
                match config.backend {
                    Backend::Graphics => graphics(),
                    Backend::Duplication => duplication(),
                    Backend::Auto if access() => graphics().or_else(|e| {
                        tracing::info!("capture: Windows Graphics Capture of {wanted}: {e}; duplicating instead");
                        duplication()
                    }),
                    Backend::Auto => duplication(),
                }
            }
            Target::Window(id) => wgc::Capture::window(hwnd(*id), config, sink).map(Stream::Graphics),
            Target::App { bundle, .. } => {
                // Windows Graphics Capture has no app capture: the app's largest window it is
                let main = main_window_of(bundle).ok_or_else(|| Error::NotFound(format!("a window of {bundle}")))?;
                wgc::Capture::window(main.hwnd, config, sink).map(Stream::Graphics)
            }
        }
    }

    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        match self {
            Stream::Graphics(c) => c.reconfigure(config),
            Stream::Duplication(c) => c.reconfigure(config),
        }
    }

    pub fn size(&self) -> (u32, u32) {
        match self {
            Stream::Graphics(c) => c.size(),
            Stream::Duplication(c) => c.size(),
        }
    }
}
