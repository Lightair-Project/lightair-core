//! Thumbnails for the Editor's looks (CapCut's grids): each filter, effect, transition and
//! animation drawn onto sample pictures made here (a sunset and a day, no one's footage), by the
//! same renderer the player uses. Transitions and animations get a few frames each, played while
//! the pointer is over their card. Made once into a folder and kept.

use std::path::{Path, PathBuf};

use lightair_model::Id;
use lightair_model::catalog;
use tiny_skia::{
    Color, FillRule, GradientStop, IntSize, LinearGradient, Paint, PathBuilder, Pixmap, Point, Shader, SpreadMode,
    Transform as Tf,
};

use crate::motion::{self, Motion, MotionKind};
use crate::seqrender::SequenceRenderer;
use crate::sequence::{
    Clip, ClipEffect, Filter, Media, Sequence, Track, TrackKind, Transform, Transition, TransitionKind,
};

/// The thumbnails' size.
pub const SIZE: (u32, u32) = (320, 180);
/// Frames of a transition's or an animation's thumbnail.
pub const FRAMES: usize = 6;
/// The frame shown when it isn't playing.
pub const STILL: usize = 3;

/// Which animation slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    In,
    Out,
    Combo,
}

impl Slot {
    fn tag(self) -> &'static str {
        match self {
            Slot::In => "in",
            Slot::Out => "out",
            Slot::Combo => "combo",
        }
    }

    /// The frame shown when it isn't playing: well on the way in (or out).
    pub fn still(self) -> usize {
        match self {
            Slot::In => 1,
            Slot::Out => 4,
            Slot::Combo => STILL,
        }
    }

    /// The animations it offers.
    pub fn kinds(self) -> &'static [(&'static str, MotionKind)] {
        match self {
            Slot::Combo => &motion::COMBO,
            _ => &motion::IN,
        }
    }
}

pub fn filter(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("filter-{i}.png"))
}

pub fn effect(dir: &Path, kind: &str) -> PathBuf {
    dir.join(format!("{kind}.png"))
}

pub fn transition(dir: &Path, i: usize, frame: usize) -> PathBuf {
    dir.join(format!("tr-{i}-{frame}.png"))
}

pub fn animation(dir: &Path, slot: Slot, i: usize, frame: usize) -> PathBuf {
    dir.join(format!("{}-{i}-{frame}.png", slot.tag()))
}

/// The effects offered: the catalog's shader effects.
pub fn effects() -> Vec<&'static catalog::EffectDef> {
    catalog::CATALOG.iter().filter(|d| d.kind.starts_with("fx.")).collect()
}

/// Whether they're all made.
pub fn ready(dir: &Path) -> bool {
    dir.join("done").exists()
}

fn stop(rgb: u32, at: f32) -> GradientStop {
    GradientStop::new(at, Color::from_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255))
}

fn fill(p: &mut Pixmap, path: tiny_skia::Path, rgb: u32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255);
    paint.anti_alias = true;
    p.fill_path(&path, &paint, FillRule::Winding, Tf::identity(), None);
}

/// Hills along the bottom: a wavy line at `base` (fraction of the height).
fn hills(p: &mut Pixmap, base: f32, wave: f32, phase: f32, rgb: u32) {
    let (w, h) = (p.width() as f32, p.height() as f32);
    let mut b = PathBuilder::new();
    b.move_to(0.0, h);
    for i in 0..=32 {
        let x = w * i as f32 / 32.0;
        let y = h * (base - wave * ((x / w * 7.0 + phase).sin() * 0.6 + (x / w * 13.0 + phase * 2.0).sin() * 0.4));
        b.line_to(x, y);
    }
    b.line_to(w, h);
    b.close();
    if let Some(path) = b.finish() {
        fill(p, path, rgb);
    }
}

/// A sample picture: 0 a sunset, 1 a day, 2 a dark backdrop.
fn sample(which: usize) -> Pixmap {
    let (w, h) = (SIZE.0 as f32, SIZE.1 as f32);
    let mut p = Pixmap::new(SIZE.0, SIZE.1).expect("size");
    let sky: [(u32, f32); 3] = match which {
        0 => [(0x2B1B5A, 0.0), (0xD9468F, 0.55), (0xFFB25B, 1.0)],
        1 => [(0x2F7DE1, 0.0), (0x7EC4F5, 0.6), (0xD8F0FF, 1.0)],
        _ => [(0x1A1D24, 0.0), (0x262A33, 0.6), (0x30343E, 1.0)],
    };
    let shader = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(0.0, h),
        sky.iter().map(|(c, a)| stop(*c, *a)).collect(),
        SpreadMode::Pad,
        Tf::identity(),
    )
    .unwrap_or(Shader::SolidColor(Color::BLACK));
    let mut paint = Paint { shader, ..Paint::default() };
    paint.anti_alias = false;
    p.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, w, h).expect("rect"), &paint, Tf::identity(), None);
    match which {
        0 => {
            if let Some(c) = PathBuilder::from_circle(w * 0.68, h * 0.58, h * 0.2) {
                fill(&mut p, c, 0xFFE3A3);
            }
            hills(&mut p, 0.62, 0.06, 0.4, 0x6B2D6E);
            hills(&mut p, 0.78, 0.05, 2.1, 0x3A1945);
            hills(&mut p, 0.9, 0.025, 4.0, 0x1E0F2A);
        }
        1 => {
            if let Some(c) = PathBuilder::from_circle(w * 0.22, h * 0.24, h * 0.1) {
                fill(&mut p, c, 0xFFF6C8);
            }
            for (x, y, r) in [(0.6, 0.22, 0.07), (0.66, 0.2, 0.09), (0.73, 0.23, 0.065)] {
                if let Some(c) = PathBuilder::from_circle(w * x, h * y, h * r) {
                    fill(&mut p, c, 0xFFFFFF);
                }
            }
            hills(&mut p, 0.66, 0.05, 1.2, 0x5DB665);
            hills(&mut p, 0.82, 0.04, 3.3, 0x2F8F4E);
        }
        _ => {}
    }
    p
}

