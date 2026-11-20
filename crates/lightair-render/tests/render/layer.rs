//! macOS: previews presented into a `CAMetalLayer` (what a phone's player shows, and a window's
//! could): drawn there, nothing read back, the layer's drawable the preview's size. Needs a GPU.

#![cfg(target_os = "macos")]

use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Content, Sequence, ShapeKind, ShapeStyle};
use objc2_quartz_core::CAMetalLayer;

#[test]
fn previews_are_presented_into_a_layer() {
    let mut seq = Sequence::new((1920, 1080), 30);
    let main = seq.main_track();
    seq.place_content_on(
        Content::Shape(ShapeStyle {
            shape: ShapeKind::Rectangle,
            width: 1920.0,
            height: 1080.0,
            fill: Some((0x0E7C86, 1.0)),
            stroke: None,
            radius: 0.0,
        }),
        0,
        2_000,
        main,
    );
    let mut r = match SequenceRenderer::new_with(&seq, Some((640, 640))) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no GPU here ({e}); skipping");
            return;
        }
    };
    assert_eq!(r.frame(500).len(), 640 * 360 * 4, "read back");
    let layer = CAMetalLayer::new();
    let ptr = objc2::rc::Retained::as_ptr(&layer) as *mut std::ffi::c_void;
    // SAFETY: the layer lives to the end of the test, past the renderer's last frame
    assert!(unsafe { r.present_into_layer(Some(ptr)) });
    assert!(r.presents_into_layer());
    assert!(r.frame(500).is_empty(), "presented, not read back");
    let size = layer.drawableSize();
    assert_eq!((size.width, size.height), (640.0, 360.0));
    // a larger preview: a new compositor, still presenting, the drawable resized
    r.set_preview(Some((1280, 1280)));
    assert!(r.presents_into_layer() && r.frame(1_000).is_empty());
    let size = layer.drawableSize();
    assert_eq!((size.width, size.height), (1280.0, 720.0));
    // an export's frames are read back: none presented while there's no preview
    r.set_preview(None);
    assert!(!r.presents_into_layer());
    assert_eq!(r.frame(500).len(), 1920 * 1080 * 4);
    // and back to reading back
    r.set_preview(Some((640, 640)));
    assert!(unsafe { r.present_into_layer(None) });
    assert_eq!(r.frame(500).len(), 640 * 360 * 4);
}
