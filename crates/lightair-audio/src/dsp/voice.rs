//! Voice Clarity: the one-knob vocal chain. Noise suppression, then a gentle expander, a
//! de-esser, EQ (rumble, mud, presence, air), a compressor and a limiter, each stage scaled by
//! one Amount and switchable on its own.

use super::dynamics::{
    Compressor, CompressorParams, DeEsser, DeEsserParams, Gate, GateParams, Limiter, LimiterParams,
};
use super::eq::{Band, BandKind, Eq, EqParams};
use super::noise::{Noise, NoiseParams};
use super::{Ctx, Params, Processor};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceParams {
    /// 0 light .. 1 very strong.
    pub amount: f32,
    pub noise: bool,
    pub gate: bool,
    pub deess: bool,
    pub eq: bool,
    pub compress: bool,
    pub limit: bool,
    /// Podcast: warmer and denser, for narration (Screen Studio's voice preset).
    pub podcast: bool,
}

impl Default for VoiceParams {
    fn default() -> Self {
        Self { amount: 0.5, noise: true, gate: true, deess: true, eq: true, compress: true, limit: true, podcast: false }
    }
}

impl VoiceParams {
    fn noise(&self) -> Params {
        Params::Noise(NoiseParams { strength: 0.5 + 0.5 * self.amount, vad_threshold: 0.0 })
    }
    fn gate(&self) -> Params {
        Params::Gate(GateParams {
            threshold_db: -55.0 + 10.0 * self.amount,
            range_db: 12.0 + 18.0 * self.amount,
            ratio: 3.0,
            ..GateParams::default()
        })
    }
    fn deess(&self) -> Params {
        Params::DeEsser(DeEsserParams {
            threshold_db: -26.0 - 6.0 * self.amount,
            range_db: 4.0 + 6.0 * self.amount,
            ..DeEsserParams::default()
        })
    }
    fn eq(&self) -> Params {
        Params::Eq(self.eq_params())
    }
    fn eq_params(&self) -> EqParams {
        let a = self.amount;
        let mut bands = [Band::OFF; super::eq::BANDS];
        bands[0] = Band { on: true, kind: BandKind::HighPass(24), freq: 80.0, gain_db: 0.0, q: 0.7 };
        bands[1] = Band { on: true, kind: BandKind::Bell, freq: 300.0, gain_db: -2.0 * a, q: 1.0 };
        bands[2] = Band { on: true, kind: BandKind::Bell, freq: 3500.0, gain_db: 3.0 * a, q: 0.8 };
        bands[3] = Band { on: true, kind: BandKind::HighShelf, freq: 10_000.0, gain_db: 2.0 * a, q: 0.7 };
        if self.podcast {
            // close-mic warmth, less boxiness, presence without harshness
            bands[1] = Band { on: true, kind: BandKind::Bell, freq: 350.0, gain_db: -2.5 * a, q: 1.2 };
            bands[2] = Band { on: true, kind: BandKind::Bell, freq: 3000.0, gain_db: 2.0 * a, q: 0.9 };
            bands[4] = Band { on: true, kind: BandKind::LowShelf, freq: 140.0, gain_db: 2.5 * a, q: 0.7 };
        }
        EqParams { bands, output_db: 0.0 }
    }
    fn compress(&self) -> Params {
        // a podcast voice is held steadier
        let (t, r) = if self.podcast { (-24.0, 3.5) } else { (-20.0, 2.5) };
        Params::Compressor(CompressorParams {
            threshold_db: t - 6.0 * self.amount,
            ratio: r + 2.0 * self.amount,
            attack_ms: if self.podcast { 5.0 } else { 8.0 },
            release_ms: if self.podcast { 90.0 } else { 120.0 },
            ..CompressorParams::default()
        })
    }
    fn limit(&self) -> Params {
        Params::Limiter(LimiterParams { ceiling_db: -1.0, ..LimiterParams::default() })
    }
}

pub struct Voice {
    p: VoiceParams,
    noise: Noise,
    gate: Gate,
    deess: DeEsser,
    eq: Eq,
    comp: Compressor,
    limit: Limiter,
}

impl Voice {
    pub fn new(p: VoiceParams, sr: f32) -> Self {
        let mut v = Self {
            p,
            noise: Noise::new(NoiseParams::default(), sr),
            gate: Gate::new(GateParams::default(), sr),
            deess: DeEsser::new(DeEsserParams::default(), sr),
            eq: Eq::new(p.eq_params(), sr),
            comp: Compressor::new(CompressorParams::default(), sr),
            limit: Limiter::new(LimiterParams::default(), sr),
        };
        v.apply();
        v
    }

    fn apply(&mut self) {
        let p = self.p;
        self.noise.set(&p.noise());
        self.gate.set(&p.gate());
        self.deess.set(&p.deess());
        self.eq.set(&p.eq());
        self.comp.set(&p.compress());
        self.limit.set(&p.limit());
    }
}

impl Processor for Voice {
    fn kind(&self) -> &'static str {
        "audio.voice"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Voice(p) = params {
            self.p = *p;
            self.apply();
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], ctx: &Ctx) {
        let p = self.p;
        if p.noise {
            self.noise.process(l, r, ctx);
        }
        if p.gate {
            self.gate.process(l, r, ctx);
        }
        if p.deess {
            self.deess.process(l, r, ctx);
        }
        if p.eq {
            self.eq.process(l, r, ctx);
        }
        if p.compress {
            self.comp.process(l, r, ctx);
        }
        if p.limit {
            self.limit.process(l, r, ctx);
        }
    }
    fn latency(&self) -> usize {
        let on = |b: bool, v: usize| if b { v } else { 0 };
        on(self.p.noise, self.noise.latency()) + on(self.p.limit, self.limit.latency())
    }
    fn reduction_db(&self) -> f32 {
        let on = |b: bool, v: f32| if b { v } else { 0.0 };
        on(self.p.gate, self.gate.reduction_db()) + on(self.p.compress, self.comp.reduction_db())
    }
}
