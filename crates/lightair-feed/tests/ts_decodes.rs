//! The muxer's output, checked by a real decoder: ffmpeg encodes two seconds of test picture
//! (H.264, then HEVC) and tone (AAC), the muxer packs the access units the way a phone's
//! encoder would hand them over, and ffmpeg decodes the transport stream reporting any error.
//! Skipped where ffmpeg isn't installed.

mod common;

use common::{ffmpeg, material, probe, run, scratch};
use lightair_feed::ts::{AudioConfig, Muxer, NalFormat, VideoCodec};

fn mux_and_decode(codec: VideoCodec) {
    let ext = if codec == VideoCodec::H264 { "h264" } else { "hevc" };
    let (units, sounds) = material(codec, "in");
    assert!(units.len() >= 59 && units[0].1, "{} units", units.len());
    let mut mux = Muxer::new(codec, Some(AudioConfig { sample_rate: 48000, channels: 2 }));
    let mut ts = vec![];
    // interleaved by time, as the encoders would deliver them
    let (mut v, mut a) = (0, 0);
    while v < units.len() || a < sounds.len() {
        let vt = v as u64 * 1_000_000 / 30;
        let at = a as u64 * 1024 * 1_000_000 / 48000;
        if v < units.len() && (a >= sounds.len() || vt <= at) {
            ts.extend(mux.video(&units[v].0, NalFormat::LengthPrefixed, vt, None, units[v].1));
            v += 1;
        } else {
            ts.extend(mux.audio(&sounds[a], at));
            a += 1;
        }
    }
    let out = scratch(&format!("out-{ext}.ts"));
    std::fs::write(&out, &ts).unwrap();

    // decodes without a single error, every frame there
    let errors = run(&["-i", out.to_str().unwrap(), "-f", "null", "-"]);
    assert!(errors.trim().is_empty(), "decoder errors: {errors}");
    let probe = probe(&out);
    let lines: Vec<&str> = probe.lines().collect();
    assert!(lines.iter().any(|l| l.starts_with(&format!("{ext},{}", units.len()))), "{probe}");
    assert!(lines.iter().any(|l| l.starts_with("aac,")), "{probe}");
}

#[test]
fn h264_and_aac_decode() {
    if !ffmpeg() {
        eprintln!("ffmpeg isn't installed: skipped");
        return;
    }
    mux_and_decode(VideoCodec::H264);
}

#[test]
fn hevc_and_aac_decode() {
    if !ffmpeg() {
        eprintln!("ffmpeg isn't installed: skipped");
        return;
    }
    mux_and_decode(VideoCodec::Hevc);
}
