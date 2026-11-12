//! Biquad filters (RBJ Audio EQ Cookbook), transposed direct form II.

use std::f32::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    LowPass,
    HighPass,
    BandPass,
    Notch,
    Bell,
    LowShelf,
    HighShelf,
}

/// Normalised coefficients (a0 = 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Coefs {
    pub const PASS: Coefs = Coefs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    pub fn new(shape: Shape, freq: f32, q: f32, gain_db: f32, sample_rate: f32) -> Self {
        let f = freq.clamp(10.0, sample_rate * 0.49);
        let w = 2.0 * PI * f / sample_rate;
        let (sw, cw) = w.sin_cos();
        let q = q.max(0.05);
        let alpha = sw / (2.0 * q);
        let a = 10f32.powf(gain_db / 40.0);
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::LowPass => ((1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            Shape::HighPass => ((1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            Shape::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            Shape::Notch => (1.0, -2.0 * cw, 1.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
            Shape::Bell => (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a),
            Shape::LowShelf => {
                let k = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cw + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                    a * ((a + 1.0) - (a - 1.0) * cw - k),
                    (a + 1.0) + (a - 1.0) * cw + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                    (a + 1.0) + (a - 1.0) * cw - k,
                )
            }
            Shape::HighShelf => {
                let k = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cw + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                    a * ((a + 1.0) + (a - 1.0) * cw - k),
                    (a + 1.0) - (a - 1.0) * cw + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * cw),
                    (a + 1.0) - (a - 1.0) * cw - k,
                )
            }
        };
        Coefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// The magnitude response at `freq`, in dB (for the EQ curve).
    pub fn response_db(&self, freq: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * PI * freq / sample_rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let (nr, ni) = (self.b0 + self.b1 * c1 + self.b2 * c2, -(self.b1 * s1 + self.b2 * s2));
        let (dr, di) = (1.0 + self.a1 * c1 + self.a2 * c2, -(self.a1 * s1 + self.a2 * s2));
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di).max(1e-30)).log10()
    }
}

/// One channel's filter state.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    z1: f32,
    z2: f32,
}

impl State {
    #[inline]
    pub fn tick(&mut self, c: &Coefs, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    pub fn run(&mut self, c: &Coefs, buf: &mut [f32]) {
        for x in buf.iter_mut() {
            *x = self.tick(c, *x);
        }
    }

    pub fn reset(&mut self) {
        *self = State::default();
    }
}

/// Q of each section of a Butterworth filter of `order` (2, 4, 6, 8) as cascaded biquads.
pub fn butterworth_qs(order: usize) -> &'static [f32] {
    match order {
        0..=2 => &[std::f32::consts::FRAC_1_SQRT_2],
        3 | 4 => &[0.541_196_1, 1.306_563],
        5 | 6 => &[0.517_638_1, std::f32::consts::FRAC_1_SQRT_2, 1.931_851_6],
        _ => &[0.509_795_6, 0.601_344_9, 0.899_976_2, 2.562_915_4],
    }
}
