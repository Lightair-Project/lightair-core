//! The mix thread: every audio feed resampled to 48 kHz stereo, the
//! project's mixer run through lightair-audio in 10 ms blocks, the program sound handed to each
//! audio encoder, and the program clock advanced. Woken by the master feed's pushes; a 10 ms
//! timer drives it when there's no microphone.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use la::dsp::{NoiseParams, Params, VoiceParams};
use la::{Channels, EffectSpec, GraphSpec, Strip};
use lightair_audio as la;
use lightair_model::{Effect, Id, Workspace};

use crate::clock::{self, RATE};
use crate::feeds::{AudioFeed, Feed};
use crate::hot::Hot;

pub const BLOCK: usize = 480;

/// The engine's node id for a document id.
pub fn node(id: Id) -> la::NodeId {
    id.0.as_u128()
}

fn num(e: &Effect, key: &str, default: f32) -> f32 {
    e.settings
        .get(key)
        .and_then(|v| v.as_f64())
        .map_or(default, |v| v as f32)
}

/// The effects Studio runs on a track: Voice Clarity, noise suppression, gain.
fn params(e: &Effect) -> Option<Params> {
    Some(match e.kind.as_str() {
        "audio.voice" => Params::Voice(VoiceParams {
            amount: num(e, "amount", 0.5).clamp(0.0, 1.0),
            podcast: e
                .settings
                .get("podcast")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            ..VoiceParams::default()
        }),
        "audio.noise" => Params::Noise(NoiseParams {
            strength: num(e, "strength", 1.0).clamp(0.0, 1.0),
            vad_threshold: num(e, "vad_threshold", 0.0),
        }),
        "audio.gain" => Params::Gain(la::dsp::GainParams {
            db: num(e, "db", 0.0),
            ..Default::default()
        }),
        _ => return None,
    })
}

/// The graph for the project: one stereo strip per track whose source has a feed.
/// The alerts' sound: its input and strip in the mix (not a document track).
pub const ALERTS_INPUT: la::NodeId = 0xA1E7_0001;
pub const ALERTS_STRIP: la::NodeId = 0xA1E7_0002;

pub fn spec(ws: &Workspace, inputs: &HashMap<Id, u32>) -> GraphSpec {
    let mut g = GraphSpec::default();
    let mut alerts = Strip::track(ALERTS_STRIP, ALERTS_INPUT, Channels::Stereo(0, 1));
    alerts.name = "Alerts".into();
    g.strips.push(alerts);
    for t in &ws.tracks {
        if !inputs.contains_key(&t.source) {
            continue;
        }
        let mut s = Strip::track(node(t.id), node(t.source), Channels::Stereo(0, 1));
        s.name = t.name.clone();
        s.gain_db = t.volume_db;
        s.mute = t.muted;
        s.inserts = t
            .effects
            .iter()
            .filter_map(|e| {
                Some(EffectSpec {
                    id: node(e.id),
                    enabled: e.enabled,
                    params: params(e)?,
                })
            })
            .collect();
        g.strips.push(s);
    }
    g
}

/// One feed's input to the graph: its samples resampled into a block, by linear interpolation.
struct Input {
    feed: u32,
    node: la::NodeId,
    mic: bool,
    pending: Vec<[f32; 2]>,
    pos: f64,
    l: Vec<f32>,
    r: Vec<f32>,
    gain: f32,
}

impl Input {
    fn new(feed: u32, node: la::NodeId, mic: bool) -> Input {
        Input {
            feed,
            node,
            mic,
            pending: Vec::new(),
            pos: 0.0,
            l: vec![0.0; BLOCK],
            r: vec![0.0; BLOCK],
            gain: 1.0,
        }
    }

