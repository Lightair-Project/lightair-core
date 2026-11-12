//! Meters, written by the audio thread and read by the UI without locks.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Default)]
struct F(AtomicU32);

impl F {
    fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    /// Keeps the larger of the stored value and `v` (until read with `take`).
    fn max(&self, v: f32) {
        let mut cur = self.0.load(Ordering::Relaxed);
        while v > f32::from_bits(cur) {
            match self.0.compare_exchange_weak(cur, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
    }
    fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0f32.to_bits(), Ordering::Relaxed))
    }
}

/// One strip's or mix's levels (linear). Peaks are the highest since the UI last read them.
#[derive(Debug, Default)]
pub struct Meter {
    peak: [F; 2],
    rms: [F; 2],
    /// Gain reduction of the strip's dynamics (dB, ≤ 0), the deepest since the last read.
    reduction: F,
    /// LUFS (program and mixes only): momentary, short-term, integrated; true peak (dBTP).
    momentary: F,
    short: F,
    integrated: F,
    true_peak: F,
}

/// What a meter shows, in dB (silence: −∞).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub peak_db: [f32; 2],
    pub rms_db: [f32; 2],
    pub reduction_db: f32,
}

/// Loudness, for masters and mixes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub momentary_lufs: f32,
    pub short_lufs: f32,
    pub integrated_lufs: f32,
    pub true_peak_dbtp: f32,
}

impl Meter {
    /// A new meter, silent (LUFS at −∞).
    pub fn shared() -> Arc<Self> {
        let m = Arc::new(Self::default());
        for f in [&m.momentary, &m.short, &m.integrated, &m.true_peak] {
            f.store(f32::NEG_INFINITY);
        }
        m
    }

    /// Audio thread: one block's levels.
    pub(crate) fn feed(&self, l: &[f32], r: &[f32]) {
        let n = l.len().max(1) as f32;
        for (ch, buf) in [l, r].into_iter().enumerate() {
            let (mut peak, mut sum) = (0f32, 0f32);
            for &x in buf {
                peak = peak.max(x.abs());
                sum += x * x;
            }
            self.peak[ch].max(peak);
            // RMS over the block, smoothed towards it (≈ 300 ms at 48 kHz/256)
            let rms = (sum / n).sqrt();
            let old = self.rms[ch].load();
            self.rms[ch].store(old + (rms - old) * 0.08);
        }
    }

    pub(crate) fn feed_reduction(&self, db: f32) {
        // stored negated so `max` keeps the deepest
        self.reduction.max(-db);
    }

    pub(crate) fn feed_loudness(&self, l: Loudness) {
        self.momentary.store(l.momentary_lufs);
        self.short.store(l.short_lufs);
        self.integrated.store(l.integrated_lufs);
        self.true_peak.store(l.true_peak_dbtp);
    }

    /// UI: the levels since the last read (peaks reset).
    pub fn read(&self) -> Reading {
        let db = |g: f32| crate::gain_to_db(g);
        Reading {
            peak_db: [db(self.peak[0].take()), db(self.peak[1].take())],
            rms_db: [db(self.rms[0].load()), db(self.rms[1].load())],
            reduction_db: -self.reduction.take(),
        }
    }

    pub fn loudness(&self) -> Loudness {
        Loudness {
            momentary_lufs: self.momentary.load(),
            short_lufs: self.short.load(),
            integrated_lufs: self.integrated.load(),
            true_peak_dbtp: self.true_peak.load(),
        }
    }
}

/// EBU R128 loudness of a stereo signal (ebur128), measured on the audio thread.
pub(crate) struct LoudnessMeter {
    r128: ebur128::EbuR128,
    blocks: u32,
}

impl LoudnessMeter {
    pub fn new(sample_rate: u32) -> Self {
        let mode = ebur128::Mode::M | ebur128::Mode::S | ebur128::Mode::I | ebur128::Mode::TRUE_PEAK;
        Self { r128: ebur128::EbuR128::new(2, sample_rate, mode).expect("ebur128"), blocks: 0 }
    }

    pub fn feed(&mut self, l: &[f32], r: &[f32], out: &Meter) {
        let _ = self.r128.add_frames_planar_f32(&[l, r]);
        self.blocks += 1;
        // integrated loudness walks a histogram: every ~100 ms is plenty
        if self.blocks.is_multiple_of(16) {
            let lufs = |v: Result<f64, ebur128::Error>| v.map_or(f32::NEG_INFINITY, |v| v as f32);
            let tp = (0..2)
                .filter_map(|c| self.r128.true_peak(c).ok())
                .fold(0f64, f64::max);
            out.feed_loudness(Loudness {
                momentary_lufs: lufs(self.r128.loudness_momentary()),
                short_lufs: lufs(self.r128.loudness_shortterm()),
                integrated_lufs: lufs(self.r128.loudness_global()),
                true_peak_dbtp: crate::gain_to_db(tp as f32),
            });
        }
    }
}
