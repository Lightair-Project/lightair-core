//! Space and time: reverb (Freeverb-style), delay, and channel tools.

use super::biquad::{Coefs, Shape, State};
use super::{Ctx, Params, Processor};

// ---------------------------------------------------------------- reverb

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReverbParams {
    /// 0 small room .. 1 large hall.
    pub size: f32,
    /// 0 bright .. 1 dark.
    pub damping: f32,
    pub pre_delay_ms: f32,
    /// 0 mono .. 1 wide.
    pub width: f32,
    /// 0 dry .. 1 wet.
    pub mix: f32,
}

impl Default for ReverbParams {
    fn default() -> Self {
        Self { size: 0.5, damping: 0.5, pre_delay_ms: 15.0, width: 1.0, mix: 0.25 }
    }
}

const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;

struct Comb {
    buf: Box<[f32]>,
    pos: usize,
    store: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)].into_boxed_slice(), pos: 0, store: 0.0 }
    }
    #[inline]
    fn tick(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.pos];
        self.store = y * (1.0 - damp) + self.store * damp;
        self.buf[self.pos] = x + self.store * feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        y
    }
}

struct Allpass {
    buf: Box<[f32]>,
    pos: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)].into_boxed_slice(), pos: 0 }
    }
    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let b = self.buf[self.pos];
        self.buf[self.pos] = x + b * 0.5;
        self.pos = (self.pos + 1) % self.buf.len();
        b - x
    }
}

pub struct Reverb {
    p: ReverbParams,
    sr: f32,
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<Allpass>; 2],
    pre: Box<[f32]>,
    pre_pos: usize,
}

impl Reverb {
    pub fn new(p: ReverbParams, sr: f32) -> Self {
        let k = sr / 44_100.0;
        let scaled = |n: usize, extra: usize| ((n + extra) as f32 * k) as usize;
        Self {
            p,
            sr,
            combs: [0, SPREAD].map(|s| COMBS.iter().map(|&n| Comb::new(scaled(n, s))).collect()),
            allpasses: [0, SPREAD].map(|s| ALLPASSES.iter().map(|&n| Allpass::new(scaled(n, s))).collect()),
            pre: vec![0.0; (sr * 0.25) as usize + 1].into_boxed_slice(),
            pre_pos: 0,
        }
    }
}

impl Processor for Reverb {
    fn kind(&self) -> &'static str {
        "audio.reverb"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Reverb(p) = params {
            self.p = *p;
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let feedback = 0.7 + 0.28 * self.p.size.clamp(0.0, 1.0);
        let damp = 0.4 * self.p.damping.clamp(0.0, 1.0);
        let mix = self.p.mix.clamp(0.0, 1.0);
        let w = self.p.width.clamp(0.0, 1.0);
        let (w1, w2) = (w / 2.0 + 0.5, (1.0 - w) / 2.0);
        let pre_len = ((self.p.pre_delay_ms.clamp(0.0, 240.0) * 0.001 * self.sr) as usize).min(self.pre.len() - 1);
        let n = self.pre.len();
        for i in 0..l.len() {
            // pre-delay on the (mono) send into the tank
            self.pre[self.pre_pos] = (l[i] + r[i]) * 0.015;
            let input = self.pre[(self.pre_pos + n - pre_len) % n];
            self.pre_pos = (self.pre_pos + 1) % n;
            let mut out = [0f32; 2];
            for ((o, combs), allpasses) in out.iter_mut().zip(&mut self.combs).zip(&mut self.allpasses) {
                let mut acc = 0f32;
                for c in combs.iter_mut() {
                    acc += c.tick(input, feedback, damp);
                }
                for a in allpasses.iter_mut() {
                    acc = a.tick(acc);
                }
                *o = acc;
            }
            let (wl, wr) = (out[0] * w1 + out[1] * w2, out[1] * w1 + out[0] * w2);
            l[i] = l[i] * (1.0 - mix) + wl * mix * 3.0;
            r[i] = r[i] * (1.0 - mix) + wr * mix * 3.0;
        }
    }
}

// ---------------------------------------------------------------- delay

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelayParams {
    pub time_ms: f32,
    /// 0 .. 0.95
    pub feedback: f32,
    /// Low-pass on the repeats (Hz).
    pub tone_hz: f32,
    pub ping_pong: bool,
    pub mix: f32,
}

impl Default for DelayParams {
    fn default() -> Self {
        Self { time_ms: 350.0, feedback: 0.35, tone_hz: 6000.0, ping_pong: false, mix: 0.25 }
    }
}

