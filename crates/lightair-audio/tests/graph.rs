//! The mixer on exact signals: levels, pan law, routing, sends, smoothing, swaps, loudness.

use lightair_audio::dsp::{GainParams, Params};
use lightair_audio::{Channels, Dest, EffectSpec, Engine, GraphSpec, InputBlock, Mix, PROGRAM, Send, Strip, Tap, Warning};

const SR: u32 = 48_000;
const MIC: u128 = 100;
const BLOCK: usize = 256;

fn dc(v: f32) -> Vec<f32> {
    vec![v; BLOCK]
}

/// Runs `blocks` blocks of a constant input on channel 0 (and 0.25 on channel 1).
fn run(spec: GraphSpec, blocks: usize) -> (Engine, lightair_audio::Renderer) {
    let (engine, mut r) = Engine::new(spec, SR);
    let (a, b) = (dc(0.5), dc(0.25));
    let chans: [&[f32]; 2] = [&a, &b];
    for _ in 0..blocks {
        r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    }
    (engine, r)
}

fn last(x: &[f32]) -> f32 {
    *x.last().unwrap()
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

#[test]
fn a_mono_track_at_the_centre_is_unity_on_both_sides() {
    let spec = GraphSpec { strips: vec![Strip::track(1, MIC, Channels::Mono(0))], ..Default::default() };
    let (_, r) = run(spec, 2);
    let (l, rr) = r.graph().program();
    assert!(close(last(l), 0.5) && close(last(rr), 0.5));
}

#[test]
fn fader_pan_and_stereo_channels() {
    let mut t = Strip::track(1, MIC, Channels::Stereo(0, 1));
    t.gain_db = -6.0206;
    t.pan = -1.0;
    let (_, r) = run(GraphSpec { strips: vec![t], ..Default::default() }, 4);
    let (l, rr) = r.graph().program();
    assert!(close(last(l), 0.25), "left: half of 0.5 ({})", last(l));
    assert!(last(rr).abs() < 1e-4, "hard left: nothing on the right ({})", last(rr));
}

#[test]
fn buses_sum_and_route_on() {
    let mut a = Strip::track(1, MIC, Channels::Mono(0));
    let mut b = Strip::track(2, MIC, Channels::Mono(1));
    a.dest = Dest::Bus(10);
    b.dest = Dest::Bus(10);
    let mut bus = Strip::bus(10);
    bus.gain_db = -6.0206;
    let (_, r) = run(GraphSpec { strips: vec![bus, a, b], ..Default::default() }, 4);
    // (0.5 + 0.25) / 2
    assert!(close(last(r.graph().program().0), 0.375));
    // the bus's own signal before its fader
    assert!(close(last(r.graph().tap(10, Tap::PreFader).unwrap().0), 0.75));
}

#[test]
fn a_loop_is_broken_and_reported() {
    let mut a = Strip::bus(10);
    let mut b = Strip::bus(11);
    a.dest = Dest::Bus(11);
    b.dest = Dest::Bus(10);
    let mut t = Strip::track(1, MIC, Channels::Mono(0));
    t.dest = Dest::Bus(10);
    let spec = GraphSpec { strips: vec![t, a, b], ..Default::default() };
    let (mut engine, mut r) = Engine::new(spec.clone(), SR);
    let warnings = engine.set(spec);
    assert!(warnings.iter().any(|w| matches!(w, Warning::BadRoute { .. })), "{warnings:?}");
    let x = dc(0.5);
    let chans: [&[f32]; 1] = [&x];
    for _ in 0..3 {
        r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    }
    assert!(last(r.graph().program().0) > 0.4, "the signal still reaches Program");
}

#[test]
fn sends_feed_mixes_before_or_after_the_fader() {
    const PHONES: u128 = 50;
    const VOD: u128 = 51;
    let mut t = Strip::track(1, MIC, Channels::Mono(0));
    t.gain_db = -144.0; // fader all the way down
    t.sends = vec![
        Send { mix: PHONES, level_db: 0.0, pre_fader: true },
        Send { mix: VOD, level_db: 0.0, pre_fader: false },
        Send { mix: 99, level_db: 0.0, pre_fader: false },
    ];
    let spec = GraphSpec { strips: vec![t], mixes: vec![Mix::new(PHONES), Mix::new(VOD)], ..Default::default() };
    let (mut engine, r) = run(spec.clone(), 4);
    assert!(close(last(r.graph().mix(PHONES).unwrap().0), 0.5), "pre-fader: the fader doesn't matter");
    assert!(last(r.graph().mix(VOD).unwrap().0).abs() < 1e-6, "post-fader: down with it");
    assert!(last(r.graph().program().0).abs() < 1e-6);
    assert!(engine.set(spec).iter().any(|w| matches!(w, Warning::BadSend { mix: 99, .. })));
}

#[test]
fn mute_ramps_instead_of_clicking() {
    let t = Strip::track(1, MIC, Channels::Mono(0));
    let spec = GraphSpec { strips: vec![t.clone()], ..Default::default() };
    let (mut engine, mut r) = Engine::new(spec, SR);
    let x = dc(0.5);
    let chans: [&[f32]; 1] = [&x];
    r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    engine.set(GraphSpec { strips: vec![Strip { mute: true, ..t }], ..Default::default() });
    let mut out = Vec::new();
    for _ in 0..4 {
        out.extend_from_slice(r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK).program().0);
    }
    let biggest_step = out.windows(2).map(|w| (w[0] - w[1]).abs()).fold(0f32, f32::max);
    assert!(close(out[0], 0.5 - 0.5 / 480.0), "the swap continued from the old level ({})", out[0]);
    assert!(biggest_step < 0.002, "no click: largest step {biggest_step}");
    assert!(last(&out).abs() < 1e-6, "silent after 10 ms");
}

