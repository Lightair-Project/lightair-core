//! Dynamics: compressor, gate/expander, look-ahead limiter, de-esser, ducker.

use super::biquad::{Coefs, Shape, State};
use super::{Ctx, Params, Processor};

/// One-pole smoothing coefficient for a time constant.
fn coef(ms: f32, sr: f32) -> f32 {
    if ms <= 0.0 { 0.0 } else { (-1.0 / (ms * 0.001 * sr)).exp() }
}

fn to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

/// Attack/release smoothing in dB (attack while moving towards more reduction).
#[derive(Debug, Clone, Copy, Default)]
struct Ballistics {
    attack: f32,
    release: f32,
    value: f32,
}

impl Ballistics {
    fn new(attack_ms: f32, release_ms: f32, sr: f32) -> Self {
        Self { attack: coef(attack_ms, sr), release: coef(release_ms, sr), value: 0.0 }
    }
    /// A level detector (tracks −level), starting at silence rather than 0 dBFS.
    fn detector(attack_ms: f32, release_ms: f32, sr: f32) -> Self {
        Self { value: 180.0, ..Self::new(attack_ms, release_ms, sr) }
    }
    fn retime(&mut self, attack_ms: f32, release_ms: f32, sr: f32) {
        self.attack = coef(attack_ms, sr);
        self.release = coef(release_ms, sr);
    }
    /// `target` is a gain change in dB (≤ 0); moving down uses the attack.
    #[inline]
    fn tick(&mut self, target: f32) -> f32 {
        let c = if target < self.value { self.attack } else { self.release };
        self.value = target + c * (self.value - target);
        self.value
    }
}

// ---------------------------------------------------------------- compressor

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorParams {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    pub auto_makeup: bool,
    /// 0 dry .. 1 wet (parallel compression).
    pub mix: f32,
    /// High-pass on the detector (Hz; 0: off), so low end doesn't pump the compressor.
    pub sidechain_hpf: f32,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 3.0,
            knee_db: 6.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            makeup_db: 0.0,
            auto_makeup: true,
            mix: 1.0,
            sidechain_hpf: 80.0,
        }
    }
}

/// The static curve: gain change (dB, ≤ 0) for an input level.
pub fn compression_db(level_db: f32, threshold: f32, ratio: f32, knee: f32) -> f32 {
    let over = level_db - threshold;
    let slope = 1.0 / ratio.max(1.0) - 1.0;
    if knee > 0.0 && 2.0 * over.abs() <= knee {
        slope * (over + knee / 2.0).powi(2) / (2.0 * knee)
    } else if over > 0.0 {
        slope * over
    } else {
        0.0
    }
}

pub struct Compressor {
    p: CompressorParams,
    sr: f32,
    env: Ballistics,
    hpf: Coefs,
    hpf_state: [State; 2],
    reduction: f32,
}

impl Compressor {
    pub fn new(p: CompressorParams, sr: f32) -> Self {
        let mut c = Self {
            p,
            sr,
            env: Ballistics::new(p.attack_ms, p.release_ms, sr),
            hpf: Coefs::PASS,
            hpf_state: [State::default(); 2],
            reduction: 0.0,
        };
        c.update();
        c
    }
    fn update(&mut self) {
        self.env.retime(self.p.attack_ms, self.p.release_ms, self.sr);
        self.hpf = if self.p.sidechain_hpf > 0.0 {
            Coefs::new(Shape::HighPass, self.p.sidechain_hpf, std::f32::consts::FRAC_1_SQRT_2, 0.0, self.sr)
        } else {
            Coefs::PASS
        };
    }
    fn makeup(&self) -> f32 {
        let auto = if self.p.auto_makeup {
            // half the reduction a 0 dBFS signal would get: a fair guess at the loss
            -compression_db(0.0, self.p.threshold_db, self.p.ratio, self.p.knee_db) * 0.5
        } else {
            0.0
        };
        crate::db_to_gain(self.p.makeup_db + auto)
    }
}

