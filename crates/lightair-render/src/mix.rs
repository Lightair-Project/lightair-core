//! A sequence's sound mixed in Rust: the same mix as [`crate::sound::mix_of`]'s FFmpeg graph, as
//! samples for any stretch of time. Each sounding clip's piece of its sources (a cleaned voice in
//! place of the source when it's been made), sped with its pitch kept, shaped by its volume keys
//! and fades, placed where it starts; music pressed down under the voices by a sidechain
//! compressor with the graph's settings. Sources are read through the media backend
//! ([`crate::media::backend`]), so a phone mixes with its own decoders, a few seconds at a time
//! as they're played: an hour's recording isn't decoded whole to play a minute of it.
//!
//! The desktop exports and plays through the graph as before; this is what a player or an export
//! without FFmpeg uses, and what a mix can be tested with, sample by sample.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::media::MediaError;
use crate::sequence::{Clip, Sequence};
use crate::sound;

/// Stereo.
const CHANNELS: usize = 2;

/// The sidechain compressor's settings in the graph (`sidechaincompress=threshold=0.02:ratio=8:
/// attack=20:release=400`).
const DUCK_THRESHOLD: f32 = 0.02;
const DUCK_RATIO: f32 = 8.0;
const DUCK_ATTACK_MS: f32 = 20.0;
const DUCK_RELEASE_MS: f32 = 400.0;

/// How much of a source is read at once (seconds), and how many such windows each source keeps.
const WINDOW_S: u64 = 10;
const WINDOWS_KEPT: usize = 3;

/// A source's sound, read in windows as it's needed.
struct Source {
    source: sound::Source,
    rate: u32,
    /// Windows read, by index, most recently used last.
    windows: Mutex<Vec<(u64, Arc<Vec<f32>>)>>,
}

impl Source {
    fn window_frames(&self) -> u64 {
        WINDOW_S * self.rate as u64
    }

    /// Window `i` (frames `i·window` onwards), read if it isn't kept; empty past the end or
    /// where it can't be read (silence: a render can't fail midway).
    fn window(&self, i: u64) -> Arc<Vec<f32>> {
        let mut kept = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = kept.iter().position(|(w, _)| *w == i) {
            let w = kept.remove(at);
            let samples = w.1.clone();
            kept.push(w);
            return samples;
        }
        let n = self.window_frames();
        let read = sound::decode_range(&self.source.0, self.source.1, self.rate, CHANNELS as u32, i * n, n as usize)
            .unwrap_or_else(|e| {
                tracing::warn!("mix: {}: {e}", self.source.0.display());
                Vec::new()
            });
        let samples = Arc::new(read);
        kept.push((i, samples.clone()));
        if kept.len() > WINDOWS_KEPT {
            kept.remove(0);
        }
        samples
    }

    /// Frames `lo..hi` of the source in one piece (silence past its end).
    fn view(&self, lo: u64, hi: u64) -> Vec<f32> {
        let n = self.window_frames();
        let mut out = Vec::with_capacity(((hi - lo) as usize) * CHANNELS);
        let mut at = lo;
        while at < hi {
            let i = at / n;
            let w = self.window(i);
            let from = ((at - i * n) as usize) * CHANNELS;
            let until = (((hi.min((i + 1) * n)) - i * n) as usize) * CHANNELS;
            let have = w.len().min(until);
            if from < have {
                out.extend_from_slice(&w[from..have]);
            }
            // past what the window held: silence up to its end (or `hi`)
            out.resize(((hi.min((i + 1) * n) - lo) as usize) * CHANNELS, 0.0);
            at = (i + 1) * n;
        }
        out
    }
}

/// One clip's sound from one source.
struct Part {
    source: Arc<Source>,
    clip: Clip,
    ducked: bool,
}

/// A sequence's sound, ready to be rendered for any stretch of time.
pub struct Mix {
    rate: u32,
    parts: Vec<Part>,
}

