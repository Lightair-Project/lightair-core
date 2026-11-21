//! Parametric EQ: eight bands of high/low pass (12–48 dB/oct), shelves, bells and notches.

use super::biquad::{Coefs, Shape, State, butterworth_qs};
use super::{Ctx, Params, Processor};

pub const BANDS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BandKind {
    /// Slope in dB/octave: 12, 24, 36 or 48.
    HighPass(u8),
    LowPass(u8),
    LowShelf,
    HighShelf,
    Bell,
    Notch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub on: bool,
    pub kind: BandKind,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl Band {
    pub const OFF: Band = Band { on: false, kind: BandKind::Bell, freq: 1000.0, gain_db: 0.0, q: 1.0 };

    /// Its sections' coefficients (up to four), the rest passing.
    fn coefs(&self, sr: f32) -> [Coefs; 4] {
        let mut out = [Coefs::PASS; 4];
        if !self.on {
            return out;
        }
        match self.kind {
            BandKind::HighPass(slope) | BandKind::LowPass(slope) => {
                let shape = if matches!(self.kind, BandKind::HighPass(_)) { Shape::HighPass } else { Shape::LowPass };
                for (i, q) in butterworth_qs((slope as usize / 6).clamp(2, 8)).iter().enumerate() {
                    out[i] = Coefs::new(shape, self.freq, *q, 0.0, sr);
                }
            }
            BandKind::LowShelf => out[0] = Coefs::new(Shape::LowShelf, self.freq, self.q, self.gain_db, sr),
            BandKind::HighShelf => out[0] = Coefs::new(Shape::HighShelf, self.freq, self.q, self.gain_db, sr),
            BandKind::Bell => out[0] = Coefs::new(Shape::Bell, self.freq, self.q, self.gain_db, sr),
            BandKind::Notch => out[0] = Coefs::new(Shape::Notch, self.freq, self.q, 0.0, sr),
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqParams {
    pub bands: [Band; BANDS],
    /// Output trim (dB).
    pub output_db: f32,
}

impl Default for EqParams {
    fn default() -> Self {
        Self { bands: [Band::OFF; BANDS], output_db: 0.0 }
    }
}

impl EqParams {
    /// The whole EQ's response at `freq` (dB): what the curve in the editor draws.
    pub fn response_db(&self, freq: f32, sample_rate: f32) -> f32 {
        self.bands.iter().flat_map(|b| b.coefs(sample_rate)).map(|c| c.response_db(freq, sample_rate)).sum::<f32>()
            + self.output_db
    }
}

pub struct Eq {
    p: EqParams,
    sr: f32,
    coefs: [[Coefs; 4]; BANDS],
    state: [[[State; 4]; BANDS]; 2],
}

impl Eq {
    pub fn new(p: EqParams, sr: f32) -> Self {
        let mut eq = Self { p, sr, coefs: [[Coefs::PASS; 4]; BANDS], state: [[[State::default(); 4]; BANDS]; 2] };
        eq.update();
        eq
    }

    fn update(&mut self) {
        for (i, b) in self.p.bands.iter().enumerate() {
            self.coefs[i] = b.coefs(self.sr);
        }
    }
}

impl Processor for Eq {
    fn kind(&self) -> &'static str {
        "audio.eq"
    }

    fn set(&mut self, params: &Params) {
        if let Params::Eq(p) = params {
            self.p = *p;
            self.update();
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let out = crate::db_to_gain(self.p.output_db);
        for (ch, buf) in [l, r].into_iter().enumerate() {
            for (b, band) in self.p.bands.iter().enumerate() {
                if !band.on {
                    continue;
                }
                for s in 0..4 {
                    if self.coefs[b][s] != Coefs::PASS {
                        self.state[ch][b][s].run(&self.coefs[b][s], buf);
                    }
                }
            }
            if out != 1.0 {
                buf.iter_mut().for_each(|x| *x *= out);
            }
        }
    }

    fn reset(&mut self) {
        self.state = [[[State::default(); 4]; BANDS]; 2];
    }
}
