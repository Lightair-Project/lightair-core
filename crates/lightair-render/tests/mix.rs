//! A sequence's sound mixed in Rust (`mix::Mix`), sample by sample, from made-up sources read
//! through a media backend of the test's own: `<level>.dc` is four seconds of that constant
//! level. No FFmpeg needed. Its own test program: the backend is the process's.

use std::path::Path;
use std::sync::Arc;

use lightair_render::media::{self, Backend, Contents, MediaError, Probe, VideoReader};
use lightair_render::mix::Mix;
use lightair_render::sequence::{Media, Sequence, Track, TrackKind};

const RATE: u32 = 1_000;

struct Dc;

fn level(path: &Path) -> Result<f32, MediaError> {
    let ok = path.extension().is_some_and(|e| e == "dc");
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse().ok())
        .filter(|_| ok)
        .ok_or_else(|| MediaError::Unreadable(path.into()))
}

impl Backend for Dc {
    fn contents(&self, path: &Path) -> Result<Contents, MediaError> {
        level(path)?;
        Ok(Contents { video: false, audio: true, still: false, width: 0, height: 0, duration_ms: 4_000 })
    }

    fn probe(&self, path: &Path) -> Result<Probe, MediaError> {
        Err(MediaError::Unreadable(path.into()))
    }

    fn video(&self, path: &Path, _: u64, _: f64, _: (u32, u32)) -> Result<Box<dyn VideoReader>, MediaError> {
        Err(MediaError::Unreadable(path.into()))
    }

    fn audio(&self, path: &Path, _track: Option<u32>, rate: u32, channels: u32) -> Result<Vec<f32>, MediaError> {
        Ok(vec![level(path)?; (rate * channels * 4) as usize])
    }
}

/// A sequence with one audio track per source: (file, start, edit of the clip).
/// A clip's file, where it starts, and what's done to it.
type Placed<'a> = (&'a str, u64, &'a dyn Fn(&mut lightair_render::sequence::Clip));

fn sequence(clips: &[Placed]) -> Sequence {
    let mut s = Sequence::new((320, 180), 30);
    s.tracks.clear();
    for (file, start, edit) in clips {
        let m = Media::from_file(Path::new(file)).unwrap();
        let id = s.add_media(m.clone());
        let mut c = lightair_render::sequence::Clip::new(id, *start, m.duration_ms);
        edit(&mut c);
        s.tracks.push(Track { clips: vec![c], ..Track::empty(TrackKind::Audio) });
    }
    s
}

fn left(stereo: &[f32], ms: usize) -> f32 {
    stereo[ms * RATE as usize / 1000 * 2]
}

#[test]
fn clips_are_placed_shaped_sped_and_ducked() {
    media::set_backend(Arc::new(Dc));

    // placed at 1 s, at half volume, faded in over a second
    let s = sequence(&[("/a/0.5.dc", 1_000, &|c| {
        c.volume = 0.5;
        c.fade_in_ms = 1_000;
    })]);
    let mix = Mix::new(&s, RATE).unwrap().unwrap();
    let all = mix.render(0, 6 * RATE as usize);
    assert_eq!(left(&all, 500), 0.0, "before it starts");
    assert!((left(&all, 1_500) - 0.125).abs() < 0.01, "half faded in: {}", left(&all, 1_500));
    assert!((left(&all, 3_000) - 0.25).abs() < 1e-4);
    assert_eq!(left(&all, 5_500), 0.0, "after it ends");
    // any stretch renders the same alone as in a run
    let piece = mix.render(2_000, RATE as usize);
    assert_eq!(&piece[..], &all[2 * RATE as usize * 2..3 * RATE as usize * 2]);

    // twice as fast: half as long, the same level
    let s = sequence(&[("/a/0.5.dc", 0, &|c| {
        c.speed = 2.0;
    })]);
    assert_eq!(s.duration_ms(), 2_000);
    let fast = Mix::new(&s, RATE).unwrap().unwrap().render(0, 3 * RATE as usize);
    assert!((left(&fast, 1_000) - 0.5).abs() < 1e-3, "{}", left(&fast, 1_000));
    assert_eq!(left(&fast, 2_500), 0.0);

    // music under a voice is pressed down; alone it isn't
    let duck = |c: &mut lightair_render::sequence::Clip| c.duck = true;
    let music = Mix::new(&sequence(&[("/a/0.4.dc", 0, &duck)]), RATE).unwrap().unwrap().render(0, 3 * RATE as usize);
    assert!((left(&music, 2_000) - 0.4).abs() < 1e-4);
    let s = sequence(&[("/a/0.4.dc", 0, &duck), ("/a/0.3.dc", 1_000, &|_| {})]);
    let both = Mix::new(&s, RATE).unwrap().unwrap().render(0, 3 * RATE as usize);
    assert!((left(&both, 500) - 0.4).abs() < 1e-4, "no voice yet: {}", left(&both, 500));
    // the voice 0.3 and the music by (0.02 + (0.3 - 0.02) / 8) / 0.3
    let g = (0.02 + (0.3 - 0.02) / 8.0) / 0.3;
    assert!((left(&both, 2_500) - (0.3 + 0.4 * g)).abs() < 0.01, "{}", left(&both, 2_500));

    // a muted clip and silence
    let s = sequence(&[("/a/0.5.dc", 0, &|c| c.muted = true)]);
    assert!(Mix::new(&s, RATE).unwrap().is_none());
}