pub struct Delay {
    p: DelayParams,
    sr: f32,
    buf: [Box<[f32]>; 2],
    pos: usize,
    /// The read distance (frames), gliding to the set time so changes don't click.
    current: f32,
    tone: Coefs,
    tone_state: [State; 2],
}

impl Delay {
    pub fn new(p: DelayParams, sr: f32) -> Self {
        let len = (sr * 2.1) as usize;
        Self {
            p,
            sr,
            buf: [vec![0.0; len].into_boxed_slice(), vec![0.0; len].into_boxed_slice()],
            pos: 0,
            current: p.time_ms * 0.001 * sr,
            tone: Coefs::new(Shape::LowPass, p.tone_hz, std::f32::consts::FRAC_1_SQRT_2, 0.0, sr),
            tone_state: [State::default(); 2],
        }
    }
}

impl Processor for Delay {
    fn kind(&self) -> &'static str {
        "audio.delay"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Delay(p) = params {
            self.p = *p;
            self.tone = Coefs::new(Shape::LowPass, p.tone_hz, std::f32::consts::FRAC_1_SQRT_2, 0.0, self.sr);
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let n = self.buf[0].len();
        let target = (self.p.time_ms.clamp(1.0, 2000.0) * 0.001 * self.sr).min((n - 2) as f32);
        let fb = self.p.feedback.clamp(0.0, 0.95);
        let mix = self.p.mix.clamp(0.0, 1.0);
        for i in 0..l.len() {
            self.current += (target - self.current) * 0.0005;
            let d = self.current;
            let read = |buf: &[f32], pos: usize| {
                let p = (pos as f32 + n as f32 - d) % n as f32;
                let (i0, f) = (p.floor() as usize, p.fract());
                buf[i0 % n] * (1.0 - f) + buf[(i0 + 1) % n] * f
            };
            let (yl, yr) = (read(&self.buf[0], self.pos), read(&self.buf[1], self.pos));
            let (tl, tr) = (self.tone_state[0].tick(&self.tone, yl), self.tone_state[1].tick(&self.tone, yr));
            // ping-pong: the input enters on the left, the repeats cross from side to side
            let (wl, wr) = if self.p.ping_pong {
                ((l[i] + r[i]) * 0.5 + tr * fb, tl * fb)
            } else {
                (l[i] + tl * fb, r[i] + tr * fb)
            };
            self.buf[0][self.pos] = wl;
            self.buf[1][self.pos] = wr;
            self.pos = (self.pos + 1) % n;
            l[i] = l[i] * (1.0 - mix) + yl * mix;
            r[i] = r[i] * (1.0 - mix) + yr * mix;
        }
    }
}

// ---------------------------------------------------------------- channel tools

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelParams {
    /// 0 mono .. 1 as is .. 2 extra wide.
    pub width: f32,
    /// Below this frequency, mono (Hz; 0: off) - tight low end.
    pub bass_mono_hz: f32,
}

impl Default for ChannelParams {
    fn default() -> Self {
        Self { width: 1.0, bass_mono_hz: 0.0 }
    }
}

pub struct ChannelTools {
    p: ChannelParams,
    sr: f32,
    lp: Coefs,
    lp_state: [[State; 2]; 2],
}

impl ChannelTools {
    pub fn new(p: ChannelParams, sr: f32) -> Self {
        let mut c = Self { p, sr, lp: Coefs::PASS, lp_state: [[State::default(); 2]; 2] };
        c.update();
        c
    }
    fn update(&mut self) {
        if self.p.bass_mono_hz > 0.0 {
            self.lp = Coefs::new(Shape::LowPass, self.p.bass_mono_hz, std::f32::consts::FRAC_1_SQRT_2, 0.0, self.sr);
        }
    }
}

impl Processor for ChannelTools {
    fn kind(&self) -> &'static str {
        "audio.channel"
    }
    fn set(&mut self, params: &Params) {
        if let Params::Channel(p) = params {
            self.p = *p;
            self.update();
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        let w = self.p.width.clamp(0.0, 2.0);
        for i in 0..l.len() {
            let (mut a, mut b) = (l[i], r[i]);
            if self.p.bass_mono_hz > 0.0 {
                // Linkwitz-Riley-ish split: two passes of the low-pass
                let first = self.lp_state[0][0].tick(&self.lp, a);
                let la = self.lp_state[0][1].tick(&self.lp, first);
                let first = self.lp_state[1][0].tick(&self.lp, b);
                let lb = self.lp_state[1][1].tick(&self.lp, first);
                let low = (la + lb) * 0.5;
                a = a - la + low;
                b = b - lb + low;
            }
            let (m, s) = ((a + b) * 0.5, (a - b) * 0.5 * w);
            l[i] = m + s;
            r[i] = m - s;
        }
    }
}
