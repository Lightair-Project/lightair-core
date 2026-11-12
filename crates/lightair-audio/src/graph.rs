//! The compiled mixer: strips in processing order with their buffers and effects.

use std::collections::HashMap;
use std::sync::Arc;

use crate::dsp::{Ctx, Params, Processor};
use crate::meter::{LoudnessMeter, Meter};
use crate::smooth::Smoothed;
use crate::spec::{Channels, Dest, EffectSpec, GraphSpec, NodeId, Role, Warning};
use crate::{MAX_BLOCK, db_to_gain};

/// One input's audio for a block: planar channels, each at least `frames` long.
pub struct InputBlock<'a> {
    pub id: NodeId,
    pub channels: &'a [&'a [f32]],
}

/// Where to listen to a strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tap {
    /// After trim and inserts, before the fader (what multitrack recording usually wants).
    PreFader,
    /// After the fader and pan (what the strip sends on).
    PostFader,
}

struct Stereo {
    l: Box<[f32]>,
    r: Box<[f32]>,
}

impl Stereo {
    fn new() -> Self {
        Self { l: vec![0.0; MAX_BLOCK].into_boxed_slice(), r: vec![0.0; MAX_BLOCK].into_boxed_slice() }
    }
    fn clear(&mut self, n: usize) {
        self.l[..n].fill(0.0);
        self.r[..n].fill(0.0);
    }
}

fn add_scaled(dst: &mut Stereo, src_l: &[f32], src_r: &[f32], gain: &[f32]) {
    let n = src_l.len();
    for i in 0..n {
        dst.l[i] += src_l[i] * gain[i];
        dst.r[i] += src_r[i] * gain[i];
    }
}

struct Slot {
    id: NodeId,
    enabled: bool,
    params: Params,
    proc: Box<dyn Processor>,
}

impl Slot {
    fn new(e: &EffectSpec, sample_rate: f32) -> Self {
        Self { id: e.id, enabled: e.enabled, params: e.params.clone(), proc: e.params.make(sample_rate) }
    }
}

#[derive(Clone, Copy)]
enum Out {
    Program,
    Bus(usize),
    Nowhere,
}

struct SendRt {
    mix: usize,
    mix_id: NodeId,
    level: Smoothed,
    pre: bool,
}

enum Source {
    Track { input: NodeId, channels: Channels },
    Bus(usize),
}

struct StripRt {
    id: NodeId,
    source: Source,
    trim: Smoothed,
    mono: bool,
    inserts: Vec<Slot>,
    fader: Smoothed,
    pan_l: Smoothed,
    pan_r: Smoothed,
    out: Out,
    sends: Vec<SendRt>,
    pre: Stereo,
    post: Stereo,
    meter: Arc<Meter>,
}

struct MixRt {
    id: NodeId,
    inserts: Vec<Slot>,
    fader: Smoothed,
    sum: Stereo,
    meter: Arc<Meter>,
    loudness: LoudnessMeter,
}

/// The mixer, ready to process. Made by [`crate::Engine`].
pub struct Graph {
    sample_rate: f32,
    strips: Vec<StripRt>,
    bus_sums: Vec<Stereo>,
    mixes: Vec<MixRt>,
    program: MixRt,
    scratch: [Box<[f32]>; 4],
    frames: usize,
    pub(crate) warnings: Vec<Warning>,
}

/// Balance pan: unity on both sides at the centre; the far side fades on a cosine.
fn pan_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    let fade = |x: f32| (x * std::f32::consts::FRAC_PI_2).cos();
    (if p > 0.0 { fade(p) } else { 1.0 }, if p < 0.0 { fade(-p) } else { 1.0 })
}