impl Processor for Compressor {
    fn kind(&self) -> &'static str {
        "audio.compressor"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Compressor(p) = params {
            self.p = *p;
            self.update();
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let makeup = self.makeup();
        let (wet, dry) = (self.p.mix.clamp(0.0, 1.0), 1.0 - self.p.mix.clamp(0.0, 1.0));
        let mut deepest = 0f32;
        for i in 0..l.len() {
            // detector: the high-passed signal's peak, both channels linked
            let kl = self.hpf_state[0].tick(&self.hpf, l[i]);
            let kr = self.hpf_state[1].tick(&self.hpf, r[i]);
            let target = compression_db(to_db(kl.abs().max(kr.abs())), self.p.threshold_db, self.p.ratio, self.p.knee_db);
            let gr = self.env.tick(target);
            deepest = deepest.min(gr);
            let g = crate::db_to_gain(gr) * makeup * wet + dry;
            l[i] *= g;
            r[i] *= g;
        }
        self.reduction = deepest;
    }
    fn reset(&mut self) {
        self.env.value = 0.0;
        self.hpf_state = [State::default(); 2];
    }
    fn reduction_db(&self) -> f32 {
        self.reduction
    }
}

// ---------------------------------------------------------------- gate / expander

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateParams {
    pub threshold_db: f32,
    /// How far down it closes (dB, ≥ 0): 80 is a gate, 10 a gentle expander.
    pub range_db: f32,
    /// Expansion ratio below the threshold (≥ 1; large: a gate).
    pub ratio: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
    /// Hysteresis: closes this far below the threshold (dB).
    pub hysteresis_db: f32,
}

impl Default for GateParams {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            range_db: 40.0,
            ratio: 10.0,
            attack_ms: 1.0,
            hold_ms: 80.0,
            release_ms: 150.0,
            hysteresis_db: 4.0,
        }
    }
}

pub struct Gate {
    p: GateParams,
    sr: f32,
    detector: Ballistics,
    gain: Ballistics,
    open: bool,
    hold_left: usize,
    reduction: f32,
}

impl Gate {
    pub fn new(p: GateParams, sr: f32) -> Self {
        Self {
            p,
            sr,
            detector: Ballistics::detector(0.5, 30.0, sr),
            gain: Ballistics::new(p.attack_ms, p.release_ms, sr),
            open: false,
            hold_left: 0,
            reduction: 0.0,
        }
    }
}

impl Processor for Gate {
    fn kind(&self) -> &'static str {
        "audio.gate"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Gate(p) = params {
            self.p = *p;
            self.gain.retime(p.attack_ms, p.release_ms, self.sr);
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let hold = (self.p.hold_ms * 0.001 * self.sr) as usize;
        let mut deepest = 0f32;
        for i in 0..l.len() {
            // envelope (dB, follows peaks fast): the detector's value is ≤ 0 in "reduction" terms,
            // so track the level as a negative number and flip
            let level = -self.detector.tick(-to_db(l[i].abs().max(r[i].abs())));
            if level > self.p.threshold_db {
                self.open = true;
                self.hold_left = hold;
            } else if level < self.p.threshold_db - self.p.hysteresis_db {
                if self.hold_left > 0 {
                    self.hold_left -= 1;
                } else {
                    self.open = false;
                }
            }
            let target = if self.open {
                0.0
            } else {
                let below = (level - self.p.threshold_db).min(0.0);
                (below * (self.p.ratio.max(1.0) - 1.0)).max(-self.p.range_db)
            };
            // opening uses the attack: invert the ballistics' sense
            let g = -self.gain.tick(-target);
            let g = if g > 0.0 { 0.0 } else { g };
            deepest = deepest.min(g);
            let lin = crate::db_to_gain(g);
            l[i] *= lin;
            r[i] *= lin;
        }
        self.reduction = deepest;
    }
    fn reduction_db(&self) -> f32 {
        self.reduction
    }
}

// ---------------------------------------------------------------- limiter

/// Look-ahead (frames) for the limiter at any rate: 1.5 ms at 48 kHz.
const LOOKAHEAD_MAX: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterParams {
    pub ceiling_db: f32,
    pub release_ms: f32,
    /// Gain before limiting (drive it into the ceiling).
    pub input_db: f32,
}

