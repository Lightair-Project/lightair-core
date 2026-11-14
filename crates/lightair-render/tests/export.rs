//! A take through an edit: cut, sped up, zoomed, with the pointer, into an MP4 of the right
//! length and size. Needs FFmpeg (skipped without it) and a GPU.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use lightair_render::edit::{Edit, Focus, Zoom};
use lightair_render::export::{Renderer, Source};
use lightair_render::media::{self, Codec};
use lightair_store::input_log::{Entry, Record, Writer};

fn make_take(dir: &Path) -> bool {
    let Ok(ff) = media::ffmpeg() else { return false };
    let ok = |c: &mut Command| c.status().is_ok_and(|s| s.success());
    if !ok(Command::new(&ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x200:rate=30:duration=6"])
        .args(["-pix_fmt", "yuv420p"]).arg(dir.join("screen.mp4")))
    {
        return false;
    }
    if !ok(Command::new(&ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=6"]).arg(dir.join("Audio - Mic.wav"))) {
        return false;
    }
    let mut w = Writer::create(&dir.join("input.bin"), true).unwrap();
    let ms = Duration::from_millis;
    w.write(&[
        Record { at: ms(0), entry: Entry::DisplaySize { w: 320.0, h: 200.0 } },
        Record { at: ms(0), entry: Entry::Move { x: 40.0, y: 40.0 } },
        Record { at: ms(1_000), entry: Entry::Move { x: 200.0, y: 120.0 } },
        Record { at: ms(1_500), entry: Entry::Down { button: 0, x: 200.0, y: 120.0 } },
    ])
    .unwrap();
    w.flush().unwrap();
    let take = serde_json::json!({
        "video": [{"name": "Screen", "file": "screen.mp4", "width": 320, "height": 200}],
        "audio": ["Audio - Mic.wav"],
        "input": "input.bin",
    });
    std::fs::write(dir.join("take.json"), serde_json::to_vec(&take).unwrap()).unwrap();
    true
}

#[test]
fn a_take_exports_through_its_edit() {
    let dir = tempfile::tempdir().unwrap();
    if !make_take(dir.path()) {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    }
    let source = Source::of_take(dir.path()).expect("a take");
    let mut edit = Edit::new(6_000);
    edit.size = (640, 360);
    edit.split(2_000);
    edit.split(3_000);
    edit.remove_clip_at(2_500); // 2–3 s cut
    edit.clips[1].speed = 2.0; // 3–6 s at twice the speed: 1.5 s
    edit.zooms.push(Zoom { start_ms: 1_000, end_ms: 2_000, scale: 2.0, focus: Focus::Pointer, auto: false });
    let mut r = match Renderer::new(source, &edit) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no renderer here ({e}); skipping");
            return;
        }
    };
    assert_eq!(r.duration_ms(), 3_500);
    let still = r.frame_at(1_800).unwrap();
    assert_eq!(still.len(), 640 * 360 * 4);
    let out = dir.path().join("Edited.mp4");
    let mut last = 0.0;
    r.export(&out, 30.0, Codec::H264, |p| {
        last = p;
        true
    })
    .unwrap();
    assert!((last - 1.0).abs() < 1e-6);
    // to look at it: LIGHTAIR_RENDER_OUT=<folder>
    if let Some(keep) = std::env::var_os("LIGHTAIR_RENDER_OUT") {
        let _ = std::fs::copy(&out, Path::new(&keep).join("Edited.mp4"));
    }
    let p = media::probe(&out).unwrap();
    assert_eq!((p.width, p.height), (640, 360));
    assert!((p.duration_ms as i64 - 3_500).abs() < 150, "{} ms", p.duration_ms);

    // with subtitles: a text track beside the video
    let srt = dir.path().join("captions.srt");
    std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
    r.subtitles = Some(srt);
    let subbed = dir.path().join("Captioned.mp4");
    r.export(&subbed, 30.0, Codec::H264, |_| true).unwrap();
    let ffprobe = media::ffmpeg().unwrap().with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    let streams = Command::new(ffprobe).args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"]).arg(&subbed).output().unwrap();
    let kinds = String::from_utf8_lossy(&streams.stdout).to_string();
    assert!(kinds.contains("video") && kinds.contains("audio") && kinds.contains("subtitle"), "{kinds}");
    // a GIF
    r.subtitles = None;
    let gif = dir.path().join("Clip.gif");
    r.export(&gif, 15.0, Codec::Gif, |_| true).unwrap();
    let g = media::probe(&gif).unwrap();
    assert_eq!((g.width, g.height), (640, 360));
}
