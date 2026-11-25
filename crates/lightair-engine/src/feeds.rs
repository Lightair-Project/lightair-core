//! Feeds: the captures the engine asked the host for. A video
//! feed is a three-slot ring of frames; an audio feed is a lock-free ring of samples the mix
//! thread drains. Pushes come from the platform's capture threads.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;

/// How many frames a video feed keeps.
pub const SLOTS: usize = 3;

/// A frame as it was pushed.
pub enum Picture {
    /// Apple: an IOSurface-backed BGRA pixel buffer, drawn as it is.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Buffer(lightair_render::texture::PixelBuffer, crate::gpu::apple::Kept),
    /// YUV 4:2:0 planes, copied (luma, then chroma interleaved U,V), converted on the GPU.
    Yuv(Yuv),
    /// RGBA bytes, tightly packed, upright (Android's screen: MediaProjection's ImageReader).
    Rgba(Vec<u8>, (u32, u32)),
    /// Android's camera zero-copy: its `AHardwareBuffer`, as captured (size), turned and
    /// mirrored as `push_yuv`'s.
    #[cfg(target_os = "android")]
    Hardware { held: Arc<crate::gpu::android::Held>, size: (u32, u32), rotation: u32, mirrored: bool },
}

impl Picture {
    /// Its size as drawn: upright (a turned camera frame's sides swapped).
    pub fn upright_size(&self) -> (u32, u32) {
        let turn = |(w, h): (u32, u32), rotation: u32| {
            if rotation % 180 == 90 { (h, w) } else { (w, h) }
        };
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Picture::Buffer(b, _) => b.size(),
            Picture::Yuv(y) => turn((y.width, y.height), y.rotation),
            Picture::Rgba(_, size) => *size,
            #[cfg(target_os = "android")]
            Picture::Hardware { size, rotation, .. } => turn(*size, *rotation),
        }
    }
}

pub struct Yuv {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    /// (width/2 × height/2) U,V pairs.
    pub uv: Vec<u8>,
    pub rotation: u32,
    pub mirrored: bool,
    pub full_range: bool,
    /// 0 BT.601, 1 BT.709, 2 BT.2020.
    pub matrix: u32,
}

pub struct Frame {
    pub picture: Arc<Picture>,
    pub host_ns: i64,
    /// Counts pushes, so the renderer converts a YUV frame once.
    pub serial: u64,
}

pub struct VideoFeed {
    ring: Mutex<VecDeque<Frame>>,
    /// Spare plane buffers, so a push of the same size doesn't allocate.
    spare: Mutex<Vec<Vec<u8>>>,
    serial: AtomicU64,
    /// When the last frame arrived, on the host clock (a feed that stops sending is shown
    /// paused by this, whatever the frames' own times say).
    pub arrived_ns: AtomicI64,
    pub pushed: AtomicU64,
    /// The latest person mask the host computed for this camera (background effects).
    pub mask: Mutex<Option<Arc<Mask>>>,
    /// Masks wanted: the quality asked for (0 fast, 1 balanced, 2 accurate) and until when
    /// (host clock), set by the render thread each frame it draws a background effect.
    pub mask_quality: AtomicU32,
    pub mask_until_ns: AtomicI64,
    /// The faces the host found (for the looks), newest last, eight kept; asked for until
    /// `face_until_ns` while a face look draws this feed.
    pub faces: Mutex<VecDeque<Arc<FacePush>>>,
    pub face_until_ns: AtomicI64,
    /// The latest multiclass segmentation (R skin, G face skin, B hair, A person).
    pub segmentation: Mutex<Option<Arc<Segmentation>>>,
    /// What the looks last saw of this camera's faces (the Looks screen's status), set by the
    /// render thread each frame it draws a face look on this feed.
    pub looks_seen: Mutex<Option<LooksSeen>>,
}

/// What the looks saw of a camera's faces on the last frame they drew it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LooksSeen {
    /// When (host clock).
    pub at_ns: i64,
    /// Whether the host has pushed any face analysis for this feed yet.
    pub analysed: bool,
    /// A face is present now.
    pub present: bool,
    /// The newest face's box (x, y, w, h, 0–1 of the picture as drawn), while present.
    pub face_box: Option<[f32; 4]>,
}

