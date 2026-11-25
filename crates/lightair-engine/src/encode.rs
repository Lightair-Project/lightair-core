//! Encoders' sides in Rust: the frames waiting for an iOS encoder to
//! pull, the program sound waiting for an audio encoder, Android's present-time → program-time
//! map, and what came back.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::host::{EncoderTarget, VideoEncoderConfig};

/// The encoder was closed: no more frames or sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// A frame drawn for an encoder, waiting to be pulled.
pub struct Drawn {
    /// iOS: a `CVPixelBufferRef` with one reference for the puller. Tests: 0.
    pub buffer: usize,
    pub pts_ns: i64,
    pub keyframe: bool,
    /// Tests: the frame's pixels.
    pub rgba: Option<Vec<u8>>,
}

// the buffer is only a pointer carried to the puller
unsafe impl Send for Drawn {}

/// What the video encoder sent back, for stats and the outputs.
#[derive(Default, Debug, Clone)]
pub struct Sent {
    pub packets: u64,
    pub bytes: u64,
    pub keyframes: u64,
    pub last_pts_ns: i64,
    pub parameter_sets: Vec<Vec<u8>>,
}

pub struct VideoEncoder {
    pub id: u32,
    pub config: VideoEncoderConfig,
    pub target: EncoderTarget,
    /// It encodes the camera alone (the "camera clean" recording), not the program.
    pub clean: bool,
    /// The format it encodes (a canvas); `None`: the active one.
    pub format: Option<lightair_model::Id>,
    queue: Mutex<VecDeque<Drawn>>,
    ready: Condvar,
    closed: AtomicBool,
    /// Android: (monotonic present time, program pts) of recent presents.
    presents: Mutex<VecDeque<(i64, i64)>>,
    /// Android: how long after a present the encoder stamps its buffer (learned).
    lag_ns: std::sync::atomic::AtomicI64,
    pub sent: Mutex<Sent>,
    pub keyframe_wanted: AtomicBool,
    /// The stream time of the last keyframe the encoder sent back, and of the last frame that
    /// asked for one (keyframes by time, whatever the frame rate).
    last_key_ns: std::sync::atomic::AtomicI64,
    key_asked_ns: std::sync::atomic::AtomicI64,
    pub dropped: AtomicU64,
    /// The picture a stream keeps sending while iOS has the app in the background.
    pub held: Mutex<Held>,
    /// A stream encode's last minute, for clips.
    pub clip: crate::clips::Ring,
}

/// The first keyframe from `from_ns` on, caught while `catching`: (data, length-prefixed).
#[derive(Default)]
pub struct Held {
    pub catching: bool,
    pub from_ns: i64,
    pub packet: Option<(Vec<u8>, bool)>,
}

/// How many drawn frames wait at most; older ones are dropped (the encoder is behind).
const QUEUE: usize = 3;

impl VideoEncoder {
    pub fn new(id: u32, config: VideoEncoderConfig, target: EncoderTarget) -> VideoEncoder {
        VideoEncoder {
            id,
            config,
            target,
            clean: false,
            format: None,
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
            presents: Mutex::new(VecDeque::new()),
            lag_ns: std::sync::atomic::AtomicI64::new(i64::MIN),
            sent: Mutex::new(Sent::default()),
            keyframe_wanted: AtomicBool::new(true),
            last_key_ns: std::sync::atomic::AtomicI64::new(i64::MIN),
            key_asked_ns: std::sync::atomic::AtomicI64::new(i64::MIN),
            dropped: AtomicU64::new(0),
            held: Mutex::new(Held::default()),
            clip: Default::default(),
        }
    }

    /// For one format, not the active one.
    pub fn in_format(mut self, format: lightair_model::Id) -> VideoEncoder {
        self.format = Some(format);
        self
    }

    /// For the camera alone, not the program.
    pub fn camera_clean(mut self) -> VideoEncoder {
        self.clean = true;
        self
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// No more frames: pullers still get those drawn (the recording's last), then -1.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_all();
    }

