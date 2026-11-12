//! Gain: trim, polarity, and channel tools (mono, swap).

use super::{Ctx, Params, Processor};
use crate::smooth::Smoothed;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainParams {
    pub db: f32,
    pub invert: bool,
    pub mono: bool,
    pub swap: bool,
}

impl Default for GainParams {
    fn default() -> Self {
        Self { db: 0.0, invert: false, mono: false, swap: false }
    }
}

pub struct Gain {
    p: GainParams,
    gain: Smoothed,
    scratch: Box<[f32; crate::MAX_BLOCK]>,
}

impl Gain {
    pub fn new(p: GainParams, sample_rate: f32) -> Self {
        let ramp = (sample_rate * 0.01) as usize;
        Self { p, gain: Smoothed::new(Self::linear(&p), ramp), scratch: Box::new([0.0; crate::MAX_BLOCK]) }
    }

    fn linear(p: &GainParams) -> f32 {
        crate::db_to_gain(p.db) * if p.invert { -1.0 } else { 1.0 }
    }
}

impl Processor for Gain {
    fn kind(&self) -> &'static str {
        "audio.gain"
    }

    fn set(&mut self, params: &Params) {
        #[allow(irrefutable_let_patterns)] // more kinds come
        let Params::Gain(p) = params else { return };
        self.p = *p;
        self.gain.set(Self::linear(p));
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        if self.p.swap {
            l.swap_with_slice(r);
        }
        if self.p.mono {
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                let m = (*a + *b) * 0.5;
                (*a, *b) = (m, m);
            }
        }
        let n = l.len();
        self.gain.fill(&mut self.scratch[..n]);
        for i in 0..n {
            l[i] *= self.scratch[i];
            r[i] *= self.scratch[i];
        }
    }
}
