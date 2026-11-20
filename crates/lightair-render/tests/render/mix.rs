//! The Rust mix (`mix::Mix`) against FFmpeg's graph (`sound::stream`) on the same sequence:
//! clips placed, at their volumes, faded, music ducked under a voice. Needs FFmpeg (skipped
//! without it).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use lightair_render::media;
use lightair_render::mix::Mix;
use lightair_render::sequence::{Clip, Media, Sequence, Track, TrackKind};

const RATE: u32 = 48_000;

fn tone(dir: &Path, name: &str, hz: u32, secs: u32) -> Option<PathBuf> {
    let ff = media::ffmpeg().ok()?;
    let out = dir.join(name);
    let input = format!("sine=frequency={hz}:duration={secs}:sample_rate={RATE}");
    Command::new(ff)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &input])
        .arg(&out)
        .status()
        .ok()?
        .success()
        .then_some(out)
}

/// Root mean square of each 100 ms of the left channel.
fn loudness(stereo: &[f32]) -> Vec<f32> {
    let n = RATE as usize / 10;
    stereo
        .chunks(n * 2)
        .map(|c| (c.iter().step_by(2).map(|s| s * s).sum::<f32>() / (c.len() / 2) as f32).sqrt())
        .collect()
}

#[test]
fn the_rust_mix_matches_ffmpegs() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(voice), Some(music)) = (tone(dir.path(), "voice.wav", 440, 3), tone(dir.path(), "music.wav", 220, 5))
    else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let mut s = Sequence::new((320, 180), 30);
    s.tracks.clear();
    for (file, start, edit) in [
        (
            &music,
            0u64,
            &(|c: &mut Clip| {
                c.duck = true;
                c.volume = 0.8;
            }) as &dyn Fn(&mut Clip),
        ),
        (&voice, 1_000, &|c: &mut Clip| {
            c.fade_in_ms = 500;
            c.fade_out_ms = 500;
        }),
    ] {
        let m = Media::from_file(file).unwrap();
        let id = s.add_media(m.clone());
        let mut c = Clip::new(id, start, m.duration_ms);
        edit(&mut c);
        s.tracks.push(Track { clips: vec![c], ..Track::empty(TrackKind::Audio) });
    }

    let mut ff = lightair_render::sound::stream(&s, 0, RATE).unwrap().unwrap();
    let mut bytes = Vec::new();
    ff.stdout.take().unwrap().read_to_end(&mut bytes).unwrap();
    ff.wait().unwrap();
    let theirs: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let ours = Mix::new(&s, RATE).unwrap().unwrap().render(0, 5 * RATE as usize);

    let (a, b) = (loudness(&theirs), loudness(&ours));
    // the same loudness, 100 ms by 100 ms: the music alone, faded voice, ducked, and after
    for (i, (x, y)) in a.iter().zip(&b).enumerate().take(48) {
        assert!((x - y).abs() < 0.03 + 0.1 * x.max(*y), "at {} ms: FFmpeg {x:.3}, Rust {y:.3}", i * 100);
    }
    // ducking really happens in both
    assert!(b[25] < b[5] * 0.8 + 0.25, "{} vs {}", b[25], b[5]);
}

/// FFmpeg's ranged reads (what the mix reads long sources in) join into the whole track, an AAC
/// one too: no seam where a read starts.
#[test]
fn ffmpegs_ranged_reads_join_into_the_whole_track() {
    use lightair_render::media::Backend;
    let dir = tempfile::tempdir().unwrap();
    let Some(file) = tone(dir.path(), "long.m4a", 440, 25) else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let whole = media::Ffmpeg.audio(&file, None, RATE, 2).unwrap();
    let n = 10 * RATE as u64;
    let mut joined = Vec::new();
    for i in 0..3 {
        joined.extend(media::Ffmpeg.audio_range(&file, None, RATE, 2, i * n, n as usize).unwrap());
    }
    assert_eq!(joined.len(), whole.len());
    let worst = joined.iter().zip(&whole).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst < 1e-3, "{worst}");
}
