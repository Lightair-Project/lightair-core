//! The hot paths behind the C ABI and JNI, in safe(ish) Rust:
//! `lightair-live-core` exports thin wrappers around these. Every call looks the session up by
//! handle and does nothing once it's closed.

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::feeds::{Feed, Picture, Yuv, copy_yuv};
use crate::hot::lookup;

/// A push's time, on the host clock: one more than 2 s from now is on another clock (a host
/// that stamped with `System.nanoTime()` or wall time), which would leave the program waiting
/// for it forever or drawing nothing: it's taken as now instead, and said once.
fn on_host_clock(host_ns: i64, what: &str) -> i64 {
    within(host_ns, 2_000_000_000, what)
}

/// An analysed frame's capture time (a face, a segmentation), on the host clock: the models run
/// late on a slow device (the Android emulator's segmentation: 2.1 s), so up to 10 s in the past
/// is still the frame's own time, which the looks match frames by; at most 2 s ahead, as
/// [`on_host_clock`].
fn analysed_on_host_clock(pts_ns: i64, what: &str) -> i64 {
    within(pts_ns, 10_000_000_000, what)
}

/// `host_ns` if it's from `past_ns` before now to 2 s after, else now (said once).
fn within(host_ns: i64, past_ns: u64, what: &str) -> i64 {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let now = crate::clock::host_now_ns();
    let ok = if host_ns <= now { now.abs_diff(host_ns) <= past_ns } else { host_ns.abs_diff(now) <= 2_000_000_000 };
    if ok {
        return host_ns;
    }
    if !SAID.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            what,
            off_ms = host_ns.saturating_sub(now) / 1_000_000,
            "pushed times aren't on the host clock (mach_absolute_time / CLOCK_BOOTTIME): taken as when they arrive"
        );
    }
    now
}

/// Runs an exported call (C ABI, JNI): a panic in it, a bug whatever the host passed, is said
/// once and `fallback` returned, rather than unwinding into Swift or Kotlin, which aborts the
/// app (a panic can't leave an `extern "C"` function).
pub fn guard<T>(what: &'static str, fallback: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(e) => {
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                tracing::error!(what, why = %crate::hot::panic_text(&e), "a call from the app panicked");
            }
            fallback
        }
    }
}

/// `rows` rows of `row` bytes, `stride` bytes apart: the bytes to copy, if `stride` holds a row
/// and the size fits in memory (a host's sizes, so never multiplied unchecked).
fn rows(row: usize, rows: u32, stride: u32) -> Option<usize> {
    if row == 0 || rows == 0 || (stride as usize) < row {
        return None;
    }
    row.checked_mul(rows as usize)
}