impl FacePush {
    /// The box around its points, 0–1 (the host pushes points as fractions of the picture).
    pub fn face_box(&self) -> Option<[f32; 4]> {
        if !self.present || self.points.is_empty() {
            return None;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &self.points {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        let (x0, y0) = (x0.clamp(0.0, 1.0), y0.clamp(0.0, 1.0));
        let (x1, y1) = (x1.clamp(0.0, 1.0), y1.clamp(0.0, 1.0));
        (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
    }
}

/// MediaPipe's Face Landmarker points for one analysed frame, as the host pushed them: 478
/// (0–467 the mesh, 468–477 the irises), normalised 0–1 in the upright picture as drawn, y down,
/// raw (the engine smooths and snaps them, as the desktop's tracker does).
pub struct FacePush {
    /// The analysed frame's capture time, on the host clock.
    pub pts_ns: i64,
    pub present: bool,
    pub score: f32,
    pub points: Vec<[f32; 2]>,
    /// The analysis image's size in pixels (the smoothing's units come from it).
    pub size: (u32, u32),
    pub serial: u64,
}

/// `selfie_multiclass_256` for one analysed frame: RGBA, R body + face skin, G face skin, B hair,
/// A person, the whole picture upright, unsmoothed.
pub struct Segmentation {
    pub pts_ns: i64,
    pub rgba: Vec<u8>,
    pub size: (u32, u32),
    pub serial: u64,
}

/// Points in a face push (MediaPipe's Face Landmarker v2 with irises).
pub const FACE_POINTS: usize = 478;

/// A person mask: one byte a pixel (255 the person), as computed from the picture as pushed
/// (its orientation), turned and mirrored as that picture is.
pub struct Mask {
    pub bytes: Vec<u8>,
    pub size: (u32, u32),
    /// Quarter turns clockwise and mirroring, as the picture's (`push_yuv`'s); 0 and false for
    /// a pixel buffer drawn as it is.
    pub rotation: u32,
    pub mirrored: bool,
    pub serial: u64,
}

impl VideoFeed {
    /// The mask quality wanted now, if any.
    pub fn mask_wanted(&self, now_ns: i64) -> Option<u32> {
        (self.mask_until_ns.load(Ordering::Acquire) > now_ns).then(|| self.mask_quality.load(Ordering::Relaxed))
    }

    /// Asked for while a background effect draws this feed: for the next second.
    pub fn want_mask(&self, quality: u32, now_ns: i64) {
        self.mask_quality.store(quality.min(2), Ordering::Relaxed);
        self.mask_until_ns.store(now_ns + 1_000_000_000, Ordering::Release);
    }

    /// Whether faces are wanted now (a face look draws this feed).
    pub fn face_wanted(&self, now_ns: i64) -> bool {
        self.face_until_ns.load(Ordering::Acquire) > now_ns
    }

    /// Asked for while a face look draws this feed: for the next second.
    pub fn want_face(&self, now_ns: i64) {
        self.face_until_ns.store(now_ns + 1_000_000_000, Ordering::Release);
    }

    pub fn push_face(&self, pts_ns: i64, present: bool, score: f32, points: Vec<[f32; 2]>, size: (u32, u32)) {
        let mut faces = self.faces.lock();
        let serial = faces.back().map_or(0, |f| f.serial) + 1;
        faces.push_back(Arc::new(FacePush {
            pts_ns,
            present: present && points.len() == FACE_POINTS,
            score,
            points,
            size,
            serial,
        }));
        while faces.len() > 8 {
            faces.pop_front();
        }
    }

    pub fn push_segmentation(&self, pts_ns: i64, rgba: Vec<u8>, size: (u32, u32)) {
        let mut s = self.segmentation.lock();
        let serial = s.as_ref().map_or(0, |s| s.serial) + 1;
        *s = Some(Arc::new(Segmentation { pts_ns, rgba, size, serial }));
    }

    pub fn push_mask(&self, bytes: Vec<u8>, size: (u32, u32), rotation: u32, mirrored: bool) {
        let mut m = self.mask.lock();
        let serial = m.as_ref().map_or(0, |m| m.serial) + 1;
        *m = Some(Arc::new(Mask { bytes, size, rotation: rotation % 360 / 90, mirrored, serial }));
    }
}

impl Default for VideoFeed {
    fn default() -> Self {
        VideoFeed {
            ring: Mutex::new(VecDeque::with_capacity(SLOTS)),
            spare: Mutex::new(Vec::new()),
            serial: AtomicU64::new(0),
            arrived_ns: AtomicI64::new(i64::MIN),
            pushed: AtomicU64::new(0),
            mask: Mutex::new(None),
            mask_quality: AtomicU32::new(1),
            mask_until_ns: AtomicI64::new(i64::MIN),
            faces: Mutex::new(VecDeque::new()),
            face_until_ns: AtomicI64::new(i64::MIN),
            segmentation: Mutex::new(None),
            looks_seen: Mutex::new(None),
        }
    }
}

impl VideoFeed {
    pub fn push(&self, picture: Picture, host_ns: i64) {
        let serial = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        let old = {
            let mut ring = self.ring.lock();
            let old = if ring.len() >= SLOTS { ring.pop_front() } else { None };
            ring.push_back(Frame { picture: Arc::new(picture), host_ns, serial });
            old
        };
        // when it arrived (not when it was captured: a camera whose times lag isn't paused)
        self.arrived_ns.store(crate::clock::host_now_ns(), Ordering::Release);
        self.pushed.fetch_add(1, Ordering::Relaxed);
        // released outside the lock (a CVPixelBuffer goes back to its pool here)
        if let Some(f) = old {
            self.recycle(f);
        }
    }

    fn recycle(&self, f: Frame) {
        if let Ok(Picture::Yuv(y)) = Arc::try_unwrap(f.picture) {
            let mut spare = self.spare.lock();
            if spare.len() < 4 {
                spare.push(y.y);
                spare.push(y.uv);
            }
        }
    }

    /// A buffer of `len` bytes, reused where one is spare.
    pub fn buffer(&self, len: usize) -> Vec<u8> {
        let mut b = self.spare.lock().pop().unwrap_or_default();
        b.clear();
        b.resize(len, 0);
        b
    }

    /// The newest frame pushed at or before `host_ns` (the newest of all when every one is
    /// later: a frame from a feed that runs ahead is better than none).
    pub fn at(&self, host_ns: i64) -> Option<(Arc<Picture>, u64, i64)> {
        let ring = self.ring.lock();
        let f = ring.iter().rev().find(|f| f.host_ns <= host_ns).or_else(|| ring.front())?;
        Some((f.picture.clone(), f.serial, f.host_ns))
    }

    pub fn clear(&self) {
        let frames: Vec<Frame> = self.ring.lock().drain(..).collect();
        drop(frames);
    }
}

/// Copies YUV 4:2:0 planes into a frame (Android's `Image`, any chroma layout).
///
/// # Safety
/// The pointers must be valid for the sizes their strides and the frame's size imply.
#[allow(clippy::too_many_arguments)]
pub unsafe fn copy_yuv(
    feed: &VideoFeed,
    y: *const u8,
    y_stride: usize,
    u: *const u8,
    v: *const u8,
    uv_stride: usize,
    uv_pixel_stride: usize,
    width: u32,
    height: u32,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if y.is_null() || u.is_null() || v.is_null() || width < 2 || height < 2 {
        return None;
    }
    let (w, h) = (width as usize & !1, height as usize & !1);
    let mut ly = feed.buffer(w * h);
    for row in 0..h {
        let src = unsafe { std::slice::from_raw_parts(y.add(row * y_stride), w) };
        ly[row * w..row * w + w].copy_from_slice(src);
    }
    let (cw, ch) = (w / 2, h / 2);
    let mut uv = feed.buffer(cw * ch * 2);
    let ps = uv_pixel_stride.max(1);
    let interleaved = ps == 2 && (v as usize) == (u as usize) + 1;
    for row in 0..ch {
        let out = &mut uv[row * cw * 2..(row + 1) * cw * 2];
        if interleaved {
            let src = unsafe { std::slice::from_raw_parts(u.add(row * uv_stride), cw * 2 - 1) };
            out[..cw * 2 - 1].copy_from_slice(src);
            out[cw * 2 - 1] = unsafe { *v.add(row * uv_stride + (cw - 1) * 2) };
        } else {
            for x in 0..cw {
                out[x * 2] = unsafe { *u.add(row * uv_stride + x * ps) };
                out[x * 2 + 1] = unsafe { *v.add(row * uv_stride + x * ps) };
            }
        }
    }
    Some((ly, uv))
}

/// An audio feed: samples in a lock-free ring, at the rate and channels last pushed.
pub struct AudioFeed {
    producer: Mutex<rtrb::Producer<f32>>,
    pub consumer: Mutex<rtrb::Consumer<f32>>,
    pub rate: AtomicU32,
    pub channels: AtomicU32,
    /// Host time just after the last pushed frame (when the next one will be heard).
    pub end_ns: AtomicI64,
    pub dropped: AtomicU64,
    pub pushed: AtomicU64,
}

impl Default for AudioFeed {
    fn default() -> Self {
        // 400 ms of stereo at 48 kHz
        let (producer, consumer) = rtrb::RingBuffer::new(48_000 * 2 * 4 / 10);
        AudioFeed {
            producer: Mutex::new(producer),
            consumer: Mutex::new(consumer),
            rate: AtomicU32::new(48_000),
            channels: AtomicU32::new(1),
            end_ns: AtomicI64::new(i64::MIN),
            dropped: AtomicU64::new(0),
            pushed: AtomicU64::new(0),
        }
    }
}

impl AudioFeed {
    /// Real-time safe: never blocks (a second pusher at the same moment loses its block).
    pub fn push(&self, samples: &[f32], channels: u32, rate: u32, host_ns: i64) -> bool {
        let Some(mut p) = self.producer.try_lock() else {
            return false;
        };
        if p.slots() < samples.len() {
            self.dropped.fetch_add(samples.len() as u64, Ordering::Relaxed);
            return false;
        }
        let Ok(mut chunk) = p.write_chunk_uninit(samples.len()) else {
            return false;
        };
        let (a, b) = chunk.as_mut_slices();
        let n = a.len();
        for (d, s) in a.iter_mut().zip(samples) {
            d.write(*s);
        }
        for (d, s) in b.iter_mut().zip(&samples[n..]) {
            d.write(*s);
        }
        unsafe { chunk.commit_all() };
        let channels = channels.max(1);
        let frames = samples.len() as u64 / channels as u64;
        self.rate.store(rate.max(1), Ordering::Relaxed);
        self.channels.store(channels, Ordering::Relaxed);
        self.end_ns.store(host_ns + (frames as i64 * 1_000_000_000 / rate.max(1) as i64), Ordering::Release);
        self.pushed.fetch_add(frames, Ordering::Relaxed);
        true
    }
}

pub enum Feed {
    Video(VideoFeed),
    Audio(AudioFeed),
}

impl Feed {
    pub fn video(&self) -> Option<&VideoFeed> {
        match self {
            Feed::Video(v) => Some(v),
            Feed::Audio(_) => None,
        }
    }
    pub fn audio(&self) -> Option<&AudioFeed> {
        match self {
            Feed::Audio(a) => Some(a),
            Feed::Video(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_whose_times_lag_is_still_arriving() {
        let feed = VideoFeed::default();
        let now = crate::clock::host_now_ns();
        // the emulator's camera: frames stamped 800 ms ago, arriving steadily
        let (y, uv) = (vec![0u8; 4], vec![128u8; 2]);
        let frame = Yuv { width: 2, height: 2, y, uv, rotation: 0, mirrored: false, full_range: true, matrix: 0 };
        feed.push(Picture::Yuv(frame), now - 800_000_000);
        let arrived = feed.arrived_ns.load(Ordering::Acquire);
        assert!((arrived - now).abs() < 100_000_000, "arrival, not capture: {}", (arrived - now) / 1_000_000);
    }

    #[test]
    fn planes_are_copied_whatever_the_chroma_layout() {
        let feed = VideoFeed::default();
        // 4×2, luma rows padded to 8; chroma 2×1
        let y: Vec<u8> = (0..16).collect();
        // NV12: U,V interleaved in one buffer, v = u + 1
        let nv12 = [100u8, 200, 101, 201];
        let (ly, uv) =
            unsafe { copy_yuv(&feed, y.as_ptr(), 8, nv12.as_ptr(), nv12.as_ptr().add(1), 4, 2, 4, 2) }.unwrap();
        assert_eq!(ly, [0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(uv, [100, 200, 101, 201]);
        // planar (I420)
        let (u, v) = ([10u8, 11], [20u8, 21]);
        let (_, uv) = unsafe { copy_yuv(&feed, y.as_ptr(), 8, u.as_ptr(), v.as_ptr(), 2, 1, 4, 2) }.unwrap();
        assert_eq!(uv, [10, 20, 11, 21]);
        // NV21: V first
        let nv21 = [200u8, 100, 201, 101];
        let (_, uv) =
            unsafe { copy_yuv(&feed, y.as_ptr(), 8, nv21.as_ptr().add(1), nv21.as_ptr(), 4, 2, 4, 2) }.unwrap();
        assert_eq!(uv, [100, 200, 101, 201]);
    }
}