    pub fn queue(&self, mut d: Drawn) {
        if self.is_closed() {
            release_buffer(d.buffer);
            return;
        }
        d.keyframe |= self.keyframe_wanted.swap(false, Ordering::AcqRel);
        // a keyframe every `keyframe_interval_s` of stream time, whatever the frame rate: the
        // host encoders count their interval in frames at the configured rate, so a slow
        // device (an emulator at 5 fps) sent one every 10 s, and platforms want one every 2 s
        let interval = self.config.keyframe_interval_s.max(1) as i64 * 1_000_000_000;
        let last_key = self.last_key_ns.load(Ordering::Acquire);
        let since = last_key.max(self.key_asked_ns.load(Ordering::Acquire));
        if last_key != i64::MIN && d.pts_ns.saturating_sub(since) >= interval {
            d.keyframe = true;
        }
        if d.keyframe {
            self.key_asked_ns.store(d.pts_ns, Ordering::Release);
        }
        let dropped = {
            let mut q = self.queue.lock();
            let dropped = if q.len() >= QUEUE {
                q.pop_front()
            } else {
                None
            };
            q.push_back(d);
            dropped
        };
        if let Some(old) = dropped {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            // the dropped frame might have carried the keyframe request
            if old.keyframe {
                self.keyframe_wanted.store(true, Ordering::Release);
            }
            release_buffer(old.buffer);
        }
        self.ready.notify_one();
    }

    /// The next drawn frame: `Ok(Some)`, `Ok(None)` on timeout, `Err(Closed)` once closed.
    pub fn next(&self, timeout: Duration) -> Result<Option<Drawn>, Closed> {
        let deadline = Instant::now() + timeout;
        let mut q = self.queue.lock();
        loop {
            if let Some(d) = q.pop_front() {
                return Ok(Some(d));
            }
            if self.is_closed() {
                return Err(Closed);
            }
            if self.ready.wait_until(&mut q, deadline).timed_out() {
                return if self.is_closed() {
                    Err(Closed)
                } else {
                    Ok(None)
                };
            }
        }
    }

    /// Android: a frame with program time `pts_ns` was presented into the encoder's surface at
    /// `mono_ns` (CLOCK_MONOTONIC).
    pub fn presented(&self, mono_ns: i64, pts_ns: i64) {
        let mut p = self.presents.lock();
        if p.len() >= 64 {
            p.pop_front();
        }
        p.push_back((mono_ns, pts_ns));
    }

    /// The program time of the frame the encoder stamped `encoder_ns`. The stamp is when the
    /// swapchain queued the buffer, which trails the present by a steady lag (often more than
    /// half a frame), so the lag is learned and each packet takes the unused present nearest
    /// its stamp less the lag; failing one within a frame and a half, the oldest unused one
    /// (no B-frames: frames come out in the order they went in). It and every older present
    /// are used up (those the encoder dropped). -1 only before any present: a recording never
    /// loses a frame, above all its first keyframe, without which nothing before the next one
    /// decodes.
    pub fn program_pts(&self, encoder_ns: i64) -> i64 {
        let period = 1_000_000_000 / self.config.fps.max(1) as i64;
        let mut p = self.presents.lock();
        if p.is_empty() {
            return -1;
        }
        let lag = self.lag_ns.load(Ordering::Relaxed);
        // the first packet: the latest present at or before its stamp (presents the encoder
        // never took, made before it started, are older); the lag is learned from it
        let known = lag != i64::MIN;
        let target = encoder_ns - lag.max(0);
        let near = known
            .then(|| {
                p.iter()
                    .enumerate()
                    .min_by_key(|(_, (m, _))| (m - target).abs())
                    .filter(|(_, (m, _))| (m - target).abs() <= period * 3 / 2)
                    .map(|(i, _)| i)
            })
            .flatten();
        let first = (!known)
            .then(|| p.iter().rposition(|(m, _)| *m <= encoder_ns))
            .flatten();
        let i = near.or(first).unwrap_or(0);
        let (m, pts) = p[i];
        p.drain(..=i);
        // the lag, slowly (a stamp off by more than a frame and a half isn't counted)
        let seen = (encoder_ns - m).clamp(0, 300_000_000);
        if !known {
            self.lag_ns.store(seen, Ordering::Relaxed);
        } else if near.is_some() {
            self.lag_ns.store(lag + (seen - lag) / 4, Ordering::Relaxed);
        }
        pts
    }

