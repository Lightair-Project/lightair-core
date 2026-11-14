//! How fast the Editor's frames come (run by hand: `cargo test -p lightair-render --test speed
//! -- --ignored --nocapture`). Needs FFmpeg and a GPU.

use std::process::Command;
use std::time::Instant;

use lightair_render::media;
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Media, Sequence};

fn source(dir: &std::path::Path, name: &str, size: &str, fps: u32) -> std::path::PathBuf {
    let src = dir.join(name);
    let ff = media::ffmpeg().unwrap();
    assert!(Command::new(ff)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("testsrc2=s={size}:r={fps}:d=6"), "-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast"])
        .arg(&src)
        .status()
        .unwrap()
        .success());
    src
}

#[test]
#[ignore]
fn frames_per_second() {
    let dir = tempfile::tempdir().unwrap();
    if media::ffmpeg().is_err() {
        return;
    }
    for (name, size, fps, seq_size) in [("hd.mp4", "1920x1080", 30, (1920, 1080)), ("retina.mp4", "2880x1800", 60, (2880, 1800))] {
        let src = source(dir.path(), name, size, fps);
        let mut seq = Sequence::new(seq_size, fps);
        let m = seq.add_media(Media::from_file(&src).unwrap());
        seq.append(m).unwrap();
        for preview in [None, Some((1280, 720))] {
            let mut r = SequenceRenderer::new(&seq).unwrap();
            r.set_preview(preview);
            r.frame(0);
            let start = Instant::now();
            let mut n = 0;
            r.play(0, |t, _| {
                n += 1;
                t < 4_000
            });
            let s = start.elapsed().as_secs_f64();
            eprintln!("{size}@{fps}, frames at {:?}: {n} frames in {s:.2} s = {:.1} fps", r.out_size(), n as f64 / s);
        }
    }
}

#[test]
#[ignore]
fn swizzle_cost() {
    let mut v = vec![7u8; 1920 * 1080 * 4];
    let start = Instant::now();
    for p in v.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    eprintln!("swap per pixel: {:?}", start.elapsed());
    let start = Instant::now();
    for w in v.as_chunks_mut::<4>().0.iter_mut() {
        let x = u32::from_le_bytes(*w);
        *w = ((x & 0xFF00FF00) | ((x & 0xFF) << 16) | ((x >> 16) & 0xFF)).to_le_bytes();
    }
    eprintln!("as u32: {:?}", start.elapsed());
}
