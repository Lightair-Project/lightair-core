//! The eyedropper: pick any pixel on screen. macOS: its own colour sampler (NSColorSampler), the
//! magnifying loupe. Windows has none, so Lightair makes one: a snapshot of every display under
//! a window that covers them, with a loupe at the pointer. Click picks, Escape (or a right
//! click) cancels. Main thread only.

use std::sync::mpsc;

/// Shows the loupe; the receiver gets the picked colour as 0xRRGGBB (sRGB), or `None` if the
/// pick was cancelled.
pub fn pick() -> mpsc::Receiver<Option<u32>> {
    let (tx, rx) = mpsc::channel();
    imp::pick(tx);
    rx
}

/// A Windows COLORREF (0x00BBGGRR) as 0xRRGGBB.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn from_colorref(c: u32) -> u32 {
    ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::mpsc;

    use block2::RcBlock;
    use objc2_app_kit::{NSColor, NSColorSampler, NSColorSpace};

    pub fn pick(tx: mpsc::Sender<Option<u32>>) {
        let handler = RcBlock::new(move |color: *mut NSColor| {
            let picked = unsafe { color.as_ref() }.and_then(|c| {
                let srgb = c.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
                let ch = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
                Some((ch(srgb.redComponent()) << 16) | (ch(srgb.greenComponent()) << 8) | ch(srgb.blueComponent()))
            });
            let _ = tx.send(picked);
        });
        let sampler = NSColorSampler::new();
        unsafe { sampler.showSamplerWithSelectionHandler(&handler) };
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, DeleteDC, DeleteObject, EndPaint,
        GetDC, GetPixel, GetStockObject, HBITMAP, HDC, HGDIOBJ, NULL_BRUSH, PAINTSTRUCT, PS_SOLID, Rectangle,
        ReleaseDC, SRCCOPY, SelectObject, SetStretchBltMode, COLORONCOLOR, StretchBlt, InvalidateRect,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetSystemMetrics, GetWindowLongPtrW, IDC_CROSS,
        LoadCursorW, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        SW_SHOW, SetCursor, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, WM_DESTROY, WM_ERASEBKGND,
        WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WNDCLASSW, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_POPUP,
    };

    /// Pixels either side of the pointer the loupe shows, and how much it magnifies them.
    const REACH: i32 = 7;
    const ZOOM: i32 = 10;

    struct State {
        /// The snapshot of every display, in a memory DC.
        shot: HDC,
        shot_bitmap: HBITMAP,
        old: HGDIOBJ,
        width: i32,
        height: i32,
        /// The pointer, in the window's (the snapshot's) pixels.
        at: (i32, i32),
        tx: Option<mpsc::Sender<Option<u32>>>,
    }

    impl State {
        fn answer(&mut self, colour: Option<u32>) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(colour);
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn state<'a>(hwnd: HWND) -> Option<&'a mut State> {
        unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State).as_mut() }
    }

    unsafe fn paint(hwnd: HWND, s: &State) {
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            // draw off screen, then once: no flicker
            let back = CreateCompatibleDC(dc);
            let bitmap = CreateCompatibleBitmap(dc, s.width, s.height);
            let old = SelectObject(back, bitmap as HGDIOBJ);
            BitBlt(back, 0, 0, s.width, s.height, s.shot, 0, 0, SRCCOPY);
            let side = (2 * REACH + 1) * ZOOM;
            // beside the pointer, kept on screen
            let mut x = s.at.0 + 24;
            let mut y = s.at.1 + 24;
            if x + side > s.width {
                x = s.at.0 - 24 - side;
            }
            if y + side > s.height {
                y = s.at.1 - 24 - side;
            }
            SetStretchBltMode(back, COLORONCOLOR);
            StretchBlt(
                back,
                x,
                y,
                side,
                side,
                s.shot,
                s.at.0 - REACH,
                s.at.1 - REACH,
                2 * REACH + 1,
                2 * REACH + 1,
                SRCCOPY,
            );
            let pen = CreatePen(PS_SOLID, 2, 0x00FF_FFFF);
            let old_pen = SelectObject(back, pen as HGDIOBJ);
            let old_brush = SelectObject(back, GetStockObject(NULL_BRUSH));
            Rectangle(back, x, y, x + side, y + side);
            // the pixel that will be picked
            let c = (x + REACH * ZOOM, y + REACH * ZOOM);
            Rectangle(back, c.0, c.1, c.0 + ZOOM, c.1 + ZOOM);
            SelectObject(back, old_brush);
            SelectObject(back, old_pen);
            DeleteObject(pen as HGDIOBJ);
            BitBlt(dc, 0, 0, s.width, s.height, back, 0, 0, SRCCOPY);
            SelectObject(back, old);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteDC(back);
            EndPaint(hwnd, &ps);
        }
    }

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        let Some(s) = state(hwnd) else { return unsafe { DefWindowProcW(hwnd, msg, w, l) } };
        let point = || ((l & 0xFFFF) as i16 as i32, ((l >> 16) & 0xFFFF) as i16 as i32);
        match msg {
            WM_PAINT => {
                unsafe { paint(hwnd, s) };
                0
            }
            WM_ERASEBKGND => 1,
            WM_SETCURSOR => {
                unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_CROSS)) };
                1
            }
            WM_MOUSEMOVE => {
                s.at = point();
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
                0
            }
            WM_LBUTTONDOWN => {
                let (x, y) = point();
                let c = unsafe { GetPixel(s.shot, x, y) };
                s.answer((c != u32::MAX).then(|| super::from_colorref(c)));
                unsafe { DestroyWindow(hwnd) };
                0
            }
            WM_RBUTTONDOWN => {
                s.answer(None);
                unsafe { DestroyWindow(hwnd) };
                0
            }
            WM_KEYDOWN if w as u16 == VK_ESCAPE => {
                s.answer(None);
                unsafe { DestroyWindow(hwnd) };
                0
            }
            WM_DESTROY => {
                // SAFETY: the box made in `pick`; nothing reads it after this
                let mut s = unsafe { Box::from_raw(s as *mut State) };
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                s.answer(None);
                unsafe {
                    SelectObject(s.shot, s.old);
                    DeleteObject(s.shot_bitmap as HGDIOBJ);
                    DeleteDC(s.shot);
                }
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
        }
    }

    fn class() -> *const u16 {
        static NAME: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
        NAME.get_or_init(|| {
            let name = wide("LightairEyedropper");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
                lpszClassName: name.as_ptr(),
                ..unsafe { std::mem::zeroed() }
            };
            unsafe { RegisterClassW(&wc) };
            name
        })
        .as_ptr()
    }

    pub fn pick(tx: mpsc::Sender<Option<u32>>) {
        unsafe {
            let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            let (width, height) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
            // the snapshot, taken before the window covers the screen
            let screen = GetDC(std::ptr::null_mut());
            let shot = CreateCompatibleDC(screen);
            let shot_bitmap = CreateCompatibleBitmap(screen, width, height);
            let old = SelectObject(shot, shot_bitmap as HGDIOBJ);
            BitBlt(shot, 0, 0, width, height, screen, x, y, SRCCOPY);
            ReleaseDC(std::ptr::null_mut(), screen);
            let state = Box::new(State { shot, shot_bitmap, old, width, height, at: (width / 2, height / 2), tx: Some(tx) });
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class(),
                std::ptr::null(),
                WS_POPUP,
                x,
                y,
                width,
                height,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            let raw = Box::into_raw(state);
            if hwnd.is_null() {
                let mut s = Box::from_raw(raw);
                s.answer(None);
                SelectObject(s.shot, s.old);
                DeleteObject(s.shot_bitmap as HGDIOBJ);
                DeleteDC(s.shot);
                return;
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn pick(tx: std::sync::mpsc::Sender<Option<u32>>) {
        let _ = tx.send(None);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_colours_read_as_rgb() {
        assert_eq!(super::from_colorref(0x0033_2211), 0x11_2233);
    }
}
