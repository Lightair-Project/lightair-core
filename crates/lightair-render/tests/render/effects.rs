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
    let kinds: Vec<&str> = lightair_model::catalog::CATALOG.iter().map(|d| d.kind).filter(|k| k.starts_with("fx.")).collect();
    assert!(kinds.len() >= 20);
    let mut quiet = Vec::new();
    for kind in &kinds {
        let mut s = seq.clone();
        let def = lightair_model::catalog::def(kind).unwrap();
        s.clip_mut(clip).unwrap().effects = vec![ClipEffect::new(*kind, def.defaults())];
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
        ClipEffect { on: true, ..ClipEffect::new("fx.blur", lightair_model::catalog::def("fx.blur").unwrap().defaults()) },
        ClipEffect { on: false, ..ClipEffect::new("fx.invert", lightair_model::catalog::def("fx.invert").unwrap().defaults()) },
        ClipEffect { on: true, ..ClipEffect::new("fx.mono", lightair_model::catalog::def("fx.mono").unwrap().defaults()) },
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

/// A colour look-up table ("fx.lut", a .cube file): every colour graded through it, by `amount`;
/// an unreadable file leaves the picture as it was.
#[test]
fn a_lut_grades_the_picture() {
    let dir = tempfile::tempdir().unwrap();
    let Ok(ff) = media::ffmpeg() else { return };
    let src = dir.path().join("red.mp4");
    assert!(
        Command::new(ff)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=160x90:r=30:d=1", "-pix_fmt", "yuv420p"])
            .arg(&src)
            .status()
            .unwrap()
            .success()
    );
    // a 2³ table that turns every colour round (red → cyan)
    let mut cube = String::from("TITLE \"Invert\"\nLUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                cube += &format!("{} {} {}\n", 1 - r, 1 - g, 1 - b);
            }
        }
    }
    let table = dir.path().join("invert.cube");
    std::fs::write(&table, cube).unwrap();
    let mut seq = Sequence::new((160, 90), 30);
    let m = seq.add_media(Media::from_file(&src).unwrap());
    let clip = seq.append(m).unwrap();
    let Ok(mut r) = SequenceRenderer::new(&seq) else { return };
    let mid = (45 * 160 + 80) * 4;
    let lut = |file: &std::path::Path, amount: f64| {
        let mut settings = serde_json::Map::new();
        settings.insert("file".into(), serde_json::json!(file.display().to_string()));
        settings.insert("amount".into(), serde_json::json!(amount));
        ClipEffect::new("fx.lut", settings)
    };
    let px = |r: &mut SequenceRenderer, s: &Sequence| {
        r.set(s).unwrap();
        r.frame(500)[mid..mid + 3].to_vec()
    };
    let mut s = seq.clone();
    s.clip_mut(clip).unwrap().effects = vec![lut(&table, 1.0)];
    let full = px(&mut r, &s);
    assert!(full[0] < 40 && full[1] > 200 && full[2] > 200, "cyan: {full:?}");
    s.clip_mut(clip).unwrap().effects = vec![lut(&table, 0.5)];
    let half = px(&mut r, &s);
    assert!((100..160).contains(&half[0]) && (100..160).contains(&half[1]), "halfway: {half:?}");
    s.clip_mut(clip).unwrap().effects = vec![lut(&dir.path().join("gone.cube"), 1.0)];
    let gone = px(&mut r, &s);
    assert!(gone[0] > 200 && gone[1] < 40, "as it was: {gone:?}");
}