#[test]
fn effects_keep_their_place_and_settings_update_in_place() {
    let mut t = Strip::track(1, MIC, Channels::Mono(0));
    t.inserts = vec![EffectSpec { id: 7, enabled: true, params: Params::Gain(GainParams { db: 6.0206, ..Default::default() }) }];
    let (mut engine, mut r) = run(GraphSpec { strips: vec![t.clone()], ..Default::default() }, 4);
    assert!(close(last(r.graph().program().0), 1.0), "+6 dB doubles");
    t.inserts[0].params = Params::Gain(GainParams { db: 0.0, invert: true, ..Default::default() });
    engine.set(GraphSpec { strips: vec![t.clone()], ..Default::default() });
    let x = dc(0.5);
    let chans: [&[f32]; 1] = [&x];
    for _ in 0..4 {
        r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    }
    assert!(close(last(r.graph().program().0), -0.5), "polarity flipped");
    t.inserts[0].enabled = false;
    engine.set(GraphSpec { strips: vec![t], ..Default::default() });
    r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    assert!(close(last(r.graph().program().0), 0.5), "bypassed");
}

#[test]
fn program_loudness_of_a_sine() {
    // A 1 kHz sine at 0.1 peak on both sides: −20 dBFS and −20 LUFS (K-weighting adds about
    // +0.69 dB at 1 kHz, cancelling the −0.691 offset).
    let spec = GraphSpec { strips: vec![Strip::track(1, MIC, Channels::Mono(0))], ..Default::default() };
    let (mut engine, mut r) = Engine::new(spec, SR);
    let meter = engine.meter(PROGRAM);
    let mut phase = 0f32;
    for _ in 0..(SR as usize * 4 / BLOCK) {
        let x: Vec<f32> = (0..BLOCK)
            .map(|_| {
                phase += 1000.0 / SR as f32;
                (phase * std::f32::consts::TAU).sin() * 0.1
            })
            .collect();
        let chans: [&[f32]; 1] = [&x];
        r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    }
    let l = meter.loudness();
    assert!((l.integrated_lufs + 20.0).abs() < 0.2, "integrated {}", l.integrated_lufs);
    assert!((l.momentary_lufs + 20.0).abs() < 0.2, "momentary {}", l.momentary_lufs);
    assert!((l.true_peak_dbtp + 20.0).abs() < 0.3, "true peak {}", l.true_peak_dbtp);
    let reading = engine.meter(1).read();
    assert!((reading.peak_db[0] + 20.0).abs() < 0.1, "strip peak {}", reading.peak_db[0]);
}

/// A stand-in plugin: scales by a factor, counts its blocks.
struct Scale(f32, std::sync::atomic::AtomicUsize);

impl lightair_audio::dsp::Plugin for Scale {
    fn process(&self, l: &mut [f32], r: &mut [f32]) -> bool {
        l.iter_mut().chain(r.iter_mut()).for_each(|x| *x *= self.0);
        self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }
}