impl Mix {
    /// The mix of `seq` at `rate` (samples a second); `None` when nothing in it makes a sound.
    /// Each source's first window is read now, so an unreadable one says so here; the rest as
    /// it's played.
    pub fn new(seq: &Sequence, rate: u32) -> Result<Option<Mix>, MediaError> {
        let mut read: HashMap<sound::Source, Arc<Source>> = HashMap::new();
        let mut parts = Vec::new();
        for (c, video) in sound::sounding(seq) {
            for source in sound::sound_files(seq, c, video) {
                let s = match read.get(&source) {
                    Some(s) => s.clone(),
                    None => {
                        let first = sound::decode_range(
                            &source.0,
                            source.1,
                            rate,
                            CHANNELS as u32,
                            0,
                            (WINDOW_S * rate as u64) as usize,
                        )?;
                        let s = Arc::new(Source {
                            source: source.clone(),
                            rate,
                            windows: Mutex::new(vec![(0, Arc::new(first))]),
                        });
                        read.insert(source, s.clone());
                        s
                    }
                };
                parts.push(Part { source: s, clip: c.clone(), ducked: c.duck });
            }
        }
        Ok((!parts.is_empty()).then_some(Mix { rate, parts }))
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The sound from `from_ms` for `frames` frames: interleaved stereo f32 at the mix's rate.
    pub fn render(&self, from_ms: u64, frames: usize) -> Vec<f32> {
        let mut voices = vec![0f32; frames * CHANNELS];
        let mut music = vec![0f32; frames * CHANNELS];
        let any_music = self.parts.iter().any(|p| p.ducked);
        let any_voice = self.parts.iter().any(|p| !p.ducked);
        for p in &self.parts {
            // music only ducks under voices; without either, everything is one plain mix
            let into = if p.ducked && any_voice { &mut music } else { &mut voices };
            self.add(p, from_ms, into);
        }
        if any_music && any_voice {
            self.duck(from_ms, &voices, &mut music);
            for (v, m) in voices.iter_mut().zip(&music) {
                *v += m;
            }
        }
        voices
    }

    /// Adds one part's sound into `out` (`from_ms` onwards).
    fn add(&self, p: &Part, from_ms: u64, out: &mut [f32]) {
        let rate = self.rate as f64;
        let c = &p.clip;
        let (start, end) = (c.start_ms, c.end_ms());
        let speed = c.speed.clamp(0.25, 16.0) as f64;
        let frames = out.len() / CHANNELS;
        let first = from_ms as f64 / 1000.0 * rate;
        // the clip's stretch of this render (frames into the clip), and the source it reads
        let clip_from = start as f64 / 1000.0 * rate;
        let clip_to = end as f64 / 1000.0 * rate;
        let (u0, u1) = ((first - clip_from).max(0.0), (first + frames as f64).min(clip_to) - clip_from);
        if u1 <= u0 {
            return;
        }
        let source_at = c.in_ms as f64 / 1000.0 * rate;
        let grain = (GRAIN_MS / 1000.0 * rate).max(4.0);
        let lo = (source_at + (u0 - grain).max(0.0) * speed - grain).max(0.0).floor() as u64;
        let hi = (source_at + (u1 + grain) * speed + grain).ceil() as u64 + 2;
        let view = p.source.view(lo, hi);
        let base = lo as f64;
        for i in 0..frames {
            let at = first + i as f64;
            let t_ms = (at / rate * 1000.0) as u64;
            if t_ms < start || t_ms >= end {
                continue;
            }
            let gain = c.gain_at(t_ms);
            if gain == 0.0 {
                continue;
            }
            // time into the clip (frames), and where that is in its source (in the view)
            let u = at - clip_from;
            let (l, r) = if speed == 1.0 {
                frame(&view, source_at + u - base)
            } else {
                stretched(&view, source_at - base, u, speed, rate)
            };
            out[i * 2] += l * gain;
            out[i * 2 + 1] += r * gain;
        }
    }

    /// The music pressed down by the voices: a compressor keyed by the voices' level, with
    /// the graph's threshold, ratio, attack and release. Its envelope starts settled half a
    /// second before `from_ms`, so a stretch sounds the same rendered alone or in a run.
    fn duck(&self, from_ms: u64, voices: &[f32], music: &mut [f32]) {
        let rate = self.rate as f32;
        let attack = (-1.0 / (DUCK_ATTACK_MS / 1000.0 * rate)).exp();
        let release = (-1.0 / (DUCK_RELEASE_MS / 1000.0 * rate)).exp();
        let mut env = 0f32;
        let mut follow = |level: f32| {
            let k = if level > env { attack } else { release };
            env = level + k * (env - level);
            env
        };
        // the voices just before, to settle the envelope
        let lead = (rate as u64 / 2).min(from_ms * self.rate as u64 / 1000) as usize;
        if lead > 0 {
            let before = self.voices_only(from_ms - (lead as u64 * 1000 / self.rate as u64), lead);
            for f in before.as_chunks::<CHANNELS>().0 {
                follow(f[0].abs().max(f[1].abs()));
            }
        }
        for (v, m) in voices.as_chunks::<CHANNELS>().0.iter().zip(music.as_chunks_mut::<CHANNELS>().0) {
            let e = follow(v[0].abs().max(v[1].abs()));
            if e > DUCK_THRESHOLD {
                let g = (DUCK_THRESHOLD + (e - DUCK_THRESHOLD) / DUCK_RATIO) / e;
                m[0] *= g;
                m[1] *= g;
            }
        }
    }

    fn voices_only(&self, from_ms: u64, frames: usize) -> Vec<f32> {
        let mut out = vec![0f32; frames * CHANNELS];
        for p in self.parts.iter().filter(|p| !p.ducked) {
            self.add(p, from_ms, &mut out);
        }
        out
    }
}

/// A source's frame at fractional position `at` (frames), between its neighbours.
fn frame(s: &[f32], at: f64) -> (f32, f32) {
    if at < 0.0 {
        return (0.0, 0.0);
    }
    let i = at.floor() as usize;
    let k = (at - i as f64) as f32;
    let get = |j: usize| -> (f32, f32) { s.get(j * 2..j * 2 + 2).map_or((0.0, 0.0), |f| (f[0], f[1])) };
    let (a, b) = (get(i), get(i + 1));
    (a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k)
}

/// Grains of the stretch: 40 ms, half overlapping.
const GRAIN_MS: f64 = 40.0;

/// A sped-up or slowed-down source with its pitch kept, at output position `u` (frames into the
/// clip): overlapping grains, each read at the source's own rate from where the speed puts it,
/// faded in and out (Hann windows summing to one). Any position computes on its own, so any
/// stretch of time renders the same alone or in a run.
fn stretched(s: &[f32], source_at: f64, u: f64, speed: f64, rate: f64) -> (f32, f32) {
    let n = (GRAIN_MS / 1000.0 * rate).max(4.0);
    let hop = n / 2.0;
    let k = (u / hop).floor();
    let mut out = (0.0f32, 0.0f32);
    // the two grains covering u: the one starting at k·hop and the one before it
    for g in [k - 1.0, k] {
        if g < 0.0 {
            continue;
        }
        let into = u - g * hop;
        if !(0.0..n).contains(&into) {
            continue;
        }
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * into / n).cos();
        let (l, r) = frame(s, source_at + g * hop * speed + into);
        out.0 += l * w as f32;
        out.1 += r * w as f32;
    }
    out
}
