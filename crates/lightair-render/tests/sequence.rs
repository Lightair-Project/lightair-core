//! A sequence of several tracks rendered and exported. Needs FFmpeg (skipped without it) and a
//! GPU.

use std::path::Path;
use std::process::Command;

use lightair_render::media::{self, Codec};
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Content, Filter, Keyframe, Media, Sequence, TextStyle, TrackKind, Transform, Transition, TransitionKind};

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

#[test]
fn text_filters_keyframes_and_a_crossfade() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(red), Some(blue)) = (
        lavfi(dir.path(), "red.mp4", "color=c=red:s=320x180:r=30:d=2"),
        lavfi(dir.path(), "blue.mp4", "color=c=blue:s=320x180:r=30:d=2"),
    ) else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let mut seq = Sequence::new((640, 360), 30);
    let r = seq.add_media(Media::from_file(&red).unwrap());
    let b = seq.add_media(Media::from_file(&blue).unwrap());
    let first = seq.append(r).unwrap();
    let second = seq.append(b).unwrap();
    seq.clip_mut(second).unwrap().transition = Some(Transition { kind: TransitionKind::Crossfade, duration_ms: 1_000 });
    // the red one in black and white
    seq.clip_mut(first).unwrap().filter = Filter::PRESETS.iter().find(|p| p.0 == "Mono").unwrap().1;
    // a title over both, moving from the left to the right
    let mut style = TextStyle::title("HELLO");
    style.background = Some((0x00FF00, 1.0));
    let title = seq.place_content(Content::Text(style), 0, 4_000);
    let (tt, _) = seq.find(title).unwrap();
    assert_eq!(tt, 0, "a title goes on the top track");
    let c = seq.clip_mut(title).unwrap();
    c.transform.scale = 0.5;
    c.keyframes = vec![
        Keyframe { at_ms: 0, transform: Transform { x: -200.0, scale: 0.5, ..Transform::default() } },
        Keyframe { at_ms: 4_000, transform: Transform { x: 200.0, scale: 0.5, ..Transform::default() } },
    ];

    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no renderer here ({e}); skipping");
            return;
        }
    };
    let at = |img: &[u8], x, y| px(img, 640, x, y);
    let early = r.frame(100);
    let grey = at(&early, 600, 340);
    assert!(grey[0].abs_diff(grey[1]) < 12 && grey[0].abs_diff(grey[2]) < 12, "the red clip is grey: {grey:?}");
    let g = at(&early, 120, 180);
    assert!(g[1] > 180 && g[0] < 80, "the title's green box on the left at first: {g:?}");
    assert!(at(&early, 520, 180)[1] < 120, "and not on the right");
    let late = r.frame(3_900);
    assert!(at(&late, 520, 180)[1] > 180, "the title moved to the right");
    // halfway through the crossfade: red's grey under half the blue
    let mid = r.frame(2_000);
    let m = at(&mid, 600, 340);
    assert!(m[2] > 90 && m[2] < 220 && m[0] > 20, "a mix of both: {m:?}");
    let after = r.frame(2_600);
    let a = at(&after, 600, 340);
    assert!(a[2] > 220 && a[0] < 40, "then the blue alone: {a:?}");
}

#[test]
fn captions_hear_the_sequences_sound() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let tone = dir.path().join("tone.wav");
    assert!(Command::new(ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2"]).arg(&tone).status().unwrap().success());
    let mut seq = Sequence::new((640, 360), 30);
    let silent = seq.clone();
    let t = seq.add_media(Media::from_file(&tone).unwrap());
    seq.place(t, 1_000, None).unwrap();
    let wav = dir.path().join("speech.wav");
    assert!(!lightair_render::seqrender::sound_to(&silent, &wav).unwrap(), "no sound, nothing to hear");
    assert!(lightair_render::seqrender::sound_to(&seq, &wav).unwrap());
    let c = media::contents(&wav).unwrap();
    assert!(c.audio && (c.duration_ms as i64 - 3_000).abs() < 150, "{} ms", c.duration_ms);

    // the lines become text clips on a captions track, made again in place
    let (style, place) = seq.caption_style();
    let lines = vec![(1_000, 1_800, "Hello there".to_string()), (1_900, 2_600, "again".to_string())];
    let ct = seq.set_captions(&lines, &style, place);
    assert!(seq.tracks[ct].captions && seq.tracks[ct].clips.len() == 2);
    assert_eq!(seq.tracks[ct].clips[0].end_ms(), 1_900, "the short gap closed");
    let again = seq.set_captions(&lines[..1], &style, place);
    assert_eq!((again, seq.tracks[again].clips.len()), (ct, 1));
}
