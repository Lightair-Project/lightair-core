//! What the mixer costs: a podcast-sized graph (eight mics with Voice Clarity, EQ and a
//! compressor, a music track ducked under them with reverb, a bus, two mixes), timed per block.
//! Run optimised: `cargo test -p lightair-audio --release --test cost -- --ignored --nocapture`.

use lightair_audio::dsp::*;
use lightair_audio::{Channels, Dest, EffectSpec, Engine, GraphSpec, InputBlock, Mix, Send, Strip};

#[test]
#[ignore = "a measurement; run optimised"]
fn podcast_graph_cost() {
    let e = |id, params| EffectSpec { id, enabled: true, params };
    let mut strips: Vec<Strip> = (0..8u128)
        .map(|i| {
            let mut s = Strip::track(i + 1, 100, Channels::Mono(i as u16));
            s.inserts = vec![
                e(1000 + i * 10, Params::Voice(VoiceParams::default())),
                e(1001 + i * 10, Params::Eq(EqParams::default())),
                e(1002 + i * 10, Params::Compressor(CompressorParams::default())),
            ];
            s.dest = Dest::Bus(50);
            s.sends = vec![Send { mix: 60, level_db: 0.0, pre_fader: true }];
            s
        })
        .collect();
    let mut music = Strip::track(20, 101, Channels::Stereo(0, 1));
    music.inserts = vec![
        e(2000, Params::Ducker(DuckerParams { source: 1, ..Default::default() })),
        e(2001, Params::Reverb(ReverbParams::default())),
    ];
    strips.push(music);
    strips.push(Strip::bus(50));
    let mut program = lightair_audio::Master::default();
    program.inserts = vec![e(3000, Params::Limiter(LimiterParams::default()))];
    let spec = GraphSpec { strips, mixes: vec![Mix::new(60), Mix::new(61)], program };
    let (_, mut r) = Engine::new(spec, 48_000);
    let chans: Vec<Vec<f32>> = (0..8).map(|c| (0..256).map(|i| ((i * (c + 3)) as f32 * 0.01).sin() * 0.2).collect()).collect();
    let refs: Vec<&[f32]> = chans.iter().map(|c| c.as_slice()).collect();
    let music_ch: [&[f32]; 2] = [&chans[0], &chans[1]];
    let inputs = [InputBlock { id: 100, channels: &refs }, InputBlock { id: 101, channels: &music_ch }];
    for _ in 0..200 {
        r.process(&inputs, 256);
    }
    let blocks = 2000;
    let t = std::time::Instant::now();
    for _ in 0..blocks {
        r.process(&inputs, 256);
    }
    let per_block = t.elapsed().as_secs_f64() / blocks as f64;
    let budget = 256.0 / 48_000.0;
    eprintln!(
        "{:.3} ms per 256-frame block ({:.1}% of the {:.2} ms real-time budget)",
        per_block * 1e3,
        per_block / budget * 100.0,
        budget * 1e3
    );
}