/// iOS: a BGRA IOSurface `CVPixelBufferRef` whose reference the caller gives up.
///
/// # Safety
/// `buffer` must be a `CVPixelBufferRef` the caller owns one reference to (or null).
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub unsafe fn push_pixel_buffer(studio: u64, feed: u32, buffer: *mut c_void, host_ns: i64) -> bool {
    // the engine's own reference first (a background effect samples it), then the caller's
    // taken over, so it's released on every path
    let kept = unsafe { crate::gpu::apple::Kept::retain(buffer) };
    let Some(b) = (unsafe { lightair_render::texture::PixelBuffer::from_raw(buffer) }) else {
        return false;
    };
    let Some(kept) = kept else { return false };
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(v) = f.video() else { return false };
    v.push(Picture::Buffer(b, kept), on_host_clock(host_ns, "a picture"));
    true
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
/// # Safety
/// Never dereferences `buffer` here (no pixel buffers off Apple platforms).
pub unsafe fn push_pixel_buffer(_studio: u64, _feed: u32, _buffer: *mut c_void, _host_ns: i64) -> bool {
    false
}

/// # Safety
/// The plane pointers must be valid for the sizes the strides and the frame's size imply.
#[allow(clippy::too_many_arguments)]
pub unsafe fn push_yuv(
    studio: u64,
    feed: u32,
    y: *const u8,
    y_stride: u32,
    u: *const u8,
    v: *const u8,
    uv_stride: u32,
    uv_pixel_stride: u32,
    width: u32,
    height: u32,
    rotation: u32,
    mirrored: bool,
    full_range: bool,
    matrix: u32,
    host_ns: i64,
) -> bool {
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(vf) = f.video() else { return false };
    let planes = unsafe {
        copy_yuv(vf, y, y_stride as usize, u, v, uv_stride as usize, uv_pixel_stride as usize, width, height)
    };
    let Some((ly, uv)) = planes else { return false };
    let frame = Yuv {
        width: width & !1,
        height: height & !1,
        y: ly,
        uv,
        rotation: rotation % 360 / 90 * 90,
        mirrored,
        full_range,
        matrix,
    };
    vf.push(Picture::Yuv(frame), on_host_clock(host_ns, "a picture"));
    true
}

/// Whether a camera feed wants faces now (a face look draws it): a host asks before running the
/// face and segmentation models on a frame.
pub fn face_wanted(studio: u64, feed: u32) -> bool {
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    f.video().is_some_and(|v| v.face_wanted(crate::clock::host_now_ns()))
}

/// A face for a camera feed: `count` points (478, MediaPipe's indexing) as x, y pairs in
/// `points`, normalised 0–1 in the upright picture as drawn (after its rotation and
/// mirroring), y down, raw; `present` false (and `count` 0) when no face was found. `pts_ns`:
/// the analysed frame's capture time on the host clock; `width`, `height`: the analysis image.
/// Copied.
///
/// # Safety
/// `points` must be valid for `count × 2` floats (or null with `count` 0).
#[allow(clippy::too_many_arguments)]
pub unsafe fn push_face(
    studio: u64,
    feed: u32,
    pts_ns: i64,
    present: bool,
    score: f32,
    points: *const f32,
    count: u32,
    width: u32,
    height: u32,
) -> bool {
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(v) = f.video() else { return false };
    let pts = if points.is_null() || count == 0 {
        Vec::new()
    } else {
        let raw = unsafe { std::slice::from_raw_parts(points, count as usize * 2) };
        raw.as_chunks::<2>().0.to_vec()
    };
    if present && pts.len() != crate::feeds::FACE_POINTS {
        return false;
    }
    v.push_face(analysed_on_host_clock(pts_ns, "a face"), present, score, pts, (width, height));
    true
}

/// The multiclass segmentation for a camera feed: `width`×`height` RGBA (R body and face skin,
/// G face skin, B hair, A person), `stride` bytes a row, the whole picture upright as drawn.
/// Copied.
///
/// # Safety
/// `data` must be valid for `stride × height` bytes.
pub unsafe fn push_segmentation(
    studio: u64,
    feed: u32,
    pts_ns: i64,
    data: *const u8,
    width: u32,
    height: u32,
    stride: u32,
) -> bool {
    let row = (width as usize).saturating_mul(4);
    let Some(total) = rows(row, height, stride) else {
        return false;
    };
    if data.is_null() {
        return false;
    }
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(v) = f.video() else { return false };
    let mut rgba = Vec::with_capacity(total);
    for y in 0..height as usize {
        let src = unsafe { std::slice::from_raw_parts(data.add(y * stride as usize), row) };
        rgba.extend_from_slice(src);
    }
    v.push_segmentation(analysed_on_host_clock(pts_ns, "a segmentation"), rgba, (width, height));
    true
}

/// The person mask quality a camera feed wants now (0 fast, 1 balanced, 2 accurate: Vision's
/// levels), or -1 for none: a host asks before segmenting a frame.
pub fn mask_wanted(studio: u64, feed: u32) -> i32 {
    let Some(hot) = lookup(studio) else { return -1 };
    let Some(f) = hot.feed(feed) else { return -1 };
    let Some(v) = f.video() else { return -1 };
    v.mask_wanted(crate::clock::host_now_ns()).map_or(-1, |q| q as i32)
}

/// A person mask for a camera feed: `width`×`height` bytes a pixel (255 the person) with
/// `stride` bytes a row, computed from the frame as pushed, so turned (`rotation`, clockwise
/// degrees) and mirrored as that frame is (iOS pixel buffers: 0, false). Copied.
///
/// # Safety
/// `data` must be valid for `stride × height` bytes.
#[allow(clippy::too_many_arguments)]
pub unsafe fn push_mask(
    studio: u64,
    feed: u32,
    data: *const u8,
    width: u32,
    height: u32,
    stride: u32,
    rotation: u32,
    mirrored: bool,
) -> bool {
    let Some(total) = rows(width as usize, height, stride) else {
        return false;
    };
    if data.is_null() {
        return false;
    }
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(v) = f.video() else { return false };
    let mut bytes = Vec::with_capacity(total);
    for y in 0..height as usize {
        let src = unsafe { std::slice::from_raw_parts(data.add(y * stride as usize), width as usize) };
        bytes.extend_from_slice(src);
    }
    v.push_mask(bytes, (width, height), rotation, mirrored);
    true
}

/// RGBA bytes for a feed (Android's screen): `width`×`height`, `stride` bytes a row,
/// upright. Copied.
///
/// # Safety
/// `data` must be valid for `stride × height` bytes.
pub unsafe fn push_rgba(
    studio: u64,
    feed: u32,
    data: *const u8,
    width: u32,
    height: u32,
    stride: u32,
    host_ns: i64,
) -> bool {
    let row = (width as usize).saturating_mul(4);
    let Some(total) = rows(row, height, stride) else {
        return false;
    };
    if data.is_null() {
        return false;
    }
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(v) = f.video() else { return false };
    let mut bytes = Vec::with_capacity(total);
    for y in 0..height as usize {
        bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(data.add(y * stride as usize), row) });
    }
    v.push(Picture::Rgba(bytes, (width, height)), on_host_clock(host_ns, "a picture"));
    true
}

