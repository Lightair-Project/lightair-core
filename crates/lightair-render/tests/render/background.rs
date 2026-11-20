//! A sequence's background behind a fitted picture (Lightair Editor's ED-18): a colour, a
//! backdrop, the picture itself blurred. Needs FFmpeg and a GPU.

use std::process::Command;

use lightair_render::media;
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Background, Media, Sequence};

fn px(img: &[u8], w: u32, x: u32, y: u32) -> [i32; 3] {
    let i = ((y * w + x) * 4) as usize;
    [img[i] as i32, img[i + 1] as i32, img[i + 2] as i32]
}

fn near(a: [i32; 3], b: u32, by: i32) -> bool {
    let b = [(b >> 16) as i32 & 255, (b >> 8) as i32 & 255, b as i32 & 255];
    (0..3).all(|i| (a[i] - b[i]).abs() <= by)
}

#[test]
fn a_fitted_picture_has_its_fill_behind_it() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    // a landscape picture in a portrait sequence: bars above and below
    let src = dir.path().join("src.mp4");
    assert!(
        Command::new(ff)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=0xE0603A:s=320x180:r=30:d=2",
                "-pix_fmt",
                "yuv420p"
            ])
            .arg(&src)
            .status()
            .unwrap()
            .success()
    );
    let (w, h) = (180u32, 320u32);
    let mut seq = Sequence::new((w, h), 30);
    let m = seq.add_media(Media::from_file(&src).unwrap());
    seq.append(m).unwrap();
    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no renderer here ({e}); skipping");
            return;
        }
    };
    let mut frame = |s: &Sequence| {
        r.set(s).unwrap();
        r.frame(500)
    };
    // (the bar: the top 10 px; the picture: the middle)
    let plain = frame(&seq);
    assert!(near(px(&plain, w, w / 2, 5), 0x000000, 8), "black bars by default");
    assert!(near(px(&plain, w, w / 2, h / 2), 0xE0603A, 24), "the picture");

    seq.background = Some(Background::Color { color: 0x2060C0 });
    let f = frame(&seq);
    assert!(near(px(&f, w, w / 2, 5), 0x2060C0, 4), "{:?}", px(&f, w, w / 2, 5));
    assert!(near(px(&f, w, w / 2, h / 2), 0xE0603A, 24), "the picture still on top");

    seq.background = Some(Background::Backdrop { name: "dusk".into() });
    let f = frame(&seq);
    let (top, bottom) = Background::backdrop("Dusk").unwrap();
    assert!(near(px(&f, w, w / 2, 1), top, 16), "{:?}", px(&f, w, w / 2, 1));
    assert!(near(px(&f, w, w / 2, h - 2), bottom, 16), "{:?}", px(&f, w, w / 2, h - 2));

    // the picture itself, covering the frame, a little darker
    seq.background = Some(Background::Blur { amount: 0.6, dim: 0.2 });
    let f = frame(&seq);
    let bar = px(&f, w, w / 2, 5);
    assert!(bar[0] > 80 && bar[0] > bar[2] + 40, "the picture's colour in the bar: {bar:?}");
    assert!(bar[0] < 0xE0, "darker than the picture: {bar:?}");
    assert!(near(px(&f, w, w / 2, h / 2), 0xE0603A, 24), "the picture itself sharp on top");

    // it's saved with the sequence; an older file has none
    let json = serde_json::to_value(&seq).unwrap();
    assert_eq!(json["background"]["kind"], "blur");
    let back: Sequence = serde_json::from_value(json).unwrap();
    assert_eq!(back.background, seq.background);
    let mut old = serde_json::to_value(Sequence::new((w, h), 30)).unwrap();
    old.as_object_mut().unwrap().remove("background");
    assert_eq!(serde_json::from_value::<Sequence>(old).unwrap().background, None);
}

/// A clip's corners rounded and a ring round it (Lightair Editor's camera bubble, ED-17).
#[test]
fn a_clip_is_drawn_as_a_bubble() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let src = dir.path().join("cam.mp4");
    assert!(
        Command::new(ff)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=0xE0603A:s=200x200:r=30:d=2",
                "-pix_fmt",
                "yuv420p"
            ])
            .arg(&src)
            .status()
            .unwrap()
            .success()
    );
    let (w, h) = (200u32, 200u32);
    let mut seq = Sequence::new((w, h), 30);
    let m = seq.add_media(Media::from_file(&src).unwrap());
    let clip = seq.append(m).unwrap();
    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no renderer here ({e}); skipping");
            return;
        }
    };
    let mut frame = |s: &Sequence| {
        r.set(s).unwrap();
        r.frame(500)
    };
    let square = frame(&seq);
    assert!(near(px(&square, w, 3, 3), 0xE0603A, 24), "square: the corner is the picture");

    // a circle: the corners gone, the middle and the edges' middles there
    seq.clip_mut(clip).unwrap().radius = 0.5;
    let f = frame(&seq);
    assert!(near(px(&f, w, 3, 3), 0x000000, 8), "{:?}", px(&f, w, 3, 3));
    assert!(near(px(&f, w, 100, 100), 0xE0603A, 24) && near(px(&f, w, 100, 3), 0xE0603A, 24));

    // a ring: its colour at the edge, the picture inside
    seq.clip_mut(clip).unwrap().border = Some((0x2060C0, 10.0));
    let f = frame(&seq);
    assert!(near(px(&f, w, 100, 4), 0x2060C0, 24), "{:?}", px(&f, w, 100, 4));
    assert!(near(px(&f, w, 100, 30), 0xE0603A, 24));
    assert!(near(px(&f, w, 3, 3), 0x000000, 8));

    // saved with the clip; a clip from before has neither
    let back: Sequence = serde_json::from_value(serde_json::to_value(&seq).unwrap()).unwrap();
    let c = back.tracks.iter().flat_map(|t| &t.clips).find(|c| c.id == clip).unwrap();
    assert_eq!((c.radius, c.border), (0.5, Some((0x2060C0, 10.0))));
    let plain = serde_json::to_value(Sequence::new((w, h), 30)).unwrap();
    assert!(!plain.to_string().contains("radius"));
}