#[test]
fn a_hosted_plugin_processes_and_survives_swaps() {
    use lightair_audio::dsp::PluginParams;
    use std::sync::Arc;
    let half = Arc::new(Scale(0.5, Default::default()));
    let spec = |p: Option<Arc<Scale>>| {
        let mut t = Strip::track(1, MIC, Channels::Mono(0));
        t.inserts.push(EffectSpec {
            id: 7,
            enabled: true,
            params: Params::Plugin(PluginParams { plugin: p.map(|p| p as Arc<dyn lightair_audio::dsp::Plugin>) }),
        });
        GraphSpec { strips: vec![t], ..Default::default() }
    };
    // not loaded yet: the audio passes through
    let (mut engine, mut r) = run(spec(None), 2);
    assert!(close(last(r.graph().program().0), 0.5));
    let (a, b) = (dc(0.5), dc(0.25));
    let chans: [&[f32]; 2] = [&a, &b];
    let go = |engine: &mut Engine, r: &mut lightair_audio::Renderer, s: GraphSpec| {
        engine.set(s);
        for _ in 0..2 {
            r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
        }
    };
    // loaded: it runs, and a fader move (a new graph) keeps the same instance running
    go(&mut engine, &mut r, spec(Some(half.clone())));
    assert!(close(last(r.graph().program().0), 0.25));
    go(&mut engine, &mut r, spec(Some(half.clone())));
    assert_eq!(half.1.load(std::sync::atomic::Ordering::Relaxed), 4);
    // another instance under the same effect id replaces it
    let quarter = Arc::new(Scale(0.25, Default::default()));
    go(&mut engine, &mut r, spec(Some(quarter.clone())));
    assert!(close(last(r.graph().program().0), 0.125));
    assert_eq!(half.1.load(std::sync::atomic::Ordering::Relaxed), 4, "the old one no longer runs");
    drop(r);
    drop(engine);
    assert_eq!(Arc::strong_count(&quarter), 1, "the graph lets go of it");
}

#[test]
fn a_mix_can_feed_program() {
    // the track goes nowhere itself; it reaches Program only through the mix, at half level
    let mut t = Strip::track(1, MIC, Channels::Mono(0));
    t.dest = Dest::Nowhere;
    t.sends.push(Send { mix: 50, level_db: 0.0, pre_fader: false });
    let mut fx = Mix::new(50);
    fx.gain_db = -6.0206;
    let spec = |to_program: bool| GraphSpec {
        strips: vec![t.clone()],
        mixes: vec![Mix { to_program, ..fx.clone() }],
        ..Default::default()
    };
    let (_, r) = run(spec(false), 4);
    assert!(last(r.graph().program().0).abs() < 1e-6, "its own output only");
    assert!(close(last(r.graph().mix(50).unwrap().0), 0.25));
    let (_, r) = run(spec(true), 4);
    assert!(close(last(r.graph().program().0), 0.25), "into Program too: {}", last(r.graph().program().0));
    assert!(close(last(r.graph().mix(50).unwrap().0), 0.25), "and still its own output");
}

#[test]
fn inserts_run_in_their_order_and_keep_state_when_reordered() {
    use lightair_audio::dsp::LimiterParams;
    // +12 dB then a limiter at −6 dB: the limiter has the last word (0.5). The other way round,
    // the gain does (0.5 passes the limiter, then ×4 = 2.0).
    let gain = EffectSpec { id: 1, enabled: true, params: Params::Gain(GainParams { db: 12.0412, ..Default::default() }) };
    let limit = EffectSpec {
        id: 2,
        enabled: true,
        params: Params::Limiter(LimiterParams { ceiling_db: -6.0206, ..Default::default() }),
    };
    let spec = |order: Vec<EffectSpec>| {
        let mut t = Strip::track(1, MIC, Channels::Mono(0));
        t.inserts = order;
        GraphSpec { strips: vec![t], ..Default::default() }
    };
    let (mut engine, mut r) = run(spec(vec![gain.clone(), limit.clone()]), 200);
    let out = last(r.graph().program().0);
    assert!((out - 0.5).abs() < 0.02, "gain then limiter: {out}");

    // reorder the running graph: the limiter (with its envelope) moves, it isn't rebuilt
    engine.set(spec(vec![limit.clone(), gain.clone()]));
    let (a, b) = (dc(0.5), dc(0.25));
    let chans: [&[f32]; 2] = [&a, &b];
    for _ in 0..200 {
        r.process(&[InputBlock { id: MIC, channels: &chans }], BLOCK);
    }
    let out = last(r.graph().program().0);
    assert!((out - 2.0).abs() < 0.05, "limiter then gain: {out}");
}

#[test]
fn a_mix_has_its_own_balance() {
    let mut t = Strip::track(1, MIC, Channels::Mono(0));
    t.sends.push(Send { mix: 50, level_db: 0.0, pre_fader: false });
    let mix = |pan: f32| Mix { pan, ..Mix::new(50) };
    let spec = |pan: f32| GraphSpec { strips: vec![t.clone()], mixes: vec![mix(pan)], ..Default::default() };
    let (_, r) = run(spec(0.0), 4);
    let (l, rr) = r.graph().mix(50).unwrap();
    assert!(close(last(l), 0.5) && close(last(rr), 0.5), "centre: unity on both sides");
    let (_, r) = run(spec(1.0), 4);
    let (l, rr) = r.graph().mix(50).unwrap();
    assert!(last(l).abs() < 1e-4 && close(last(rr), 0.5), "hard right: {} {}", last(l), last(rr));
    // Program isn't moved by a mix's balance
    assert!(close(last(r.graph().program().0), 0.5));
}
