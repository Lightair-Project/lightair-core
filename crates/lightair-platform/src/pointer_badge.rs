//! A small badge by the real pointer, on the user's screen only, while what viewers see is
//! zoomed in (the live follow-zoom, `lightair_engine::pointer::live_zoom`): a magnifier and how
//! far ("2×"). Clicks go through it, it never takes focus, and it's on every Space.
//!
//! Kept out of the picture: the window is marked not to be shared (NSWindowSharingNone), and
//! ScreenCaptureKit leaves it out of display streams, OBS's screen capture's included, even
//! when that shows Lightair's own windows (checked on macOS 27: a stream with OBS's filter, a
//! not-shared window over a window of ours; the shared control showed it). Other platforms: none
//! yet.
//!
//! Main thread only.

/// The badge (hidden until shown).
pub struct Badge {
    imp: imp::Badge,
}

impl Badge {
    /// `None` where there's no badge (not macOS).
    pub fn new() -> Option<Badge> {
        imp::Badge::new().map(|imp| Badge { imp })
    }

    /// Shows it by the pointer with the zoom it's at, or hides it (`None`).
    pub fn show(&mut self, zoom: Option<f32>) {
        self.imp.show(zoom)
    }
}

/// The zoom as the badge says it: "2×", "1.5×".
pub fn label(zoom: f32) -> String {
    let z = (zoom * 10.0).round() / 10.0;
    if (z - z.round()).abs() < 0.05 { format!("{}×", z.round() as i64) } else { format!("{z:.1}×") }
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::msg_send;
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSString;

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

    /// A CGColorRef (opaque).
    #[repr(C)]
    struct CgColor {
        _private: [u8; 0],
    }
    unsafe impl RefEncode for CgColor {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
    }

    // NSPanel, NSTextField, NSImageView
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    /// Its size (points), and where it sits from the pointer's tip (below right of the arrow).
    const W: f64 = 58.0;
    const H: f64 = 24.0;
    const OFFSET: (f64, f64) = (16.0, 20.0);

    pub struct Badge {
        panel: Retained<AnyObject>,
        label: Retained<AnyObject>,
        text: String,
        shown: bool,
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { origin: Point { x, y }, size: Size { w, h } }
    }

    fn color(r: f64, g: f64, b: f64, a: f64) -> Option<Retained<AnyObject>> {
        let class = AnyClass::get(c"NSColor")?;
        unsafe { msg_send![class, colorWithSRGBRed: r, green: g, blue: b, alpha: a] }
    }

    impl Badge {
        pub fn new() -> Option<Badge> {
            unsafe {
                let panel_class = AnyClass::get(c"NSPanel")?;
                let panel: Allocated<AnyObject> = msg_send![panel_class, alloc];
                // borderless (0) | non-activating panel (1 << 7); buffered (2)
                let panel: Retained<AnyObject> = msg_send![panel, initWithContentRect: rect(0.0, 0.0, W, H), styleMask: 1usize << 7, backing: 2usize, defer: Bool::NO];
                let clear: Retained<AnyObject> = msg_send![AnyClass::get(c"NSColor")?, clearColor];
                let _: () = msg_send![&*panel, setOpaque: Bool::NO];
                let _: () = msg_send![&*panel, setBackgroundColor: &*clear];
                let _: () = msg_send![&*panel, setHasShadow: Bool::YES];
                let _: () = msg_send![&*panel, setIgnoresMouseEvents: Bool::YES];
                let _: () = msg_send![&*panel, setHidesOnDeactivate: Bool::NO];
                let _: () = msg_send![&*panel, setReleasedWhenClosed: Bool::NO];
                // above everything, the menu bar and full-screen apps included (screen saver level)
                let _: () = msg_send![&*panel, setLevel: 1000isize];
                // every Space, stays put, not in the window cycle, beside full-screen apps
                let _: () = msg_send![&*panel, setCollectionBehavior: (1usize | 1 << 4 | 1 << 6 | 1 << 8)];
                // NSWindowSharingNone: never in a screen capture
                let _: () = msg_send![&*panel, setSharingType: 0usize];

                // a dark pill
                let content: Retained<AnyObject> = msg_send![&*panel, contentView];
                let _: () = msg_send![&*content, setWantsLayer: Bool::YES];
                let layer: Option<Retained<AnyObject>> = msg_send![&*content, layer];
                if let (Some(layer), Some(fill), Some(edge)) =
                    (layer, color(0.08, 0.08, 0.09, 0.86), color(1.0, 0.565, 0.0, 1.0))
                {
                    let fill: *mut CgColor = msg_send![&*fill, CGColor];
                    let edge: *mut CgColor = msg_send![&*edge, CGColor];
                    let _: () = msg_send![&*layer, setBackgroundColor: fill];
                    let _: () = msg_send![&*layer, setBorderColor: edge];
                    let _: () = msg_send![&*layer, setBorderWidth: 1.5f64];
                    let _: () = msg_send![&*layer, setCornerRadius: H / 2.0];
                }
                let white = color(1.0, 1.0, 1.0, 1.0)?;
                // the magnifier (SF Symbols)
                if let Some(image_class) = AnyClass::get(c"NSImage") {
                    let name = NSString::from_str("plus.magnifyingglass");
                    let nil: *mut AnyObject = std::ptr::null_mut();
                    let image: Option<Retained<AnyObject>> =
                        msg_send![image_class, imageWithSystemSymbolName: &*name, accessibilityDescription: nil];
                    if let (Some(image), Some(view_class)) = (image, AnyClass::get(c"NSImageView")) {
                        let view: Retained<AnyObject> = msg_send![view_class, imageViewWithImage: &*image];
                        let _: () = msg_send![&*view, setFrame: rect(8.0, 4.0, 16.0, 16.0)];
                        let _: () = msg_send![&*view, setContentTintColor: &*white];
                        let _: () = msg_send![&*content, addSubview: &*view];
                    }
                }
                // how far
                let field_class = AnyClass::get(c"NSTextField")?;
                let empty = NSString::from_str("");
                let label: Retained<AnyObject> = msg_send![field_class, labelWithString: &*empty];
                let font: Retained<AnyObject> =
                    msg_send![AnyClass::get(c"NSFont")?, monospacedDigitSystemFontOfSize: 12.0f64, weight: 0.4f64];
                let _: () = msg_send![&*label, setFont: &*font];
                let _: () = msg_send![&*label, setTextColor: &*white];
                let _: () = msg_send![&*label, setFrame: rect(26.0, 4.0, W - 30.0, 16.0)];
                let _: () = msg_send![&*content, addSubview: &*label];
                Some(Badge { panel, label, text: String::new(), shown: false })
            }
        }

        pub fn show(&mut self, zoom: Option<f32>) {
            let Some(zoom) = zoom else {
                if self.shown {
                    self.shown = false;
                    let _: () = unsafe { msg_send![&*self.panel, orderOut: std::ptr::null_mut::<AnyObject>()] };
                }
                return;
            };
            let text = super::label(zoom);
            if text != self.text {
                let s = NSString::from_str(&text);
                let _: () = unsafe { msg_send![&*self.label, setStringValue: &*s] };
                self.text = text;
            }
            // global points (top-left origin) → AppKit's (bottom-left of the main display)
            let (px, py) = crate::input::pointer();
            let (_, _, _, main_h) = crate::input::main_display();
            let origin = Point { x: px as f64 + OFFSET.0, y: main_h as f64 - (py as f64 + OFFSET.1 + H) };
            unsafe {
                let _: () = msg_send![&*self.panel, setFrameOrigin: origin];
                if !self.shown {
                    let _: () = msg_send![&*self.panel, orderFrontRegardless];
                }
            }
            self.shown = true;
        }
    }

    impl Drop for Badge {
        fn drop(&mut self) {
            let _: () = unsafe { msg_send![&*self.panel, close] };
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub struct Badge;

    impl Badge {
        pub fn new() -> Option<Badge> {
            None
        }

        pub fn show(&mut self, _: Option<f32>) {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_zoom_reads_short() {
        assert_eq!(super::label(2.0), "2×");
        assert_eq!(super::label(1.5), "1.5×");
        assert_eq!(super::label(2.49), "2.5×");
        assert_eq!(super::label(2.98), "3×");
    }
}
