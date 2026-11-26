//! Apple's video encoder as a stream or a file gets its pictures: with B-frames no picture is
//! decoded after it's shown (RTMP has no negative composition times), and the plan's constant
//! quality (CRF) is followed instead of the bitrate. macOS only (VideoToolbox).
#![cfg(target_os = "macos")]
// the free CoreVideo functions read more plainly here than objc2's renamed methods
#![allow(deprecated)]

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use lightair_encode::VideoCodec;
use lightair_encode::apple::{EncodedVideo, VideoEncoder, VideoOutput, VideoSettings};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPoolCreatePixelBuffer,
    CVPixelBufferUnlockBaseAddress,
};

const W: u32 = 640;
const H: u32 = 360;
const FPS: u32 = 30;

/// Encodes `frames` frames of moving noise (hard to compress, so the rate control shows) and
/// gives the pictures that came out.
fn encode(settings: VideoSettings, frames: u32) -> Vec<EncodedVideo> {
    let got: Arc<Mutex<Vec<EncodedVideo>>> = Arc::default();
    let sink = got.clone();
    let encoder = VideoEncoder::new(
        settings,
        Box::new(move |out| {
            if let VideoOutput::Packet(p) = out {
                sink.lock().unwrap().push(p);
            }
        }),
    )
    .expect("a VideoToolbox session");
    let pool = encoder.pixel_buffer_pool().expect("the session's pool");
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for n in 0..frames {
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        let rc = unsafe { CVPixelBufferPoolCreatePixelBuffer(None, &pool, NonNull::from(&mut out)) };
        assert_eq!(rc, 0, "a buffer from the pool");
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
        unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
        let (stride, rows) = (CVPixelBufferGetBytesPerRow(&buffer), CVPixelBufferGetHeight(&buffer));
        for i in 0..stride * rows {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            unsafe { *base.add(i) = (seed >> 24) as u8 };
        }
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        let pts = n as i64 * 1_000_000_000 / FPS as i64;
        encoder.encode(&buffer, pts, n == 0).expect("the frame is taken");
    }
    encoder.flush();
    let out = got.lock().unwrap().clone();
    assert!(!out.is_empty(), "pictures came out");
    out
}

/// A recording's encoder makes B-frames; each picture's decode time is at or before its
/// presentation time, so a stream never carries a negative composition time.
#[test]
fn with_b_frames_no_picture_is_decoded_after_it_is_shown() {
    let pictures = encode(VideoSettings::recording(VideoCodec::H264, W, H, FPS, 2_000), 60);
    for p in &pictures {
        assert!(p.dts_ns <= p.pts_ns, "decoded at {} ns, shown at {} ns", p.dts_ns, p.pts_ns);
    }
    let dts: Vec<i64> = pictures.iter().map(|p| p.dts_ns).collect();
    assert!(dts.windows(2).all(|w| w[0] < w[1]), "decode times only go forward: {dts:?}");
}

/// The plan's CRF is constant quality: a hard picture gets the bits its quality needs, not the
/// planned average (a 640×360 track at quality 55 went out near 1.8 Mbps where CBR held 350).
#[test]
fn constant_quality_isnt_held_to_the_bitrate() {
    let bytes = |quality: Option<f32>| -> usize {
        let settings = VideoSettings { quality, ..VideoSettings::streaming(VideoCodec::H264, W, H, FPS, 300) };
        encode(settings, 60).iter().map(|p| p.data.len()).sum()
    };
    let (rate, quality) = (bytes(None), bytes(Some(0.8)));
    assert!(quality > rate * 2, "at the bitrate {rate} bytes, at constant quality {quality} bytes");
}
