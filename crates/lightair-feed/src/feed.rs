//! One camera's feed: access units in, the transport stream out to the desktop.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::srt::{SenderStats, SrtSender, SrtTarget};
use crate::ts::{AudioConfig, Muxer, NalFormat, VideoCodec};

/// Where the transport stream goes: SRT, or the USB link's media channel.
pub trait TsSink: Send {
    /// Whole 188-byte packets.
    fn write(&mut self, packets: &[u8]);
    /// The end of an access unit: send what's waiting.
    fn flush(&mut self) {}
    /// The end of a video picture: flush, and time how long it waits to be written.
    fn end_frame(&mut self) {
        self.flush();
    }
    /// How long pictures waited between [`Self::end_frame`] and the socket, since the last call.
    fn take_send_times(&mut self) -> Vec<Duration> {
        Vec::new()
    }
    /// Datagrams (or frames) still waiting to go out.
    fn queued(&self) -> usize {
        0
    }
    fn stats(&self) -> SenderStats;
    /// Whether the encoder should make a keyframe now (a new connection, or data dropped).
    fn take_keyframe_request(&self) -> bool {
        false
    }
}

impl TsSink for SrtSender {
    fn end_frame(&mut self) {
        SrtSender::end_frame(self);
    }
    fn take_send_times(&mut self) -> Vec<Duration> {
        SrtSender::take_send_times(self)
    }
    fn write(&mut self, packets: &[u8]) {
        SrtSender::write(self, packets);
    }
    fn flush(&mut self) {
        SrtSender::flush(self);
    }
    fn queued(&self) -> usize {
        SrtSender::queued(self)
    }
    fn stats(&self) -> SenderStats {
        SrtSender::stats(self)
    }
    fn take_keyframe_request(&self) -> bool {
        SrtSender::take_keyframe_request(self)
    }
}

/// How the feed is doing, for the phone's health line and the desktop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeedStats {
    pub connected: bool,
    /// Over the last second or so.
    pub bitrate_kbps: u32,
    /// Datagrams dropped so far.
    pub dropped: u64,
    pub error: Option<String>,
}

struct Inner {
    mux: Muxer,
    out: Box<dyn TsSink>,
    rate: (Instant, u64, u32),
    adapt: Adapt,
    /// The shortest the queue has been since the last look: a keyframe's burst fills it for a
    /// moment, a network that can't keep up keeps it full.
    queue_low: usize,
}

/// The bitrate the network can carry now: down 20 %
/// when the queue backs up or data is dropped, up 10 % after 10 s of a short queue, never above
/// what the desktop asked for, never below a quarter of it.
#[derive(Default)]
struct Adapt {
    target: u32,
    calm_since: Option<Instant>,
    dropped: u64,
    last_step: Option<Instant>,
}

impl Adapt {
    fn step(&mut self, waiting_ms: u32, dropped: u64, max_kbps: u32, now: Instant) -> u32 {
        if self.target == 0 || self.target > max_kbps {
            self.target = max_kbps;
        }
        let floor = (max_kbps / 4).max(500).min(max_kbps);
        let may_step = self.last_step.is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1));
        if (dropped > self.dropped || waiting_ms > CONGESTED_MS) && may_step {
            self.target = (self.target * 4 / 5).max(floor);
            self.calm_since = None;
            self.last_step = Some(now);
        } else if waiting_ms < CALM_MS {
            let since = *self.calm_since.get_or_insert(now);
            if now.duration_since(since) >= std::time::Duration::from_secs(10) && self.target < max_kbps {
                self.target = (self.target + max_kbps / 10).min(max_kbps);
                self.calm_since = Some(now);
                self.last_step = Some(now);
            }
        } else {
            self.calm_since = None;
        }
        self.dropped = dropped;
        self.target
    }
}

/// More than this much waiting is congestion; less than `CALM` is a short queue.
const CONGESTED_MS: u32 = 120;
const CALM_MS: u32 = 20;

/// A camera's encoded picture and sound on their way to the desktop. Thread-safe: the video and
/// audio encoders may call from their own threads.
pub struct Feed {
    inner: Mutex<Inner>,
}

impl Feed {
    /// Sends to any [`TsSink`] (the USB link's media channel, a file in tests).
    pub fn sink(out: Box<dyn TsSink>, video: VideoCodec, audio: Option<AudioConfig>) -> Feed {
        Feed {
            inner: Mutex::new(Inner {
                mux: Muxer::new(video, audio),
                out,
                rate: (Instant::now(), 0, 0),
                adapt: Adapt::default(),
                queue_low: usize::MAX,
            }),
        }
    }