/// Whether camera frames should come as hardware buffers (Android: `push_hardware_buffer`)
/// rather than planes (`push_yuv`).
pub fn zero_copy() -> bool {
    crate::gpu::zero_copy()
}

/// Android: a camera frame's `AHardwareBuffer` (an `Image`'s, its `token` given back by
/// `released_frames` once nothing reads it: close the `Image` then), turned `rotation` clockwise
/// degrees and `mirrored` as `push_yuv`'s. Acquired here.
///
/// # Safety
/// `buffer` must be a valid `AHardwareBuffer*`.
#[cfg(target_os = "android")]
pub unsafe fn push_hardware_buffer(
    studio: u64,
    feed: u32,
    buffer: *mut ndk_sys::AHardwareBuffer,
    rotation: u32,
    mirrored: bool,
    host_ns: i64,
    token: i64,
) -> bool {
    let Some(hot) = lookup(studio) else {
        return false;
    };
    if buffer.is_null() {
        return false;
    }
    unsafe { ndk_sys::AHardwareBuffer_acquire(buffer) };
    let held = std::sync::Arc::new(crate::gpu::android::Held { buffer, token, released: hot.released.clone() });
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Some(v) = f.video() else { return false };
    let mut desc = ndk_sys::AHardwareBuffer_Desc {
        width: 0,
        height: 0,
        layers: 0,
        format: 0,
        usage: 0,
        stride: 0,
        rfu0: 0,
        rfu1: 0,
    };
    unsafe { ndk_sys::AHardwareBuffer_describe(buffer, &mut desc) };
    v.push(
        Picture::Hardware { held, size: (desc.width, desc.height), rotation: rotation % 360 / 90 * 90, mirrored },
        on_host_clock(host_ns, "a picture"),
    );
    true
}

/// The tokens of camera buffers nothing reads any more (Android): close their `Image`s.
pub fn released_frames(studio: u64) -> Vec<i64> {
    lookup(studio).map_or(Vec::new(), |h| std::mem::take(&mut *h.released.lock()))
}