    /// Reads what's in the ring into `pending` (stereo frames).
    fn drain(&mut self, f: &AudioFeed) {
        let channels = f.channels.load(Ordering::Relaxed).max(1) as usize;
        let Some(mut c) = f.consumer.try_lock() else {
            return;
        };
        let n = c.slots() / channels * channels;
        if n == 0 {
            return;
        }
        if let Ok(chunk) = c.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            let mut it = a.iter().chain(b.iter());
            for _ in 0..n / channels {
                let mut frame = [0f32; 8];
                for s in frame.iter_mut().take(channels) {
                    *s = *it.next().unwrap_or(&0.0);
                }
                let (l, r) = if channels == 1 {
                    (frame[0], frame[0])
                } else {
                    (frame[0], frame[1])
                };
                self.pending.push([l, r]);
            }
            chunk.commit_all();
        }
    }

    /// Puts a feed that isn't the clock (a video's sound) on the program clock: its next frame
    /// is heard at its stamp, so silence goes in front of sound that's early and sound that's
    /// late is skipped. 10 ms either way is left alone, so a host's jitter doesn't churn.
    fn align(&mut self, f: &AudioFeed, block_ns: i64) {
        let end = f.end_ns.load(Ordering::Acquire);
        if end == i64::MIN || self.pending.is_empty() {
            return;
        }
        let rate = f.rate.load(Ordering::Relaxed).max(1) as f64;
        let waiting = self.pending.len() as f64 - self.pos;
        let head = end - (waiting * 1e9 / rate) as i64;
        let off = head - block_ns;
        if off.abs() <= 10_000_000 {
            return;
        }
        let frames = (off.abs() as f64 * rate / 1e9) as usize;
        if off > 0 {
            // early: wait for it, but never more than the ring holds
            let frames = frames.min(rate as usize * 2 / 5);
            let at = self.pos as usize;
            self.pending
                .splice(at..at, std::iter::repeat_n([0.0, 0.0], frames));
        } else {
            let cut = frames.min(self.pending.len().saturating_sub(self.pos as usize));
            let at = self.pos as usize;
            self.pending.drain(at..at + cut);
        }
    }

    /// Output frames available at 48 kHz.
    fn available(&self, rate: u32) -> usize {
        let step = rate as f64 / RATE as f64;
        ((self.pending.len() as f64 - 1.0 - self.pos) / step).max(0.0) as usize
    }

    /// One block at 48 kHz (zeros where there's too little). `gain(i)`: the gain the block's
    /// `i`th sample wants (the privacy cut); the gain moves to it in a straight line over 2.5 ms.
    fn block(&mut self, rate: u32, gain: impl Fn(usize) -> f32) {
        let step = rate as f64 / RATE as f64;
        const RAMP: f32 = 1.0 / 120.0;
        for i in 0..BLOCK {
            let target = gain(i);
            self.gain = if self.gain < target {
                (self.gain + RAMP).min(target)
            } else {
                (self.gain - RAMP).max(target)
            };
            let p = self.pos as usize;
            let (l, r) = if p + 1 < self.pending.len() {
                let t = (self.pos - p as f64) as f32;
                let (a, b) = (self.pending[p], self.pending[p + 1]);
                (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t)
            } else {
                (0.0, 0.0)
            };
            self.l[i] = l * self.gain;
            self.r[i] = r * self.gain;
            if p + 1 < self.pending.len() {
                self.pos += step;
            }
        }
        let used = (self.pos as usize).min(self.pending.len());
        self.pending.drain(..used);
        self.pos -= used as f64;
        // more than half a second waiting: it's running away; keep the newest 100 ms
        if self.pending.len() > rate as usize / 2 {
            let keep = rate as usize / 10;
            let cut = self.pending.len() - keep;
            self.pending.drain(..cut);
        }
    }
}

struct Table<'a>(&'a [Input]);

impl la::Inputs for Table<'_> {
    fn channel(&self, input: la::NodeId, ch: u16) -> Option<&[f32]> {
        let i = self.0.iter().find(|i| i.node == input)?;
        Some(if ch == 0 { &i.l } else { &i.r })
    }
}

/// Which feeds the graph reads, and as which source.
#[derive(Clone, Default)]
pub struct Routing {
    /// (feed, source node, is a microphone)
    pub feeds: Vec<(u32, la::NodeId, bool)>,
}

pub struct Mixer {
    hot: Arc<Hot>,
    renderer: la::Renderer,
    inputs: Vec<Input>,
    routing: Arc<parking_lot::Mutex<Routing>>,
    quit: Arc<AtomicBool>,
}

pub fn spawn(
    hot: Arc<Hot>,
    renderer: la::Renderer,
    routing: Arc<parking_lot::Mutex<Routing>>,
    quit: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("lightair-mix".into())
        .spawn(move || {
            *hot.mixer.lock() = Some(std::thread::current());
            let failed = hot.clone();
            let mut m = Mixer {
                hot,
                renderer,
                inputs: Vec::new(),
                routing,
                quit,
            };
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.run())) {
                failed.fail(format!("The sound stopped: {}", crate::hot::panic_text(&e)));
            }
        })
}