impl Graph {
    /// Compiles `spec` (off the audio thread). `meter` gives each node its meter (kept across
    /// compiles by the engine, so the UI's handles stay valid).
    pub(crate) fn compile(
        spec: &GraphSpec,
        sample_rate: u32,
        mut meter: impl FnMut(NodeId) -> Arc<Meter>,
    ) -> Self {
        let sr = sample_rate as f32;
        let ramp = (sr * 0.01) as usize; // 10 ms
        let mut warnings = Vec::new();
        let buses: HashMap<NodeId, usize> = spec
            .strips
            .iter()
            .filter(|s| s.role == Role::Bus)
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect();
        let mix_index: HashMap<NodeId, usize> = spec.mixes.iter().enumerate().map(|(i, m)| (m.id, i)).collect();

        // A strip's destination, following buses: a loop or a missing bus goes to Program.
        let dest_of = |id: NodeId, d: Dest| -> Dest {
            let Dest::Bus(b) = d else { return d };
            if !buses.contains_key(&b) || b == id {
                return Dest::Program;
            }
            let mut seen = vec![id];
            let mut cur = b;
            loop {
                if seen.contains(&cur) {
                    return Dest::Program;
                }
                seen.push(cur);
                match spec.strips.iter().find(|s| s.id == cur).map(|s| s.dest) {
                    Some(Dest::Bus(next)) if buses.contains_key(&next) => cur = next,
                    _ => return d,
                }
            }
        };
        // How many buses down a strip's output passes before Program: buses feeding others
        // are processed before them.
        let depth = |id: NodeId| -> usize {
            let mut n = 0;
            let mut cur = id;
            while let Some(Dest::Bus(b)) = spec.strips.iter().find(|s| s.id == cur).map(|s| dest_of(s.id, s.dest)) {
                n += 1;
                cur = b;
                if n > spec.strips.len() {
                    break;
                }
            }
            n
        };
        let mut order: Vec<usize> = (0..spec.strips.len()).filter(|&i| spec.strips[i].role != Role::Bus).collect();
        let mut bus_order: Vec<usize> = (0..spec.strips.len()).filter(|&i| spec.strips[i].role == Role::Bus).collect();
        bus_order.sort_by_key(|&i| std::cmp::Reverse(depth(spec.strips[i].id)));
        order.extend(bus_order);

        let strips = order
            .into_iter()
            .map(|i| {
                let s = &spec.strips[i];
                let dest = dest_of(s.id, s.dest);
                if dest != s.dest {
                    warnings.push(Warning::BadRoute { strip: s.id });
                }
                let (gl, gr) = pan_gains(s.pan);
                let sends = s
                    .sends
                    .iter()
                    .filter_map(|send| match mix_index.get(&send.mix) {
                        Some(&mix) => Some(SendRt {
                            mix,
                            mix_id: send.mix,
                            level: Smoothed::new(db_to_gain(send.level_db), ramp),
                            pre: send.pre_fader,
                        }),
                        None => {
                            warnings.push(Warning::BadSend { strip: s.id, mix: send.mix });
                            None
                        }
                    })
                    .collect();
                StripRt {
                    id: s.id,
                    source: match &s.role {
                        Role::Track { input, channels } => Source::Track { input: *input, channels: *channels },
                        Role::Bus => Source::Bus(buses[&s.id]),
                    },
                    trim: Smoothed::new(db_to_gain(s.trim_db) * if s.invert { -1.0 } else { 1.0 }, ramp),
                    mono: s.mono,
                    inserts: s.inserts.iter().map(|e| Slot::new(e, sr)).collect(),
                    fader: Smoothed::new(if s.mute { 0.0 } else { db_to_gain(s.gain_db) }, ramp),
                    pan_l: Smoothed::new(gl, ramp),
                    pan_r: Smoothed::new(gr, ramp),
                    out: match dest {
                        Dest::Program => Out::Program,
                        Dest::Bus(b) => Out::Bus(buses[&b]),
                        Dest::Nowhere => Out::Nowhere,
                    },
                    sends,
                    pre: Stereo::new(),
                    post: Stereo::new(),
                    meter: meter(s.id),
                }
            })
            .collect();
        let mix_rt = |id: NodeId, inserts: &[EffectSpec], gain_db: f32, mute: bool, meter: Arc<Meter>| MixRt {
            id,
            inserts: inserts.iter().map(|e| Slot::new(e, sr)).collect(),
            fader: Smoothed::new(if mute { 0.0 } else { db_to_gain(gain_db) }, ramp),
            sum: Stereo::new(),
            meter,
            loudness: LoudnessMeter::new(sample_rate),
        };
        let mixes = spec.mixes.iter().map(|m| mix_rt(m.id, &m.inserts, m.gain_db, m.mute, meter(m.id))).collect();
        let program =
            mix_rt(crate::PROGRAM, &spec.program.inserts, spec.program.gain_db, spec.program.mute, meter(crate::PROGRAM));
        Self {
            sample_rate: sr,
            strips,
            bus_sums: (0..buses.len()).map(|_| Stereo::new()).collect(),
            mixes,
            program,
            scratch: std::array::from_fn(|_| vec![0.0; MAX_BLOCK].into_boxed_slice()),
            frames: 0,
            warnings,
        }
    }