fn save(rgba: Vec<u8>, path: &Path) -> Result<(), String> {
    let mut rgba = rgba;
    // the frame is opaque; make sure (a PNG of a premultiplied pixmap)
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    let size = IntSize::from_wh(SIZE.0, SIZE.1).ok_or("size")?;
    let p = Pixmap::from_vec(rgba, size).ok_or("pixels")?;
    let tmp = path.with_extension("tmp");
    p.save_png(&tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// A sequence of one picture (with `f` done to its clip), and its clip.
fn one(media: &Media, len: u64, f: impl FnOnce(&mut Clip)) -> Sequence {
    let mut s = Sequence::new(SIZE, 30);
    s.add_media(media.clone());
    let mut c = Clip::new(media.id, 0, len);
    f(&mut c);
    s.tracks = vec![Track { clips: vec![c], ..Track::empty(TrackKind::Video) }];
    s
}

/// Makes the thumbnails in `dir` (those not made yet). Slow-ish: run it on a thread.
pub fn make(dir: &Path) -> Result<(), String> {
    if ready(dir) {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut media = Vec::new();
    for which in 0..3 {
        let path = dir.join(format!("sample-{which}.png"));
        if !path.exists() {
            sample(which).save_png(&path).map_err(|e| e.to_string())?;
        }
        let mut m = Media::from_file(&path).map_err(|e| e.to_string())?;
        m.id = Id::new();
        media.push(m);
    }
    let (sunset, day, backdrop) = (&media[0], &media[1], &media[2]);
    let mut r = SequenceRenderer::new(&Sequence::new(SIZE, 30)).map_err(|e| e.to_string())?;
    let mut shoot = |seq: &Sequence, t: u64, path: &Path| -> Result<(), String> {
        if path.exists() {
            return Ok(());
        }
        r.set(seq).map_err(|e| e.to_string())?;
        save(r.frame(t), path)
    };
    for (i, (_, f)) in Filter::PRESETS.iter().enumerate() {
        shoot(&one(sunset, 2_000, |c| c.filter = *f), 0, &filter(dir, i))?;
    }
    for d in effects() {
        let fx = ClipEffect::new(d.kind, d.defaults());
        shoot(&one(sunset, 2_000, |c| c.effects = vec![fx]), 500, &effect(dir, d.kind))?;
    }
    for (i, (_, kind)) in TransitionKind::ALL.iter().enumerate() {
        let mut s = one(sunset, 2_000, |_| {});
        s.add_media(day.clone());
        let mut b = Clip::new(day.id, 2_000, 2_000);
        b.transition = Some(Transition { kind: *kind, duration_ms: 1_000 });
        s.tracks[0].clips.push(b);
        for f in 0..FRAMES {
            let t = 1_500 + ((f as f32 + 0.5) / FRAMES as f32 * 1_000.0) as u64;
            shoot(&s, t, &transition(dir, i, f))?;
        }
    }
    // the animations: the picture smaller, over a backdrop, so it can be seen moving
    for slot in [Slot::In, Slot::Out, Slot::Combo] {
        for (i, (_, kind)) in slot.kinds().iter().enumerate() {
            let len = if slot == Slot::Combo { 3_000 } else { 2_000 };
            let mut s = one(sunset, len, |c| {
                c.transform = Transform { scale: 0.62, ..Transform::default() };
                let m = Some(Motion { kind: *kind, ms: if slot == Slot::Combo { 2_000 } else { 1_000 } });
                match slot {
                    Slot::In => c.motion.enter = m,
                    Slot::Out => c.motion.exit = m,
                    Slot::Combo => c.motion.combo = m,
                }
            });
            s.add_media(backdrop.clone());
            s.tracks.push(Track { clips: vec![Clip::new(backdrop.id, 0, len)], ..Track::empty(TrackKind::Video) });
            for f in 0..FRAMES {
                let k = (f as f32 + 1.0) / (FRAMES as f32 + 1.0);
                let t = match slot {
                    Slot::In => (k * 1_000.0) as u64,
                    Slot::Out => 1_000 + (k * 1_000.0) as u64,
                    Slot::Combo => (k * len as f32) as u64,
                };
                shoot(&s, t, &animation(dir, slot, i, f))?;
            }
        }
    }
    std::fs::write(dir.join("done"), b"").map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_pictures() {
        for which in 0..3 {
            let p = sample(which);
            assert_eq!((p.width(), p.height()), SIZE);
            // not one colour
            let px = p.pixels();
            assert!(px.iter().any(|c| *c != px[0]), "sample {which}");
        }
    }

    #[test]
    fn every_look_gets_a_thumbnail() {
        // LIGHTAIR_THUMBS: where to keep them, to look at
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::env::var_os("LIGHTAIR_THUMBS").map(PathBuf::from).unwrap_or_else(|| tmp.path().to_path_buf());
        make(&dir).unwrap();
        assert!(ready(&dir));
        assert!(filter(&dir, 4).exists() && effect(&dir, effects()[0].kind).exists());
        assert!(transition(&dir, TransitionKind::ALL.len() - 1, FRAMES - 1).exists());
        assert!(animation(&dir, Slot::Combo, motion::COMBO.len() - 1, FRAMES - 1).exists());
        // a transition's frames differ
        assert_ne!(
            std::fs::read(transition(&dir, 3, 0)).unwrap(),
            std::fs::read(transition(&dir, 3, FRAMES - 1)).unwrap()
        );
    }
}
