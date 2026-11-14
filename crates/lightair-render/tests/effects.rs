//! The effect catalog's shader effects and hidden areas on a clip. Needs FFmpeg and a GPU.

use std::process::Command;

use lightair_render::media;
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{ClipEffect, Media, Sequence};

fn differ(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(p, q)| (0..3).map(|i| (p[i] as i32 - q[i] as i32).abs()).sum::<i32>() > 24).count()
}

#[test]
fn every_effect_changes_the_picture_and_hidden_areas_pixelate() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else {
        eprintln!("FFmpeg isn't installed; skipping");
        return;
    };
    let src = dir.path().join("src.mp4");
    assert!(Command::new(ff).args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=320x180:r=30:d=2", "-pix_fmt", "yuv420p"]).arg(&src).status().unwrap().success());
    let mut seq = Sequence::new((320, 180), 30);
    let m = seq.add_media(Media::from_file(&src).unwrap());
    let clip = seq.append(m).unwrap();
    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no renderer here ({e}); skipping");
            return;
        }
    };
    let plain = r.frame(1_000);
    let kinds: Vec<&str> = lightair_core::catalog::CATALOG.iter().map(|d| d.kind).filter(|k| k.starts_with("fx.")).collect();
    assert!(kinds.len() >= 20);
    let mut quiet = Vec::new();
    for kind in &kinds {
        let mut s = seq.clone();
        let def = lightair_core::catalog::def(kind).unwrap();
        s.clip_mut(clip).unwrap().effects = vec![ClipEffect { kind: kind.to_string(), settings: def.defaults(), on: true }];
        r.set(&s).unwrap();
        let f = r.frame(1_000);
        let n = differ(&plain, &f);
        eprintln!("{kind}: {n} pixels changed");
        if n < 200 {
            quiet.push(*kind);
        }
    }
    assert!(quiet.is_empty(), "no visible change: {quiet:?}");
    // two in a row, and one switched off
    let mut s = seq.clone();
    s.clip_mut(clip).unwrap().effects = vec![
        ClipEffect { kind: "fx.blur".into(), settings: lightair_core::catalog::def("fx.blur").unwrap().defaults(), on: true },
        ClipEffect { kind: "fx.invert".into(), settings: lightair_core::catalog::def("fx.invert").unwrap().defaults(), on: false },
        ClipEffect { kind: "fx.mono".into(), settings: lightair_core::catalog::def("fx.mono").unwrap().defaults(), on: true },
    ];
    r.set(&s).unwrap();
    let f = r.frame(1_000);
    let i = (90 * 320 + 40) * 4;
    assert!(f[i].abs_diff(f[i + 1]) < 6 && f[i].abs_diff(f[i + 2]) < 6, "grey: {:?}", &f[i..i + 3]);

    // the left half hidden: blocks there, the right half as it was
    let mut s = seq.clone();
    s.clip_mut(clip).unwrap().hidden = vec![(0.0, 0.0, 0.5, 1.0)];
    r.set(&s).unwrap();
    let f = r.frame(1_000);
    let half = |img: &[u8], left: bool| -> Vec<u8> {
        img.chunks(320 * 4).flat_map(|row| if left { row[..160 * 4].to_vec() } else { row[160 * 4..].to_vec() }).collect()
    };
    assert!(differ(&half(&plain, true), &half(&f, true)) > 1000, "the left half pixelated");
    assert!(differ(&half(&plain, false), &half(&f, false)) < 50, "the right half as it was");
}

#[test]
fn cropped_and_mirrored() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else { return };
    // left half red, right half blue
    let src = dir.path().join("halves.mp4");
    assert!(Command::new(ff)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=160x180:r=30:d=1", "-f", "lavfi", "-i", "color=c=blue:s=160x180:r=30:d=1", "-filter_complex", "[0][1]hstack", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap()
        .success());
    let mut seq = Sequence::new((320, 180), 30);
    let m = seq.add_media(Media::from_file(&src).unwrap());
    let clip = seq.append(m).unwrap();
    let Ok(mut r) = SequenceRenderer::new(&seq) else { return };
    let at = |f: &[u8], x: u32, y: u32| f[((y * 320 + x) * 4) as usize..][..3].to_vec();
    let plain = r.frame(500);
    assert!(at(&plain, 40, 90)[0] > 200 && at(&plain, 280, 90)[2] > 200);
    // mirrored: blue on the left
    seq.clip_mut(clip).unwrap().flip_h = true;
    r.set(&seq).unwrap();
    let f = r.frame(500);
    assert!(at(&f, 40, 90)[2] > 200 && at(&f, 280, 90)[0] > 200, "mirrored");
    // the left half cut away: blue where it was, nothing left of it
    let c = seq.clip_mut(clip).unwrap();
    c.flip_h = false;
    c.crop = (0.5, 0.0, 0.0, 0.0);
    r.set(&seq).unwrap();
    let f = r.frame(500);
    assert!(at(&f, 280, 90)[2] > 200, "the right half stays where it was");
    assert!(at(&f, 40, 90).iter().all(|v| *v < 30), "the cut part is gone: {:?}", at(&f, 40, 90));
}
