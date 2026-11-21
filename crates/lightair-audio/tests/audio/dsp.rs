//! Each built-in effect on test signals, with measurable expectations.

use lightair_audio::dsp::*;
use lightair_audio::{Channels, EffectSpec, Engine, GraphSpec, InputBlock, Strip, db_to_gain, gain_to_db};

const SR: f32 = 48_000.0;

fn sine(freq: f32, amp: f32, frames: usize) -> Vec<f32> {
    (0..frames).map(|i| (i as f32 * freq / SR * std::f32::consts::TAU).sin() * amp).collect()
}

/// Runs `params` over a mono signal (both channels), in 256-frame blocks.
fn run(params: Params, input: &[f32]) -> (Vec<f32>, Box<dyn Processor>) {
    let mut p = params.make(SR);
    let (mut l, mut r) = (input.to_vec(), input.to_vec());
    let ctx = Ctx { sample_rate: SR, key: None };
    for (a, b) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
        p.process(a, b, &ctx);
    }
    (l, p)
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0f32, |m, v| m.max(v.abs()))
}

fn peak_db(x: &[f32]) -> f32 {
    gain_to_db(peak(x))
}

fn tail(x: &[f32]) -> &[f32] {
    &x[x.len() / 2..]
}

#[test]
fn eq_curve_and_bell() {
    let mut eq = EqParams::default();
    eq.bands[0] = Band { on: true, kind: BandKind::Bell, freq: 1000.0, gain_db: 6.0, q: 1.0 };
    eq.bands[1] = Band { on: true, kind: BandKind::HighPass(24), freq: 100.0, gain_db: 0.0, q: 0.7 };
    assert!((eq.response_db(1000.0, SR) - 6.0).abs() < 0.1, "bell peak {}", eq.response_db(1000.0, SR));
    let hp = eq.response_db(50.0, SR);
    assert!((hp + 24.1).abs() < 1.5, "24 dB/oct an octave below: {hp}");
    assert!(eq.response_db(100.0, SR).abs() < 3.5 && eq.response_db(100.0, SR) < -2.5, "−3 dB at the corner");
    let (out, _) = run(Params::Eq(eq), &sine(1000.0, 0.25, 48_000));
    assert!((peak_db(tail(&out)) - (gain_to_db(0.25) + 6.0)).abs() < 0.2, "1 kHz sine up 6 dB");
}

#[test]
fn compressor_curve_and_settled_level() {
    assert!((compression_db(-8.0, -18.0, 2.0, 0.0) + 5.0).abs() < 1e-4);
    assert_eq!(compression_db(-30.0, -18.0, 4.0, 6.0), 0.0, "below the knee: nothing");
    let p = CompressorParams {
        threshold_db: -18.0,
        ratio: 4.0,
        knee_db: 0.0,
        attack_ms: 1.0,
        release_ms: 50.0,
        auto_makeup: false,
        sidechain_hpf: 0.0,
        ..CompressorParams::default()
    };
    let (out, proc) = run(Params::Compressor(p), &sine(1000.0, db_to_gain(-6.0), 48_000));
    // 12 dB over at 4:1 → 3 dB over: −15 dBFS (peak detector on a sine: within 1 dB)
    assert!((peak_db(tail(&out)) + 15.0).abs() < 1.0, "settled at {}", peak_db(tail(&out)));
    assert!(proc.reduction_db() < -8.0, "meter shows the reduction ({})", proc.reduction_db());
}

#[test]
fn gate_closes_on_quiet_and_opens_on_loud() {
    let p = GateParams { threshold_db: -40.0, range_db: 40.0, ratio: 20.0, ..GateParams::default() };
    let (quiet, _) = run(Params::Gate(p), &sine(300.0, db_to_gain(-60.0), 48_000));
    assert!(peak_db(tail(&quiet)) < -95.0, "quiet hum taken down 40 dB: {}", peak_db(tail(&quiet)));
    let (loud, _) = run(Params::Gate(p), &sine(300.0, db_to_gain(-20.0), 48_000));
    assert!((peak_db(tail(&loud)) + 20.0).abs() < 0.2, "speech-level passes: {}", peak_db(tail(&loud)));
}

#[test]
fn limiter_never_lets_a_sample_past_the_ceiling() {
    let p = LimiterParams { ceiling_db: -1.0, release_ms: 50.0, input_db: 6.0 };
    let ceiling = db_to_gain(-1.0) * 1.0001;
    // a loud sine with sharp bursts on top
    let mut x = sine(220.0, 0.9, 96_000);
    for i in (1000..96_000).step_by(4_801) {
        x[i] = 1.0;
        x[i + 1] = -1.0;
    }
    let (out, proc) = run(Params::Limiter(p), &x);
    assert!(peak(&out) <= ceiling, "peak {} over the ceiling", peak(&out));
    assert!(peak_db(tail(&out)) > -2.0, "and it's loud right up to it");
    assert_eq!(proc.latency(), 71, "1.5 ms look-ahead");
}