impl Default for LimiterParams {
    fn default() -> Self {
        Self { ceiling_db: -1.0, release_ms: 80.0, input_db: 0.0 }
    }
}

/// Look-ahead brickwall: the gain is the minimum of the needed gains over the look-ahead,
/// averaged over the look-ahead and applied to the delayed audio - which can be shown never to
/// let a sample past the ceiling, with a smooth gain curve.
pub struct Limiter {
    p: LimiterParams,
    sr: f32,
    len: usize,
    delay: [[f32; LOOKAHEAD_MAX]; 2],
    need: [f32; LOOKAHEAD_MAX],
    held: [f32; LOOKAHEAD_MAX],
    pos: usize,
    sum: f64,
    env: f32,
    release: f32,
    reduction: f32,
}

impl Limiter {
    pub fn new(p: LimiterParams, sr: f32) -> Self {
        let len = ((sr * 0.0015) as usize).clamp(8, LOOKAHEAD_MAX);
        Self {
            p,
            sr,
            len,
            delay: [[0.0; LOOKAHEAD_MAX]; 2],
            need: [1.0; LOOKAHEAD_MAX],
            held: [1.0; LOOKAHEAD_MAX],
            pos: 0,
            sum: len as f64,
            env: 1.0,
            release: coef(p.release_ms, sr),
            reduction: 0.0,
        }
    }

}

impl Processor for Limiter {
    fn kind(&self) -> &'static str {
        "audio.limiter"
    }
    fn latency(&self) -> usize {
        self.len - 1
    }
    fn set(&mut self, params: &Params) {
        if let Params::Limiter(p) = params {
            self.p = *p;
            self.release = coef(p.release_ms, self.sr);
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let ceiling = crate::db_to_gain(self.p.ceiling_db);
        let input = crate::db_to_gain(self.p.input_db);
        let n = self.len;
        let mut deepest = 0f32;
        for i in 0..l.len() {
            let (xl, xr) = (l[i] * input, r[i] * input);
            let peak = xl.abs().max(xr.abs());
            let need = if peak > ceiling { ceiling / peak } else { 1.0 };
            self.need[self.pos] = need;
            // the smallest need over the look-ahead, with release towards 1
            let window_min = self.need[..n].iter().copied().fold(1.0f32, f32::min);
            self.env = window_min.min(1.0 - self.release * (1.0 - self.env));
            // box average of the held envelope over the look-ahead
            self.sum += (self.env - self.held[self.pos]) as f64;
            self.held[self.pos] = self.env;
            let g = (self.sum / n as f64) as f32;
            // the delayed sample
            let out_l = self.delay[0][self.pos];
            let out_r = self.delay[1][self.pos];
            self.delay[0][self.pos] = xl;
            self.delay[1][self.pos] = xr;
            self.pos = (self.pos + 1) % n;
            l[i] = out_l * g;
            r[i] = out_r * g;
            deepest = deepest.min(to_db(g));
        }
        self.reduction = deepest;
    }
    fn reduction_db(&self) -> f32 {
        self.reduction
    }
}

// ---------------------------------------------------------------- de-esser

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeEsserParams {
    pub freq: f32,
    pub threshold_db: f32,
    /// The most it takes off (dB, ≥ 0).
    pub range_db: f32,
    /// Only the band above `freq` is turned down (true), or the whole signal.
    pub split: bool,
    /// Hear only what it removes.
    pub listen: bool,
}

impl Default for DeEsserParams {
    fn default() -> Self {
        Self { freq: 6000.0, threshold_db: -30.0, range_db: 8.0, split: true, listen: false }
    }
}

pub struct DeEsser {
    p: DeEsserParams,
    sr: f32,
    hp: Coefs,
    detect: State,
    band: [State; 2],
    shelf: Coefs,
    shelf_state: [State; 2],
    env: Ballistics,
    reduction: f32,
}