    pub fn packet(&self, len: usize, pts_ns: i64, keyframe: bool) {
        let mut s = self.sent.lock();
        s.packets += 1;
        s.bytes += len as u64;
        s.keyframes += keyframe as u64;
        s.last_pts_ns = pts_ns;
        if keyframe {
            self.last_key_ns.fetch_max(pts_ns, Ordering::AcqRel);
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        for d in self.queue.get_mut().drain(..) {
            release_buffer(d.buffer);
        }
    }
}

/// Gives back a pulled frame's buffer reference (Apple; nothing elsewhere).
pub fn release_buffer(buffer: usize) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if buffer != 0 {
        unsafe extern "C" {
            fn CFRelease(cf: *const std::ffi::c_void);
        }
        unsafe { CFRelease(buffer as *const std::ffi::c_void) };
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = buffer;
}

/// Program sound waiting for one audio encoder: 48 kHz stereo interleaved.
pub struct AudioEncoder {
    pub id: u32,
    ring: Mutex<(VecDeque<f32>, i64)>,
    ready: Condvar,
    closed: AtomicBool,
    pub sent: Mutex<Sent>,
    /// Its last minute of packets, for clips.
    pub clip: crate::clips::Ring,
}

/// At most two seconds wait (an encoder that stopped pulling loses the oldest).
const AUDIO_MAX: usize = 48_000 * 2 * 2;

impl AudioEncoder {
    pub fn new(id: u32) -> AudioEncoder {
        AudioEncoder {
            id,
            ring: Mutex::new((VecDeque::new(), -1)),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
            sent: Mutex::new(Sent::default()),
            clip: Default::default(),
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The mix thread's block: `l`, `r` from program time `pts_ns`. Nothing once closed (the
    /// sound ends with the pictures).
    pub fn write(&self, l: &[f32], r: &[f32], pts_ns: i64) {
        if self.is_closed() {
            return;
        }
        {
            let mut g = self.ring.lock();
            let (q, first) = &mut *g;
            if q.is_empty() {
                *first = pts_ns;
            }
            for (a, b) in l.iter().zip(r) {
                q.push_back(*a);
                q.push_back(*b);
            }
            let over = q.len().saturating_sub(AUDIO_MAX);
            if over > 0 {
                let over = over + over % 2;
                q.drain(..over);
                *first += crate::clock::samples_to_ns(over as u64 / 2);
            }
        }
        self.ready.notify_one();
    }

    /// Up to `out.len() / 2` frames: `Ok(Some((frames, pts)))`, `Ok(None)` on timeout,
    /// `Err(Closed)` once closed and drained.
    pub fn read(&self, out: &mut [f32], timeout: Duration) -> Result<Option<(usize, i64)>, Closed> {
        let deadline = Instant::now() + timeout;
        let mut g = self.ring.lock();
        loop {
            let (q, first) = &mut *g;
            if !q.is_empty() {
                let n = (out.len() / 2).min(q.len() / 2);
                for (i, s) in q.drain(..n * 2).enumerate() {
                    out[i] = s;
                }
                let pts = *first;
                *first += crate::clock::samples_to_ns(n as u64);
                return Ok(Some((n, pts)));
            }
            if self.is_closed() {
                return Err(Closed);
            }
            if self.ready.wait_until(&mut g, deadline).timed_out() {
                return if self.is_closed() {
                    Err(Closed)
                } else {
                    Ok(None)
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Codec;

    fn encoder() -> VideoEncoder {
        let c = VideoEncoderConfig {
            codec: Codec::H264,
            width: 16,
            height: 16,
            fps: 30,
            bitrate_kbps: 1000,
            keyframe_interval_s: 2,
            realtime: true,
        };
        VideoEncoder::new(1, c, EncoderTarget::Texture)
    }

    /// A slow device (an emulator at 5 fps) whose host encoder counts its keyframe interval in
    /// frames at the configured 30 fps: the stream still gets a keyframe every 2 s of stream time.
    #[test]
    fn a_slow_stream_gets_a_keyframe_every_two_seconds_of_stream_time() {
        let e = encoder();
        let (mut keys, mut since_host_key) = (Vec::new(), 0);
        for i in 0..60i64 {
            let pts = i * 200_000_000; // 5 fps for 12 s
            e.queue(Drawn {
                buffer: 0,
                pts_ns: pts,
                keyframe: false,
                rgba: None,
            });
            let d = e.next(Duration::ZERO).unwrap().unwrap();
            // the host: a keyframe when asked, else every 2 s × 30 frames of its own count
            let key = d.keyframe || since_host_key >= 60;
            since_host_key = if key { 0 } else { since_host_key + 1 };
            e.packet(1000, d.pts_ns, key);
            if key {
                keys.push(d.pts_ns);
            }
        }
        assert_eq!(keys.first(), Some(&0), "the stream starts on one");
        for w in keys.windows(2) {
            assert!(
                w[1] - w[0] <= 2_000_000_000,
                "a keyframe every 2 s: {keys:?}"
            );
        }
        assert!(
            *keys.last().unwrap() >= 10_000_000_000,
            "to the end: {keys:?}"
        );
    }

    #[test]
    fn pulls_get_frames_in_order_then_end() {
        let e = encoder();
        for i in 0..5 {
            e.queue(Drawn {
                buffer: 0,
                pts_ns: i,
                keyframe: false,
                rgba: None,
            });
        }
        // only the newest three wait; the first frame's keyframe request, dropped with it, is
        // carried on to the next frame queued
        let a = e.next(Duration::ZERO).unwrap().unwrap();
        assert_eq!(a.pts_ns, 2);
        assert_eq!(e.next(Duration::ZERO).unwrap().unwrap().pts_ns, 3);
        let last = e.next(Duration::ZERO).unwrap().unwrap();
        assert!(last.pts_ns == 4 && last.keyframe);
        assert!(e.next(Duration::from_millis(5)).unwrap().is_none());
        e.close();
        assert!(e.next(Duration::from_millis(5)).is_err());
    }

    #[test]
    fn closing_keeps_the_last_frames_and_stops_the_sound() {
        let e = encoder();
        e.queue(Drawn {
            buffer: 0,
            pts_ns: 7,
            keyframe: false,
            rgba: None,
        });
        e.close();
        assert_eq!(
            e.next(Duration::ZERO).unwrap().unwrap().pts_ns,
            7,
            "the tail is pulled"
        );
        assert!(e.next(Duration::ZERO).is_err());
        let a = AudioEncoder::new(2);
        a.write(&[0.1; 480], &[0.1; 480], 0);
        a.close();
        a.write(&[0.1; 480], &[0.1; 480], 10_000_000);
        let mut out = vec![0.0; 4096];
        assert_eq!(
            a.read(&mut out, Duration::ZERO).unwrap().unwrap().0,
            480,
            "nothing after the close"
        );
        assert!(a.read(&mut out, Duration::ZERO).is_err());
    }

    #[test]
    fn android_packets_map_to_their_presents_in_order_and_none_is_lost() {
        let e = encoder();
        let f = 33_333_333i64;
        let m = |n: i64| 1_000_000_000 + n * f;
        for n in 0..40i64 {
            e.presented(m(n), n * f);
        }
        // stamped about 25 ms after each present (most of a frame), give or take 5 ms
        let jitter = [0, 5_000_000, -5_000_000, 3_000_000, -2_000_000];
        for n in 0..20i64 {
            let stamp = m(n) + 25_000_000 + jitter[n as usize % 5];
            assert_eq!(e.program_pts(stamp), n * f, "frame {n}");
        }
        // the encoder dropped frame 20: frame 21's packet skips it
        assert_eq!(e.program_pts(m(21) + 26_000_000), 21 * f);
        // a stamp far from every present still gets the next one in order, never -1
        assert_eq!(e.program_pts(99_000_000_000), 22 * f);
        assert_eq!(e.program_pts(m(23) + 24_000_000), 23 * f);
        // before any present: nothing to give
        assert_eq!(encoder().program_pts(1_000_000_000), -1);
        // presents the encoder never took (made before it started) don't shift the first match
        let e = encoder();
        for n in 0..10i64 {
            e.presented(m(n), n * f);
        }
        assert_eq!(e.program_pts(m(6) + 20_000_000), 6 * f);
        assert_eq!(e.program_pts(m(7) + 22_000_000), 7 * f);
    }

    #[test]
    fn audio_reads_carry_program_time() {
        let a = AudioEncoder::new(2);
        a.write(&[0.1; 480], &[0.2; 480], 10_000_000);
        a.write(&[0.1; 480], &[0.2; 480], 20_000_000);
        let mut out = vec![0.0; 1024 * 2];
        let (n, pts) = a.read(&mut out, Duration::ZERO).unwrap().unwrap();
        assert_eq!((n, pts), (960, 10_000_000));
        assert_eq!(out[0..2], [0.1, 0.2]);
        a.close();
        assert!(a.read(&mut out, Duration::ZERO).is_err());
    }
}