#[test]
fn de_esser_takes_sibilance_not_voice() {
    let p = DeEsserParams { freq: 5000.0, threshold_db: -30.0, range_db: 10.0, split: true, listen: false };
    let (s, _) = run(Params::DeEsser(p), &sine(8000.0, db_to_gain(-10.0), 48_000));
    assert!(peak_db(tail(&s)) < -15.0, "8 kHz 'sss' down: {}", peak_db(tail(&s)));
    let (v, _) = run(Params::DeEsser(p), &sine(200.0, db_to_gain(-10.0), 48_000));
    assert!((peak_db(tail(&v)) + 10.0).abs() < 0.3, "200 Hz voice untouched: {}", peak_db(tail(&v)));
}

#[test]
fn music_ducks_under_the_mic() {
    const MIC: u128 = 1;
    const MUSIC: u128 = 2;
    let mic = Strip::track(MIC, 100, Channels::Mono(0));
    let mut music = Strip::track(MUSIC, 101, Channels::Mono(0));
    // listed before the mic: the graph still processes the mic first
    music.inserts = vec![EffectSpec {
        id: 9,
        enabled: true,
        params: Params::Ducker(DuckerParams {
            source: MIC,
            amount_db: 12.0,
            attack_ms: 5.0,
            ..DuckerParams::default()
        }),
    }];
    let (_, mut r) = Engine::new(GraphSpec { strips: vec![music, mic], ..Default::default() }, 48_000);
    let tone = sine(440.0, 0.5, 256);
    let voice = sine(200.0, 0.3, 256);
    let silence = vec![0f32; 256];
    let level = |r: &mut lightair_audio::Renderer, mic: &[f32], blocks: usize| {
        let mut last = 0f32;
        for _ in 0..blocks {
            let (m, t): ([&[f32]; 1], [&[f32]; 1]) = ([mic], [&tone]);
            let g = r.process(&[InputBlock { id: 100, channels: &m }, InputBlock { id: 101, channels: &t }], 256);
            last = peak(g.tap(MUSIC, lightair_audio::Tap::PostFader).unwrap().0);
        }
        gain_to_db(last)
    };
    let free = level(&mut r, &silence, 200);
    let ducked = level(&mut r, &voice, 100);
    assert!((free - gain_to_db(0.5)).abs() < 0.2, "music alone at full level: {free}");
    assert!((ducked - (gain_to_db(0.5) - 12.0)).abs() < 1.0, "12 dB under the mic: {ducked}");
}

#[test]
fn noise_suppression_takes_down_noise() {
    // white noise at about −30 dBFS, deterministic
    let mut seed = 12345u32;
    let noise: Vec<f32> = (0..96_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.06
        })
        .collect();
    let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
    let (out, proc) = run(Params::Noise(NoiseParams::default()), &noise);
    let drop = gain_to_db(rms(tail(&out))) - gain_to_db(rms(tail(&noise)));
    assert!(drop < -10.0, "noise down by more than 10 dB ({drop:.1})");
    assert_eq!(proc.latency(), 480, "10 ms frames");
}

fn impulse() -> Vec<f32> {
    let mut impulse = vec![0f32; 48_000];
    impulse[0] = 1.0;
    impulse
}

#[test]
fn a_reverb_leaves_a_tail() {
    let (wet, _) = run(Params::Reverb(ReverbParams { mix: 1.0, ..ReverbParams::default() }), &impulse());
    let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>();
    assert!(energy(&wet[4_800..24_000]) > 1e-4, "a tail between 100 and 500 ms");
}

#[test]
fn a_dry_reverb_leaves_the_sound_untouched() {
    let (dry, _) = run(Params::Reverb(ReverbParams { mix: 0.0, ..ReverbParams::default() }), &impulse());
    assert_eq!(dry, impulse());
}

#[test]
fn a_delay_echoes_after_its_time() {
    let (echo, _) = run(
        Params::Delay(DelayParams { time_ms: 250.0, feedback: 0.0, mix: 0.5, ..DelayParams::default() }),
        &impulse(),
    );
    let at = (1..echo.len()).max_by(|&a, &b| echo[a].abs().total_cmp(&echo[b].abs())).unwrap();
    assert!((at as i32 - 12_000).abs() <= 2, "the echo 250 ms later (at {at})");
}

#[test]
fn width_zero_is_mono() {
    let mut p = Params::Channel(ChannelParams { width: 0.0, ..ChannelParams::default() }).make(SR);
    let (mut l, mut r) = (sine(300.0, 0.5, 256), sine(500.0, 0.2, 256));
    p.process(&mut l, &mut r, &Ctx { sample_rate: SR, key: None });
    assert_eq!(l, r);
}

#[test]
fn voice_clarity_reports_its_delay_and_stays_finite() {
    let (out, proc) = run(Params::Voice(VoiceParams::default()), &sine(220.0, 0.3, 48_000));
    assert!(out.iter().all(|v| v.is_finite()));
    assert_eq!(proc.latency(), 480 + 71, "noise frames + limiter look-ahead");
    assert!(peak(tail(&out)) > 0.05, "the voice comes through");
}