/// Real-time safe.
pub fn push_audio(studio: u64, feed: u32, samples: &[f32], channels: u32, rate: u32, host_ns: i64) -> bool {
    let Some(hot) = lookup(studio) else {
        return false;
    };
    let Some(f) = hot.feed(feed) else {
        return false;
    };
    let Feed::Audio(a) = &*f else { return false };
    let ok = a.push(samples, channels, rate, on_host_clock(host_ns, "sound"));
    if hot.master.load(Ordering::Relaxed) == feed {
        hot.wake_mixer();
    }
    ok
}

/// One pulled frame: (1, buffer, pts, keyframe), (0, …) on timeout, (-1, …) once closed.
pub fn next_video_frame(studio: u64, encoder: u32, timeout_ms: u32) -> (i32, usize, i64, bool) {
    let Some(hot) = lookup(studio) else {
        return (-1, 0, 0, false);
    };
    let e = match hot.video_encoder(encoder) {
        Some(e) => e,
        // being opened: the host pulled before its open call returned; it's not closed
        None if hot.opening.lock().contains(&encoder) => {
            std::thread::sleep(Duration::from_millis(timeout_ms.min(20) as u64));
            return (0, 0, 0, false);
        }
        // registered between the two looks (an encoder is registered before it leaves
        // `opening`), else gone
        None => match hot.video_encoder(encoder) {
            Some(e) => e,
            None => return (-1, 0, 0, false),
        },
    };
    match e.next(Duration::from_millis(timeout_ms as u64)) {
        Ok(Some(d)) => (1, d.buffer, d.pts_ns, d.keyframe),
        Ok(None) => (0, 0, 0, false),
        Err(_) => (-1, 0, 0, false),
    }
}

/// Program sound into `out` (stereo interleaved): (frames, pts), (0, 0) on timeout, (-1, 0)
/// once closed.
pub fn read_program_audio(studio: u64, encoder: u32, out: &mut [f32], timeout_ms: u32) -> (i32, i64) {
    let Some(hot) = lookup(studio) else {
        return (-1, 0);
    };
    let Some(e) = hot.audio_encoder(encoder) else {
        return (-1, 0);
    };
    match e.read(out, Duration::from_millis(timeout_ms as u64)) {
        Ok(Some((n, pts))) => (n as i32, pts),
        Ok(None) => (0, 0),
        Err(_) => (-1, 0),
    }
}

pub fn parameter_sets(studio: u64, encoder: u32, nals: Vec<Vec<u8>>) {
    let Some(hot) = lookup(studio) else { return };
    // kept first, so a destination registered a moment later starts from them (go_live)
    if let Some(e) = hot.video_encoder(encoder) {
        e.sent.lock().parameter_sets = nals.clone();
    }
    for o in hot.outputs.read().iter().filter(|o| o.video_encoder() == encoder) {
        o.parameter_sets(&nals);
    }
}

/// The program time to mux a packet at (-1: drop it).
pub fn video_packet(
    studio: u64,
    encoder: u32,
    data: &[u8],
    length_prefixed: bool,
    encoder_pts_ns: i64,
    keyframe: bool,
) -> i64 {
    let Some(hot) = lookup(studio) else { return -1 };
    let Some(e) = hot.video_encoder(encoder) else {
        return -1;
    };
    let pts = if cfg!(any(target_os = "macos", target_os = "ios"))
        || !matches!(e.target, crate::host::EncoderTarget::NativeWindow(_))
    {
        encoder_pts_ns
    } else {
        e.program_pts(encoder_pts_ns)
    };
    if pts >= 0 {
        e.packet(data.len(), pts, keyframe);
        if keyframe {
            let mut h = e.held.lock();
            if h.catching && pts >= h.from_ns {
                h.packet = Some((data.to_vec(), length_prefixed));
                h.catching = false;
            }
        }
        // a stream encode's last minute, for clips
        if hot.outputs.read().iter().any(|o| o.video_encoder() == encoder) {
            e.clip.push(crate::clips::Packet { pts_ns: pts, keyframe, data: data.to_vec(), length_prefixed });
        }
        // the stream's destinations
        for o in hot.outputs.read().iter().filter(|o| o.video_encoder() == encoder) {
            o.video(data, length_prefixed, pts, keyframe);
        }
    }
    pts
}

