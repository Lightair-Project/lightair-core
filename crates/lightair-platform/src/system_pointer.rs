//! The system pointer as the OS draws it now (macOS): its picture, and which of the standard
//! shapes it is. The drawn pointer (`lightair_engine::pointer`) uses Lightair's own art for the
//! arrow, I-beam and crosshair, and this picture for everything else (a hand, a resize arrow, an
//! app's own pointer); the Studio take's input log records the shape.
//!
//! Read at most every 30 ms however many ask (the recorder and each drawn pointer do).

use std::sync::Arc;

use crate::input::Shape;

/// The picture's side in pixels: a 32-point pointer at 4× (as `lightair_render::shapes::RES`).
pub const SIDE: usize = 128;

/// The pointer's picture: premultiplied RGBA, SIDE×SIDE, the pointer from the top-left corner.
#[derive(Debug, Clone)]
pub struct Picture {
    pub rgba: Arc<[u8]>,
    /// The hot spot, as a fraction of the side.
    pub hot: (f32, f32),
    /// Changes when the picture does.
    pub serial: u64,
    /// The standard shape it is; `None`: none of them (an app's own pointer).
    pub shape: Option<Shape>,
}

/// The pointer now; `None` where it can't be read (not macOS, or none showing).
pub fn now() -> Option<Picture> {
    imp::now()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Picture, SIDE, Shape};
    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::msg_send;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }
    unsafe impl Encode for Point {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        w: f64,
        h: f64,
    }
    unsafe impl Encode for Size {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Size,
    }
    unsafe impl Encode for Rect {
        const ENCODING: Encoding = Encoding::Struct("CGRect", &[Point::ENCODING, Size::ENCODING]);
    }
    unsafe impl RefEncode for Rect {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    /// A CGImageRef (opaque).
    #[repr(C)]
    struct CgImage {
        _private: [u8; 0],
    }
    unsafe impl RefEncode for CgImage {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
    }

    // NSCursor
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits: usize,
            row: usize,
            space: *mut c_void,
            info: u32,
        ) -> *mut c_void;
        fn CGContextDrawImage(context: *mut c_void, rect: Rect, image: *mut c_void);
        fn CGContextRelease(context: *mut c_void);
    }

    /// kCGImageAlphaPremultipliedLast: RGBA, premultiplied.
    const PREMULTIPLIED_LAST: u32 = 1;

    /// A cursor drawn into a SIDE square (from the top-left, at 4 pixels a point, smaller if
    /// it's bigger than 32 points), and its hot spot as a fraction of the side.
    fn draw(cursor: &AnyObject) -> Option<(Vec<u8>, (f32, f32))> {
        unsafe {
            let image: Option<Retained<AnyObject>> = msg_send![cursor, image];
            let image = image?;
            let size: Size = msg_send![&*image, size];
            let hot: Point = msg_send![cursor, hotSpot];
            if size.w <= 0.0 || size.h <= 0.0 {
                return None;
            }
            let k = 4.0 * (32.0 / size.w.max(size.h)).min(1.0);
            let (w, h) = (size.w * k, size.h * k);
            let mut want = Rect { origin: Point { x: 0.0, y: 0.0 }, size: Size { w, h } };
            let nil: *mut AnyObject = std::ptr::null_mut();
            let cg: *mut CgImage = msg_send![&*image, CGImageForProposedRect: &mut want, context: nil, hints: nil];
            if cg.is_null() {
                return None;
            }
            let mut rgba = vec![0u8; SIDE * SIDE * 4];
            let space = CGColorSpaceCreateDeviceRGB();
            let context =
                CGBitmapContextCreate(rgba.as_mut_ptr().cast(), SIDE, SIDE, 8, SIDE * 4, space, PREMULTIPLIED_LAST);
            CGColorSpaceRelease(space);
            if context.is_null() {
                return None;
            }
            // the context is y-up; its first row in memory is the top
            let side = SIDE as f64;
            CGContextDrawImage(context, Rect { origin: Point { x: 0.0, y: side - h }, size: Size { w, h } }, cg.cast());
            CGContextRelease(context);
            Some((rgba, ((hot.x * k / side) as f32, (hot.y * k / side) as f32)))
        }
    }

    /// How unlike two pictures are: the mean difference of their alpha (0–255).
    fn unlike(a: &[u8], b: &[u8]) -> f32 {
        let sum: u64 = a.iter().zip(b).skip(3).step_by(4).map(|(x, y)| x.abs_diff(*y) as u64).sum();
        sum as f32 / (SIDE * SIDE) as f32
    }

    /// A standard cursor: its shape, its picture, its hot spot.
    pub(super) type Standard = (Shape, Vec<u8>, (f32, f32));

    /// The standard cursors, drawn the same way, to tell which one the pointer is.
    pub(super) fn standards() -> &'static Vec<Standard> {
        static ALL: OnceLock<Vec<Standard>> = OnceLock::new();
        ALL.get_or_init(|| {
            let Some(class) = AnyClass::get(c"NSCursor") else { return Vec::new() };
            let named: [(Shape, &str); 11] = [
                (Shape::Arrow, "arrowCursor"),
                (Shape::IBeam, "IBeamCursor"),
                (Shape::Hand, "pointingHandCursor"),
                (Shape::Crosshair, "crosshairCursor"),
                (Shape::ResizeHorizontal, "resizeLeftRightCursor"),
                (Shape::ResizeHorizontal, "columnResizeCursor"),
                (Shape::ResizeVertical, "resizeUpDownCursor"),
                (Shape::ResizeVertical, "rowResizeCursor"),
                (Shape::NotAllowed, "operationNotAllowedCursor"),
                (Shape::Move, "openHandCursor"),
                (Shape::Move, "closedHandCursor"),
            ];
            autoreleasepool(|_| {
                named
                    .iter()
                    .filter_map(|&(shape, name)| {
                        let sel = objc2::runtime::Sel::register(&std::ffi::CString::new(name).ok()?);
                        let known: objc2::runtime::Bool = unsafe { msg_send![class, respondsToSelector: sel] };
                        if !known.as_bool() {
                            return None;
                        }
                        let cursor: Option<Retained<AnyObject>> = unsafe { msg_send![class, performSelector: sel] };
                        let (rgba, hot) = draw(&*cursor?)?;
                        Some((shape, rgba, hot))
                    })
                    .collect()
            })
        })
    }

    pub(super) fn classify(rgba: &[u8], hot: (f32, f32)) -> Option<Shape> {
        let near = |a: (f32, f32), b: (f32, f32)| {
            (a.0 - b.0).abs() * SIDE as f32 <= 6.0 && (a.1 - b.1).abs() * SIDE as f32 <= 6.0
        };
        standards()
            .iter()
            .filter(|(_, _, h)| near(*h, hot))
            .map(|(s, p, _)| (*s, unlike(p, rgba)))
            .filter(|(_, d)| *d < 6.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(s, _)| s)
            .or_else(|| by_form(rgba, hot))
    }

    /// The arrow and the I-beam by their form: macOS draws those two itself (their NSCursor
    /// pictures are empty), so they can't be compared. In points: the arrow's tip is its hot
    /// spot, at its top-left; the I-beam is tall and thin around its hot spot.
    pub(super) fn by_form(rgba: &[u8], hot: (f32, f32)) -> Option<Shape> {
        let (mut x0, mut y0, mut x1, mut y1) = (SIDE, SIDE, 0, 0);
        for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
            if px[3] > 64 {
                let (x, y) = (i % SIDE, i / SIDE);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
        if x1 < x0 {
            return None;
        }
        let pt = |v: usize| v as f32 * 32.0 / SIDE as f32;
        let (x0, y0, x1, y1) = (pt(x0), pt(y0), pt(x1 + 1), pt(y1 + 1));
        let (hx, hy) = (hot.0 * 32.0, hot.1 * 32.0);
        let (w, h) = (x1 - x0, y1 - y0);
        if (hx - x0).abs() <= 3.0 && (hy - y0).abs() <= 3.0 && h > w && h >= 12.0 {
            return Some(Shape::Arrow);
        }
        if w <= 12.0
            && h >= 12.0
            && h > 1.6 * w
            && (hx - (x0 + x1) / 2.0).abs() <= 2.5
            && (hy - (y0 + y1) / 2.0).abs() <= 4.0
        {
            return Some(Shape::IBeam);
        }
        None
    }

    struct Cache {
        read: Option<Instant>,
        picture: Option<Picture>,
        serial: u64,
    }

    pub fn now() -> Option<Picture> {
        static CACHE: Mutex<Cache> = Mutex::new(Cache { read: None, picture: None, serial: 0 });
        let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if c.read.is_some_and(|t| t.elapsed() < Duration::from_millis(30)) {
            return c.picture.clone();
        }
        c.read = Some(Instant::now());
        let class = AnyClass::get(c"NSCursor")?;
        let drawn = autoreleasepool(|_| {
            let cursor: Option<Retained<AnyObject>> = unsafe { msg_send![class, currentSystemCursor] };
            draw(&*cursor?)
        });
        let Some((rgba, hot)) = drawn else {
            c.picture = None;
            return None;
        };
        if let Some(p) = &c.picture
            && p.hot == hot
            && *p.rgba == *rgba
        {
            return c.picture.clone();
        }
        c.serial += 1;
        let shape = classify(&rgba, hot);
        c.picture = Some(Picture { rgba: Arc::from(rgba), hot, serial: c.serial, shape });
        c.picture.clone()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn now() -> Option<super::Picture> {
        None
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Each standard cursor is told as its own shape, and the pointer now can be read.
    #[test]
    #[ignore = "depends on this Mac's window server (cargo xtask test --machine)"]
    fn standard_cursors_are_told_apart() {
        let all = imp::standards();
        assert!(all.len() >= 8, "only {} standard cursors drawn", all.len());
        for (shape, rgba, hot) in all {
            assert!(rgba.iter().skip(3).step_by(4).any(|a| *a > 0), "{shape:?} drew nothing");
            assert_eq!(imp::classify(rgba, *hot), Some(*shape));
        }
        assert!(now().is_some());
    }

    /// A picture with a box filled, in points (x, y, w, h).
    fn filled(x: f32, y: f32, w: f32, h: f32) -> Vec<u8> {
        let mut rgba = vec![0u8; SIDE * SIDE * 4];
        let px = |v: f32| (v * SIDE as f32 / 32.0) as usize;
        for row in px(y)..px(y + h) {
            for col in px(x)..px(x + w) {
                rgba[(row * SIDE + col) * 4 + 3] = 255;
            }
        }
        rgba
    }

    #[test]
    fn the_arrow_and_i_beam_are_told_by_their_form() {
        let frac = |x: f32, y: f32| (x / 32.0, y / 32.0);
        assert_eq!(imp::by_form(&filled(4.0, 4.0, 12.0, 18.0), frac(4.0, 4.0)), Some(Shape::Arrow));
        assert_eq!(imp::by_form(&filled(14.0, 7.0, 4.0, 18.0), frac(16.0, 16.0)), Some(Shape::IBeam));
        // a round thing with its hot spot in the middle: neither
        assert_eq!(imp::by_form(&filled(8.0, 8.0, 16.0, 16.0), frac(16.0, 16.0)), None);
        assert_eq!(imp::by_form(&vec![0u8; SIDE * SIDE * 4], frac(4.0, 4.0)), None);
    }
}