impl DeEsser {
    pub fn new(p: DeEsserParams, sr: f32) -> Self {
        Self {
            p,
            sr,
            hp: Coefs::new(Shape::HighPass, p.freq, std::f32::consts::FRAC_1_SQRT_2, 0.0, sr),
            detect: State::default(),
            band: [State::default(); 2],
            shelf: Coefs::PASS,
            shelf_state: [State::default(); 2],
            env: Ballistics::new(1.0, 60.0, sr),
            reduction: 0.0,
        }
    }
}

impl Processor for DeEsser {
    fn kind(&self) -> &'static str {
        "audio.deesser"
    }
    fn set(&mut self, params: &Params) {
        if let Params::DeEsser(p) = params {
            self.p = *p;
            self.hp = Coefs::new(Shape::HighPass, p.freq, std::f32::consts::FRAC_1_SQRT_2, 0.0, self.sr);
        }
    }
    /// Split mode is a dynamic high shelf: from the detector's reduction, block by block, a
    /// shelf at the frequency turns the sibilant band down and leaves the voice below alone.
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let mut deepest = 0f32;
        let n = l.len();
        // detect over this block
        for i in 0..n {
            let s = self.detect.tick(&self.hp, (l[i] + r[i]) * 0.5);
            let target = compression_db(to_db(s.abs()), self.p.threshold_db, 4.0, 4.0).max(-self.p.range_db);
            deepest = deepest.min(self.env.tick(target));
        }
        let gr = self.env.value;
        if self.p.listen {
            for (ch, buf) in [l, r].into_iter().enumerate() {
                for x in buf.iter_mut() {
                    *x = self.band[ch].tick(&self.hp, *x);
                }
            }
        } else if self.p.split {
            self.shelf = Coefs::new(Shape::HighShelf, self.p.freq * 0.8, 0.7, gr, self.sr);
            for (ch, buf) in [l, r].into_iter().enumerate() {
                self.shelf_state[ch].run(&self.shelf, buf);
            }
        } else {
            let g = crate::db_to_gain(gr);
            l.iter_mut().chain(r.iter_mut()).for_each(|x| *x *= g);
        }
        self.reduction = deepest;
    }
    fn reduction_db(&self) -> f32 {
        self.reduction
    }
}

// ---------------------------------------------------------------- ducker

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckerParams {
    /// The strip whose signal ducks this one.
    pub source: crate::NodeId,
    /// How far down (dB, ≥ 0).
    pub amount_db: f32,
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
}

impl Default for DuckerParams {
    fn default() -> Self {
        Self { source: 0, amount_db: 12.0, threshold_db: -40.0, attack_ms: 30.0, hold_ms: 300.0, release_ms: 600.0 }
    }
}

pub struct Ducker {
    p: DuckerParams,
    sr: f32,
    detector: Ballistics,
    gain: Ballistics,
    hold_left: usize,
    reduction: f32,
}

impl Ducker {
    pub fn new(p: DuckerParams, sr: f32) -> Self {
        Self {
            p,
            sr,
            detector: Ballistics::detector(1.0, 50.0, sr),
            gain: Ballistics::new(p.attack_ms, p.release_ms, sr),
            hold_left: 0,
            reduction: 0.0,
        }
    }
}

impl Processor for Ducker {
    fn kind(&self) -> &'static str {
        "audio.ducker"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Ducker(p) = params {
            self.p = *p;
            self.gain.retime(p.attack_ms, p.release_ms, self.sr);
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], ctx: &Ctx) {
        let Some(key) = ctx.key else {
            self.reduction = 0.0;
            return;
        };
        let hold = (self.p.hold_ms * 0.001 * self.sr) as usize;
        let mut deepest = 0f32;
        for i in 0..l.len() {
            let level = -self.detector.tick(-to_db(key.get(i).copied().unwrap_or(0.0).abs()));
            let active = if level > self.p.threshold_db {
                self.hold_left = hold;
                true
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
                true
            } else {
                false
            };
            let g = self.gain.tick(if active { -self.p.amount_db } else { 0.0 });
            deepest = deepest.min(g);
            let lin = crate::db_to_gain(g);
            l[i] *= lin;
            r[i] *= lin;
        }
        self.reduction = deepest;
    }
    fn reduction_db(&self) -> f32 {
        self.reduction
    }
}
