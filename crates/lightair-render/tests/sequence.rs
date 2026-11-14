//! A sequence of several tracks rendered and exported. Needs FFmpeg (skipped without it) and a
//! GPU.

use std::path::Path;
use std::process::Command;

use lightair_render::media::{self, Codec};
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Media, Sequence, TrackKind, Transform};

fn lavfi(dir: &Path, name: &str, input: &str) -> Option<std::path::PathBuf> {
    let ff = media::ffmpeg().ok()?;
    let out = dir.join(name);
    Command::new(ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", input]).args(["-pix_fmt", "yuv420p"]).arg(&out).status().ok()?.success().then_some(out)
}

fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

#[test]
fn tracks_stack_clips_follow_each_other_and_the_export_sounds() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(red), Some(blue)) = (
        lavfi(dir.path(), "red.mp4", "color=c=red:s=320x180:r=30:d=3"),
        lavfi(dir.path(), "blue.mp4", "color=c=blue:s=320x180:r=30:d=3"),
    ) else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let ff = media::ffmpeg().unwrap();
    let tone = dir.path().join("tone.wav");
    assert!(Command::new(ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=5"]).arg(&tone).status().unwrap().success());

    let mut seq = Sequence::new((640, 360), 30);
    let r = seq.add_media(Media::from_file(&red).unwrap());
    let b = seq.add_media(Media::from_file(&blue).unwrap());
    let t = seq.add_media(Media::from_file(&tone).unwrap());
    let first = seq.append(r).unwrap();
    let second = seq.append(b).unwrap();
    assert_eq!(seq.clip(second).unwrap().start_ms, seq.clip(first).unwrap().end_ms());
    // a small blue picture over the red, from 1 s
    let over = seq.place(b, 1_000, None).unwrap();
    seq.trim_end(over, 2_000);
    seq.clip_mut(over).unwrap().transform = Transform { scale: 0.3, ..Transform::default() };
    let tone_clip = seq.place(t, 0, None).unwrap();
    let (tt, _) = seq.find(tone_clip).unwrap();
    assert_eq!(seq.tracks[tt].kind, TrackKind::Audio);

    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no renderer here ({e}); skipping");
            return;
        }
    };
    let at = |img: &[u8], x, y| px(img, 640, x, y);
    let red_frame = r.frame(500);
    assert!(at(&red_frame, 320, 180)[0] > 200 && at(&red_frame, 320, 180)[2] < 60, "red first");
    let both = r.frame(1_500);
    assert!(at(&both, 320, 180)[2] > 200, "the small blue clip in front, in the middle");
    assert!(at(&both, 20, 20)[0] > 200, "the red around it");
    let later = r.frame(3_500);
    assert!(at(&later, 20, 20)[2] > 200, "then the blue clip on the main track");

    let out = dir.path().join("Sequence.mp4");
    r.export(&out, Codec::H264, |_| true).unwrap();
    let p = media::probe(&out).unwrap();
    assert_eq!((p.width, p.height), (640, 360));
    assert!((p.duration_ms as i64 - 6_000).abs() < 200, "{} ms", p.duration_ms);
    let c = media::contents(&out).unwrap();
    assert!(c.audio, "the tone is in it");
}
