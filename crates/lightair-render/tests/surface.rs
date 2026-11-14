//! macOS: the player's frames drawn into IOSurfaces (no read back). Needs FFmpeg and a GPU.
#![cfg(target_os = "macos")]

use lightair_render::media;
use lightair_render::seqrender::{SequenceRenderer, Shown};
use lightair_render::sequence::{Media, Sequence};

/// The luma (Y) at the middle of an NV12 frame.
fn luma(f: &lightair_render::surface::Frame) -> u8 {
    use objc2_core_video::{CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress};
    let (w, h) = (CVPixelBufferGetWidth(f), CVPixelBufferGetHeight(f));
    unsafe {
        assert_eq!(CVPixelBufferLockBaseAddress(f, CVPixelBufferLockFlags::ReadOnly), 0);
        let y = CVPixelBufferGetBaseAddressOfPlane(f, 0) as *const u8;
        let v = *y.add(h / 2 * CVPixelBufferGetBytesPerRowOfPlane(f, 0) + w / 2);
        CVPixelBufferUnlockBaseAddress(f, CVPixelBufferLockFlags::ReadOnly);
        v
    }
}

#[test]
fn previews_come_as_surfaces_with_the_picture_in_them() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else { return };
    let mut seq = Sequence::new((640, 360), 30);
    for (name, colour) in [("red.mp4", "red"), ("blue.mp4", "blue")] {
        let p = dir.path().join(name);
        assert!(std::process::Command::new(&ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("color=c={colour}:s=320x180:r=30:d=1"), "-pix_fmt", "yuv420p"]).arg(&p).status().unwrap().success());
        let m = seq.add_media(Media::from_file(&p).unwrap());
        seq.append(m).unwrap();
    }
    let Ok(mut r) = SequenceRenderer::new_with(&seq, Some((320, 180))) else { return };
    r.set_zero_copy(true);
    let (Shown::Surface(red), Shown::Surface(blue)) = (r.shown(500), r.shown(1_500)) else { panic!("previews should be surfaces") };
    assert_eq!((objc2_core_video::CVPixelBufferGetWidth(&red), objc2_core_video::CVPixelBufferGetHeight(&red)), (320, 180));
    // BT.601 full range: red's luma ~76, blue's ~29
    let (yr, yb) = (luma(&red), luma(&blue));
    assert!((60..95).contains(&yr) && (15..45).contains(&yb), "red {yr}, blue {yb}");
    // the earlier frame is untouched by the later one (a buffer each)
    assert_eq!(luma(&red), yr);
    // off again: pixels
    r.set_zero_copy(false);
    r.set_preview(None);
    assert!(matches!(r.shown(500), Shown::Pixels(p) if p.len() == 640 * 360 * 4));
}
