//! Noise suppression with RNNoise (nnnoiseless): a recurrent network trained on speech and
//! noise, real time on the CPU. Works in 10 ms frames at 48 kHz, so it adds 10 ms of delay.

use nnnoiseless::DenoiseState;

use super::{Ctx, Params, Processor};

const FRAME: usize = DenoiseState::FRAME_SIZE;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseParams {
    /// 0 (off) .. 1 (fully denoised).
    pub strength: f32,
    /// Below this voice probability, turn down further (0: off). Cuts what isn't speech.
    pub vad_threshold: f32,
}

impl Default for NoiseParams {
    fn default() -> Self {
        Self { strength: 1.0, vad_threshold: 0.0 }
    }
}

pub struct Noise {
    p: NoiseParams,
    active: bool,
    states: [Box<DenoiseState<'static>>; 2],
    inbuf: [[f32; FRAME]; 2],
    outbuf: [[f32; FRAME]; 2],
    dry: [[f32; FRAME]; 2],
    fill: usize,
    vad_gain: f32,
}

impl Noise {
    pub fn new(p: NoiseParams, sr: f32) -> Self {
        Self {
            p,
            active: (sr - 48_000.0).abs() < 1.0, // RNNoise is trained at 48 kHz
            states: [DenoiseState::new(), DenoiseState::new()],
            inbuf: [[0.0; FRAME]; 2],
            outbuf: [[0.0; FRAME]; 2],
            dry: [[0.0; FRAME]; 2],
            fill: 0,
            vad_gain: 1.0,
        }
    }
}

impl Processor for Noise {
    fn kind(&self) -> &'static str {
        "audio.noise"
    }
    fn latency(&self) -> usize {
        if self.active { FRAME } else { 0 }
    }
    fn set(&mut self, params: &Params) {
        if let Params::Noise(p) = params {
            self.p = *p;
        }
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32], _: &Ctx) {
        if !self.active {
            return;
        }
        let wet = self.p.strength.clamp(0.0, 1.0);
        for i in 0..l.len() {
            // swap the new sample in, the delayed processed one out
            let k = self.fill;
            self.inbuf[0][k] = l[i] * 32768.0;
            self.inbuf[1][k] = r[i] * 32768.0;
            let out_l = self.outbuf[0][k] / 32768.0;
            let out_r = self.outbuf[1][k] / 32768.0;
            let (dl, dr) = (self.dry[0][k], self.dry[1][k]);
            self.dry[0][k] = l[i];
            self.dry[1][k] = r[i];
            let g = self.vad_gain;
            l[i] = (out_l * wet + dl * (1.0 - wet)) * g;
            r[i] = (out_r * wet + dr * (1.0 - wet)) * g;
            self.fill += 1;
            if self.fill == FRAME {
                self.fill = 0;
                let mut vad = 0f32;
                for ch in 0..2 {
                    vad = vad.max(self.states[ch].process_frame(&mut self.outbuf[ch], &self.inbuf[ch]));
                }
                let target = if self.p.vad_threshold > 0.0 && vad < self.p.vad_threshold { 0.1 } else { 1.0 };
                self.vad_gain += (target - self.vad_gain) * 0.5;
            }
        }
    }
}
