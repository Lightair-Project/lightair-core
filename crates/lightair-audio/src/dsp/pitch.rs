//! The voice changer (docs/13, "Built-in replacements"): pitch moved up or down by semitones
//! without changing speed, and an optional robot tone. A delay line read by two taps sliding
//! at the pitch ratio, each faded in and out (Hann windows that sum to one) so the jumps where
//! a tap wraps aren't heard. Lightair's own code.

use super::{Ctx, Params, Processor};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchParams {
    /// −12..12.
    pub semitones: f32,
    /// A robot's ring-modulated tone.
    pub robot: bool,
    /// 0 dry .. 1 changed.
    pub mix: f32,
}

impl Default for PitchParams {
    fn default() -> Self {
        Self { semitones: 0.0, robot: false, mix: 1.0 }
    }
}

pub struct Pitch {
    p: PitchParams,
    sr: f32,
    buf: [Box<[f32]>; 2],
    pos: usize,
    /// The taps' place in the window (0..1).
    phase: f32,
    /// The robot's oscillator (radians).
    ring: f32,
}

/// The window the taps slide across (seconds): long enough for low voices, short enough not to echo.
const WINDOW: f32 = 0.06;

impl Pitch {
    pub fn new(p: PitchParams, sr: f32) -> Self {
        let len = (sr * WINDOW * 2.0) as usize + 4;
        Self { p, sr, buf: [vec![0.0; len].into_boxed_slice(), vec![0.0; len].into_boxed_slice()], pos: 0, phase: 0.0, ring: 0.0 }
    }
}

impl Processor for Pitch {
    fn kind(&self) -> &'static str {
        "audio.pitch"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Pitch(p) = params {
            self.p = *p;
        }
    }
    fn reset(&mut self) {
        for b in &mut self.buf {
            b.fill(0.0);
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let n = self.buf[0].len();
        let w = WINDOW * self.sr;
        let ratio = 2f32.powf(self.p.semitones.clamp(-12.0, 12.0) / 12.0);
        let step = (1.0 - ratio) / w;
        let mix = self.p.mix.clamp(0.0, 1.0);
        let ring_step = 2.0 * std::f32::consts::PI * 55.0 / self.sr;
        let shift = self.p.semitones.abs() > 0.01;
        for i in 0..l.len() {
            self.buf[0][self.pos] = l[i];
            self.buf[1][self.pos] = r[i];
            let (mut yl, mut yr) = (l[i], r[i]);
            if shift {
                self.phase = (self.phase + step).rem_euclid(1.0);
                let read = |buf: &[f32], pos: usize, d: f32| {
                    let p = (pos as f32 + n as f32 - d - 1.0).rem_euclid(n as f32);
                    let (i0, f) = (p.floor() as usize, p.fract());
                    buf[i0 % n] * (1.0 - f) + buf[(i0 + 1) % n] * f
                };
                let (p1, p2) = (self.phase, (self.phase + 0.5) % 1.0);
                let (g1, g2) = (
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * p1).cos(),
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * p2).cos(),
                );
                yl = read(&self.buf[0], self.pos, p1 * w) * g1 + read(&self.buf[0], self.pos, p2 * w) * g2;
                yr = read(&self.buf[1], self.pos, p1 * w) * g1 + read(&self.buf[1], self.pos, p2 * w) * g2;
            }
            if self.p.robot {
                self.ring = (self.ring + ring_step) % (2.0 * std::f32::consts::PI);
                let m = self.ring.sin();
                yl *= m;
                yr *= m;
            }
            self.pos = (self.pos + 1) % n;
            l[i] = l[i] * (1.0 - mix) + yl * mix;
            r[i] = r[i] * (1.0 - mix) + yr * mix;
        }
    }
}