    /// Takes over a running graph's state: effect history (same id and kind), levels mid-ramp,
    /// loudness measurement. Audio thread: no allocation.
    pub(crate) fn adopt(&mut self, old: &mut Graph) {
        fn slots(new: &mut [Slot], old: &mut [Slot]) {
            for s in new.iter_mut() {
                if let Some(o) = old.iter_mut().find(|o| o.id == s.id && o.proc.kind() == s.proc.kind()) {
                    std::mem::swap(&mut s.proc, &mut o.proc);
                    s.proc.set(&s.params);
                }
            }
        }
        for s in &mut self.strips {
            let Some(o) = old.strips.iter_mut().find(|o| o.id == s.id) else { continue };
            s.trim.continue_from(&o.trim);
            s.fader.continue_from(&o.fader);
            s.pan_l.continue_from(&o.pan_l);
            s.pan_r.continue_from(&o.pan_r);
            for send in &mut s.sends {
                if let Some(os) = o.sends.iter().find(|os| os.mix_id == send.mix_id) {
                    send.level.continue_from(&os.level);
                }
            }
            slots(&mut s.inserts, &mut o.inserts);
        }
        let mix = |new: &mut MixRt, old: &mut MixRt| {
            new.fader.continue_from(&old.fader);
            slots(&mut new.inserts, &mut old.inserts);
            std::mem::swap(&mut new.loudness, &mut old.loudness);
        };
        mix(&mut self.program, &mut old.program);
        for m in &mut self.mixes {
            if let Some(o) = old.mixes.iter_mut().find(|o| o.id == m.id) {
                mix(m, o);
            }
        }
    }

