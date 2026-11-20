//! macOS and iOS: frames a platform's decoder hands over in its own IOSurface-backed buffers
//! (`media::Picture::Buffer`) are drawn as they are, with no copy to the CPU: here a made-up
//! backend whose reader gives BGRA buffers of one colour, drawn by the sequence renderer and read
//! back. Its own test program: the backend is the process's.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lightair_render::media::{self, Backend, Contents, MediaError, Picture, Probe, VideoReader};
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Media, Sequence};
use lightair_render::texture::PixelBuffer;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_32BGRA,
    kCVReturnSuccess,
};

/// Buffers handed over (none of them read back as bytes).
static HANDED: AtomicU64 = AtomicU64::new(0);

/// An IOSurface-backed BGRA buffer of one colour (RGB given).
fn buffer(size: (u32, u32), rgb: [u8; 3]) -> PixelBuffer {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let keys: [&CFString; 2] = unsafe { [kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey] };
    let values: [&CFType; 2] = [empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    let rc = unsafe {
        CVPixelBufferCreate(
            None,
            size.0 as usize,
            size.1 as usize,
            kCVPixelFormatType_32BGRA,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    assert_eq!(rc, kCVReturnSuccess);
    let b: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
    unsafe { CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags::empty()) };
    let (base, stride) = (CVPixelBufferGetBaseAddress(&b) as *mut u8, CVPixelBufferGetBytesPerRow(&b));
    for y in 0..size.1 as usize {
        let row = unsafe { std::slice::from_raw_parts_mut(base.add(y * stride), size.0 as usize * 4) };
        for p in row.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags::empty()) };
    unsafe { PixelBuffer::from_raw(CFRetained::into_raw(b).as_ptr().cast()) }.expect("BGRA in an IOSurface")
}

struct Frames {
    size: (u32, u32),
    next: u64,
    step: f64,
    read: u64,
}

impl VideoReader for Frames {
    fn next_ms(&self) -> u64 {
        self.next + (self.read as f64 * self.step) as u64
    }

    fn next(&mut self, _: &mut Vec<u8>) -> bool {
        panic!("frames are handed over as buffers");
    }

    fn next_picture(&mut self) -> Option<Picture> {
        if self.next_ms() >= 2_000 {
            return None;
        }
        self.read += 1;
        HANDED.fetch_add(1, Ordering::Relaxed);
        Some(Picture::Buffer(buffer(self.size, [255, 128, 0])))
    }
}

/// `.orange`: a 2 s, 320×180 video, frames in buffers.
struct Orange;

impl Backend for Orange {
    fn contents(&self, _: &Path) -> Result<Contents, MediaError> {
        Ok(Contents { video: true, audio: false, still: false, width: 320, height: 180, duration_ms: 2_000 })
    }

    fn probe(&self, _: &Path) -> Result<Probe, MediaError> {
        Ok(Probe { width: 320, height: 180, fps: 30.0, duration_ms: 2_000 })
    }

    fn video(&self, _: &Path, start_ms: u64, fps: f64, size: (u32, u32)) -> Result<Box<dyn VideoReader>, MediaError> {
        Ok(Box::new(Frames { size, next: start_ms, step: 1000.0 / fps, read: 0 }))
    }

    fn audio(&self, path: &Path, _: Option<u32>, _: u32, _: u32) -> Result<Vec<f32>, MediaError> {
        Err(MediaError::Unreadable(path.into()))
    }
}

#[test]
fn a_decoders_buffers_are_drawn_as_they_are() {
    media::set_backend(Arc::new(Orange));
    let mut seq = Sequence::new((320, 180), 30);
    let id = seq.add_media(Media::from_file(Path::new("/nowhere/clip.orange")).unwrap());
    seq.append(id).unwrap();
    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) if std::env::var_os("LIGHTAIR_REQUIRE_GPU").is_none() => {
            eprintln!("no GPU here ({e}); skipping");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let at = |f: &[u8]| [f[(90 * 320 + 160) * 4], f[(90 * 320 + 160) * 4 + 1], f[(90 * 320 + 160) * 4 + 2]];
    // BGRA in the buffer, RGBA in the frame: orange either way
    assert_eq!(at(&r.frame(500)), [255, 128, 0]);
    assert_eq!(at(&r.frame(1_500)), [255, 128, 0]);
    assert!(HANDED.load(Ordering::Relaxed) >= 2);
    // and as a preview (BGRA frames)
    r.set_preview(Some((160, 90)));
    let f = r.frame(1_000);
    let p = (45 * 160 + 80) * 4;
    assert_eq!([f[p + 2], f[p + 1], f[p]], [255, 128, 0]);
}