impl Mixer {
    fn run(&mut self) {
        while !self.quit.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(5));
            self.sync_inputs();
            while self.step() {}
        }
    }

    fn sync_inputs(&mut self) {
        let routing = self.routing.lock().clone();
        self.inputs.retain(|i| {
            routing
                .feeds
                .iter()
                .any(|(f, n, _)| *f == i.feed && *n == i.node)
        });
        for (f, n, mic) in routing.feeds {
            if !self.inputs.iter().any(|i| i.feed == f) {
                self.inputs.push(Input::new(f, n, mic));
            }
        }
    }

    /// Makes one block if it's time; false when not.
    fn step(&mut self) -> bool {
        let hot = self.hot.clone();
        let feeds: Vec<Option<Arc<Feed>>> = self.inputs.iter().map(|i| hot.feed(i.feed)).collect();
        for (i, f) in self.inputs.iter_mut().zip(&feeds) {
            if let Some(a) = f.as_deref().and_then(Feed::audio) {
                i.drain(a);
            }
        }
        let samples = hot.clock.samples();
        let now = clock::host_now_ns();
        let master = hot.master.load(Ordering::Acquire);
        let master_at = self.inputs.iter().position(|i| i.feed == master);
        let master_feed = master_at.and_then(|i| feeds[i].clone());
        let live_master = master_feed
            .as_deref()
            .and_then(Feed::audio)
            .filter(|a| now.saturating_sub(a.end_ns.load(Ordering::Acquire)) < 300_000_000);
        match (master_at, live_master) {
            (Some(i), Some(a)) => {
                let rate = a.rate.load(Ordering::Relaxed);
                if self.inputs[i].available(rate) < BLOCK {
                    return false;
                }
                // when the first frame of this block was heard
                let waiting = self.inputs[i].pending.len() as f64 - self.inputs[i].pos;
                let head = a.end_ns.load(Ordering::Acquire)
                    - (waiting * 1e9 / rate as f64) as i64
                    - hot.mic_latency_ns.load(Ordering::Relaxed);
                hot.clock.observe(samples, head);
            }
            _ => {
                // no microphone: the host clock paces it
                if hot.clock.host_at(samples + BLOCK as u64) > now {
                    return false;
                }
            }
        }
        // the privacy cut by program time: the microphones are silent from the sample the
        // picture is covered from
        let block_ns = hot.clock.host_at(samples);
        for (i, f) in self.inputs.iter_mut().zip(&feeds) {
            if !i.mic
                && let Some(a) = f.as_deref().and_then(Feed::audio)
            {
                i.align(a, block_ns);
            }
        }
        let (on, from) = hot.privacy();
        let covered = |i: usize| if samples + i as u64 >= from { on } else { !on };
        for (i, f) in self.inputs.iter_mut().zip(&feeds) {
            let rate = f
                .as_deref()
                .and_then(Feed::audio)
                .map_or(RATE, |a| a.rate.load(Ordering::Relaxed));
            let mic = i.mic;
            i.block(rate, |k| if mic && covered(k) { 0.0 } else { 1.0 });
        }
        let graph = self.renderer.process(&Table(&self.inputs), BLOCK);
        let (l, r) = graph.program();
        let pts = clock::samples_to_ns(samples);
        for e in hot.audio_encoders.read().values() {
            e.write(&l[..BLOCK], &r[..BLOCK], pts);
        }
        hot.clock.produced(BLOCK as u64);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_privacy_ramp_lands_on_silence_within_the_block() {
        let f = AudioFeed::default();
        let mut i = Input::new(1, 7, true);
        assert!(f.push(&[0.5; 960], 1, 48_000, 0));
        i.drain(&f);
        // covered from the block's 100th sample: 2.5 ms later it's silent, and stays so
        i.block(48_000, |k| if k >= 100 { 0.0 } else { 1.0 });
        assert_eq!(i.l[99], 0.5);
        assert!(i.l[100] < 0.5 && i.l[150] > 0.0);
        assert!(i.l[220..].iter().all(|s| *s == 0.0), "{:?}", &i.l[215..225]);
    }

    #[test]
    fn a_videos_sound_is_heard_at_its_stamp() {
        // 100 ms of ones at 48 kHz, stamped to be heard from 1 s
        let ones = vec![1.0f32; 4_800];
        let early = AudioFeed::default();
        assert!(early.push(&ones, 1, 48_000, 1_000_000_000));
        let mut i = Input::new(1, 7, false);
        i.drain(&early);
        // the block starts 50 ms before: 50 ms of silence, then the sound
        i.align(&early, 950_000_000);
        i.block(48_000, |_| 1.0);
        assert_eq!(i.l[BLOCK - 1], 0.0);
        // (BLOCK is 10 ms: the sound starts with the sixth)
        for _ in 0..5 {
            i.block(48_000, |_| 1.0);
        }
        assert_eq!(i.l[0], 1.0);

        // stamped 50 ms before the block: the first 50 ms are skipped
        let late = AudioFeed::default();
        let ramp: Vec<f32> = (0..4_800).map(|n| n as f32).collect();
        assert!(late.push(&ramp, 1, 48_000, 1_000_000_000));
        let mut i = Input::new(1, 7, false);
        i.drain(&late);
        i.align(&late, 1_050_000_000);
        i.block(48_000, |_| 1.0);
        assert_eq!(i.l[0], 2_400.0);

        // within 10 ms: left alone
        let near = AudioFeed::default();
        assert!(near.push(&ramp, 1, 48_000, 1_000_000_000));
        let mut i = Input::new(1, 7, false);
        i.drain(&near);
        i.align(&near, 1_005_000_000);
        i.block(48_000, |_| 1.0);
        assert_eq!(i.l[0], 0.0);
    }

    #[test]
    fn a_16k_feed_comes_out_at_48k() {
        let f = AudioFeed::default();
        let mut i = Input::new(1, 7, true);
        // 20 ms of a 16 kHz ramp, mono
        let samples: Vec<f32> = (0..320).map(|n| n as f32 / 320.0).collect();
        assert!(f.push(&samples, 1, 16_000, 0));
        i.drain(&f);
        assert!(i.available(16_000) >= BLOCK);
        i.block(16_000, |_| 1.0);
        // 10 ms at 48 kHz used 160 input frames: the block ends near 0.5
        assert!(
            (i.l[BLOCK - 1] - 159.66 / 320.0).abs() < 0.01,
            "{}",
            i.l[BLOCK - 1]
        );
        assert_eq!(i.l[100], i.r[100]);
    }
}