    /// Mixes one block of `frames` (≤ [`MAX_BLOCK`]) from `inputs`. Real-time safe.
    pub fn process(&mut self, inputs: &[InputBlock], frames: usize) {
        let n = frames.min(MAX_BLOCK);
        self.frames = n;
        let ctx = Ctx { sample_rate: self.sample_rate, key: None };
        self.program.sum.clear(n);
        for m in &mut self.mixes {
            m.sum.clear(n);
        }
        for b in &mut self.bus_sums {
            b.clear(n);
        }
        let [g0, g1, g2, g3] = &mut self.scratch;
        for s in &mut self.strips {
            // source
            match &s.source {
                Source::Track { input, channels } => {
                    let block = inputs.iter().find(|b| b.id == *input);
                    let ch = |c: u16| block.and_then(|b| b.channels.get(c as usize)).map(|c| &c[..n]);
                    let (l, r) = match *channels {
                        Channels::Mono(c) => (ch(c), ch(c)),
                        Channels::Stereo(a, b) => (ch(a), ch(b)),
                    };
                    match l {
                        Some(l) => s.pre.l[..n].copy_from_slice(l),
                        None => s.pre.l[..n].fill(0.0),
                    }
                    match r {
                        Some(r) => s.pre.r[..n].copy_from_slice(r),
                        None => s.pre.r[..n].fill(0.0),
                    }
                }
                Source::Bus(b) => {
                    s.pre.l[..n].copy_from_slice(&self.bus_sums[*b].l[..n]);
                    s.pre.r[..n].copy_from_slice(&self.bus_sums[*b].r[..n]);
                }
            }
            // trim (and polarity), mono
            s.trim.fill(&mut g0[..n]);
            for i in 0..n {
                s.pre.l[i] *= g0[i];
                s.pre.r[i] *= g0[i];
            }
            if s.mono {
                for i in 0..n {
                    let m = (s.pre.l[i] + s.pre.r[i]) * 0.5;
                    (s.pre.l[i], s.pre.r[i]) = (m, m);
                }
            }
            // inserts
            let mut reduction = 0f32;
            for slot in s.inserts.iter_mut().filter(|e| e.enabled) {
                slot.proc.process(&mut s.pre.l[..n], &mut s.pre.r[..n], &ctx);
                reduction += slot.proc.reduction_db();
            }
            // fader and pan
            s.fader.fill(&mut g0[..n]);
            s.pan_l.fill(&mut g1[..n]);
            s.pan_r.fill(&mut g2[..n]);
            for i in 0..n {
                s.post.l[i] = s.pre.l[i] * g0[i] * g1[i];
                s.post.r[i] = s.pre.r[i] * g0[i] * g2[i];
            }
            s.meter.feed(&s.post.l[..n], &s.post.r[..n]);
            s.meter.feed_reduction(reduction);
            // route
            g3[..n].fill(1.0);
            match s.out {
                Out::Program => add_scaled(&mut self.program.sum, &s.post.l[..n], &s.post.r[..n], &g3[..n]),
                Out::Bus(b) => add_scaled(&mut self.bus_sums[b], &s.post.l[..n], &s.post.r[..n], &g3[..n]),
                Out::Nowhere => {}
            }
            for send in &mut s.sends {
                send.level.fill(&mut g1[..n]);
                let src = if send.pre { &s.pre } else { &s.post };
                add_scaled(&mut self.mixes[send.mix].sum, &src.l[..n], &src.r[..n], &g1[..n]);
            }
        }
        let master = |m: &mut MixRt, g: &mut [f32]| {
            for slot in m.inserts.iter_mut().filter(|e| e.enabled) {
                slot.proc.process(&mut m.sum.l[..n], &mut m.sum.r[..n], &ctx);
            }
            m.fader.fill(&mut g[..n]);
            for (i, g) in g[..n].iter().enumerate() {
                m.sum.l[i] *= g;
                m.sum.r[i] *= g;
            }
            m.meter.feed(&m.sum.l[..n], &m.sum.r[..n]);
            m.loudness.feed(&m.sum.l[..n], &m.sum.r[..n], &m.meter);
        };
        master(&mut self.program, g0);
        for m in &mut self.mixes {
            master(m, g0);
        }
    }

    /// The program (master) output of the last block.
    pub fn program(&self) -> (&[f32], &[f32]) {
        (&self.program.sum.l[..self.frames], &self.program.sum.r[..self.frames])
    }

    /// A mix's output of the last block.
    pub fn mix(&self, id: NodeId) -> Option<(&[f32], &[f32])> {
        let m = self.mixes.iter().find(|m| m.id == id)?;
        Some((&m.sum.l[..self.frames], &m.sum.r[..self.frames]))
    }

    /// A strip's signal in the last block.
    pub fn tap(&self, id: NodeId, tap: Tap) -> Option<(&[f32], &[f32])> {
        let s = self.strips.iter().find(|s| s.id == id)?;
        let b = if tap == Tap::PreFader { &s.pre } else { &s.post };
        Some((&b.l[..self.frames], &b.r[..self.frames]))
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }
}