pub fn audio_packet(studio: u64, encoder: u32, data: &[u8], pts_ns: i64) {
    let Some(hot) = lookup(studio) else { return };
    for o in hot.outputs.read().iter().filter(|o| o.audio_encoder == encoder) {
        o.audio(data, pts_ns);
    }
    if let Some(e) = hot.audio_encoder(encoder) {
        if hot.outputs.read().iter().any(|o| o.audio_encoder == encoder) {
            e.clip.push(crate::clips::Packet { pts_ns, keyframe: true, data: data.to_vec(), length_prefixed: false });
        }
        let mut s = e.sent.lock();
        s.packets += 1;
        s.bytes += data.len() as u64;
        s.last_pts_ns = pts_ns;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_on_no_clock_at_all_are_taken_as_now() {
        let now = crate::clock::host_now_ns();
        for t in [i64::MIN, i64::MAX, -1] {
            let got = on_host_clock(t, "a test");
            assert!((got - now).abs() < 1_000_000_000, "{t}: {got}");
        }
        assert_eq!(on_host_clock(now, "a test"), now);
    }

    /// A face model that ran 3 s late (a slow phone, the emulator): the face keeps its frame's
    /// capture time, which the looks match frames by; frames and sound still take ±2 s.
    #[test]
    fn a_late_face_keeps_its_frames_time() {
        use lightair_model::{Id, Workspace};
        let program = crate::hot::Program {
            workspace: std::sync::Arc::new(Workspace::new("Test")),
            folder: std::path::PathBuf::new(),
            scene: Id::new(),
            canvas: Id::new(),
            switch: None,
            privacy_scene: None,
            feeds: Default::default(),
            on_air_ns: 0,
            utc_offset_min: 0,
            frozen: Vec::new(),
        };
        let hot = std::sync::Arc::new(crate::hot::Hot::new(program, 0));
        hot.feeds.write().insert(1, std::sync::Arc::new(Feed::Video(crate::feeds::VideoFeed::default())));
        let studio = crate::hot::register(hot.clone());
        let now = crate::clock::host_now_ns();
        let points = vec![0.5f32; crate::feeds::FACE_POINTS * 2];
        let late = now - 3_000_000_000;
        let pushed = unsafe {
            push_face(studio, 1, late, true, 0.9, points.as_ptr(), crate::feeds::FACE_POINTS as u32, 256, 256)
        };
        assert!(pushed);
        let f = hot.feed(1).unwrap();
        let face = f.video().unwrap().faces.lock().back().cloned().unwrap();
        assert_eq!(face.pts_ns, late, "its frame's time, not when it arrived");
        let seg = [0u8; 16];
        assert!(unsafe { push_segmentation(studio, 1, late, seg.as_ptr(), 2, 2, 8) });
        let s = f.video().unwrap().segmentation.lock().clone().unwrap();
        assert_eq!(s.pts_ns, late);
        crate::hot::unregister(studio);
        // past 10 s, or ahead more than 2 s: another clock
        assert!(analysed_on_host_clock(now - 11_000_000_000, "a test") >= now);
        assert!(analysed_on_host_clock(now + 3_000_000_000, "a test") < now + 1_000_000_000);
        // pictures and sound: 2 s
        assert!(on_host_clock(now - 3_000_000_000, "a test") >= now);
    }

    #[test]
    fn a_hosts_sizes_are_checked_not_multiplied_blindly() {
        assert_eq!(rows(4, 2, 4), Some(8));
        assert_eq!(rows(4, 2, 3), None, "a stride shorter than a row");
        assert_eq!(rows(0, 2, 4), None);
        assert_eq!(rows(4, 0, 4), None);
        assert_eq!(rows(usize::MAX, 2, u32::MAX), None);
        let data = [0u8; 16];
        // once `width * 4` in u32: an overflow (a panic, or a huge read in release)
        assert!(!unsafe { push_rgba(0, 1, data.as_ptr(), 0x4000_0001, 1, 4, 0) });
        assert!(!unsafe { push_segmentation(0, 1, 0, data.as_ptr(), 0x4000_0001, 1, 4) });
    }
}