    /// Starts sending to the desktop's SRT listener (connecting, and reconnecting, by itself).
    pub fn srt(target: SrtTarget, video: VideoCodec, audio: Option<AudioConfig>) -> std::io::Result<Feed> {
        Ok(Feed {
            inner: Mutex::new(Inner {
                mux: Muxer::new(video, audio),
                out: Box::new(SrtSender::start(target)?),
                rate: (Instant::now(), 0, 0),
                adapt: Adapt::default(),
                queue_low: usize::MAX,
            }),
        })
    }

    /// The encoder's parameter sets (SPS, PPS, and VPS for HEVC), without start codes.
    pub fn parameter_sets(&self, nals: Vec<Vec<u8>>) {
        self.inner.lock().mux.parameter_sets(nals);
    }

    pub fn video(&self, data: &[u8], format: NalFormat, pts_us: u64, keyframe: bool) {
        let mut i = self.inner.lock();
        let packets = i.mux.video(data, format, pts_us, None, keyframe);
        // before this frame joins it
        i.queue_low = i.queue_low.min(i.out.queued());
        i.out.write(&packets);
        i.out.end_frame();
    }

    /// How long pictures waited between being muxed and written to the socket, since the last
    /// call.
    pub fn take_send_times(&self) -> Vec<Duration> {
        self.inner.lock().out.take_send_times()
    }

    pub fn audio(&self, data: &[u8], pts_us: u64) {
        let mut i = self.inner.lock();
        let packets = i.mux.audio(data, pts_us);
        i.out.write(&packets);
        i.out.flush();
    }

    /// Whether the encoder should make a keyframe now: the desktop connected again, or data
    /// was dropped. (The desktop's own `keyframe` message comes over the control channel.)
    pub fn take_keyframe_request(&self) -> bool {
        self.inner.lock().out.take_keyframe_request()
    }

    /// The encoder's bitrate now, given what the desktop asked for (`max_kbps`). Call it about
    /// once a second; set the encoder when the answer changes.
    pub fn adapt(&self, max_kbps: u32) -> u32 {
        let mut i = self.inner.lock();
        let queued = match std::mem::replace(&mut i.queue_low, usize::MAX) {
            usize::MAX => i.out.queued(),
            low => low,
        } as u64;
        let dropped = i.out.stats().dropped;
        let target = i.adapt.target.max(1);
        // what's waiting, in milliseconds at the current bitrate
        let waiting_ms = (queued * crate::srt::DATAGRAM as u64 * 8 / target as u64) as u32;
        i.adapt.step(waiting_ms, dropped, max_kbps, Instant::now())
    }

    pub fn stats(&self) -> FeedStats {
        let mut i = self.inner.lock();
        let s = i.out.stats();
        let (at, bytes, kbps) = i.rate;
        let elapsed = at.elapsed().as_secs_f64();
        let kbps = if elapsed >= 1.0 {
            let k = ((s.sent_bytes - bytes) as f64 * 8.0 / 1000.0 / elapsed) as u32;
            i.rate = (Instant::now(), s.sent_bytes, k);
            k
        } else {
            kbps
        };
        FeedStats { connected: s.connected, bitrate_kbps: kbps, dropped: s.dropped, error: s.error }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn steps_down_on_congestion_and_back_up_when_calm() {
        let mut a = Adapt::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(a.step(0, 0, 10_000, at(0)), 10_000);
        // a backed-up queue: down 20 %, at most once a second
        assert_eq!(a.step(300, 0, 10_000, at(1)), 8_000);
        assert_eq!(a.step(300, 0, 10_000, at(1)), 8_000);
        // drops count as congestion too
        assert_eq!(a.step(0, 3, 10_000, at(2)), 6_400);
        // never below a quarter
        for s in 3..20 {
            a.step(500, 3, 10_000, at(s));
        }
        assert_eq!(a.target, 2_500);
        // ten calm seconds: up 10 % of the maximum, and so on to the maximum, never above
        assert_eq!(a.step(0, 3, 10_000, at(30)), 2_500);
        assert_eq!(a.step(0, 3, 10_000, at(40)), 3_500);
        let mut s = 50;
        while a.target < 10_000 {
            a.step(0, 3, 10_000, at(s));
            s += 10;
        }
        assert_eq!(a.step(0, 3, 10_000, at(s + 10)), 10_000);
        // the desktop asking for less lowers it at once
        assert_eq!(a.step(0, 3, 6_000, at(s + 11)), 6_000);
    }
}
