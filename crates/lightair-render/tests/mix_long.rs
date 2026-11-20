//! A long source mixed a few seconds at a time (`mix::Mix`): an hour-long made-up recording whose
//! backend only answers ranged reads. Playing a stretch reads that stretch, not the hour, and
//! the windows it's read in join without a seam. Its own test program: the backend is the
//! process's.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lightair_render::media::{self, Backend, Contents, MediaError, Probe, VideoReader};
use lightair_render::mix::Mix;
use lightair_render::sequence::{Clip, Media, Sequence, Track, TrackKind};

const RATE: u32 = 1_000;
const HOUR_MS: u64 = 3_600_000;

/// Frames read so far.
static READ: AtomicU64 = AtomicU64::new(0);

/// `.ramp`: an hour whose every sample is its own time in hours (0 to 1).
struct Ramp;

impl Backend for Ramp {
    fn contents(&self, _: &Path) -> Result<Contents, MediaError> {
        Ok(Contents { video: false, audio: true, still: false, width: 0, height: 0, duration_ms: HOUR_MS })
    }

    fn probe(&self, path: &Path) -> Result<Probe, MediaError> {
        Err(MediaError::Unreadable(path.into()))
    }

    fn video(&self, path: &Path, _: u64, _: f64, _: (u32, u32)) -> Result<Box<dyn VideoReader>, MediaError> {
        Err(MediaError::Unreadable(path.into()))
    }

    fn audio(&self, _: &Path, _: Option<u32>, _: u32, _: u32) -> Result<Vec<f32>, MediaError> {
        panic!("a long source is never read whole");
    }

    fn audio_range(
        &self,
        _: &Path,
        _: Option<u32>,
        rate: u32,
        channels: u32,
        from: u64,
        frames: usize,
    ) -> Result<Vec<f32>, MediaError> {
        let total = HOUR_MS * rate as u64 / 1000;
        let end = (from + frames as u64).min(total);
        READ.fetch_add(end.saturating_sub(from), Ordering::Relaxed);
        Ok((from..end).flat_map(|f| vec![f as f32 / total as f32; channels as usize]).collect())
    }
}

fn hour(at_ms: u64) -> f32 {
    at_ms as f32 / HOUR_MS as f32
}

#[test]
fn an_hour_long_source_is_read_where_its_played() {
    media::set_backend(Arc::new(Ramp));
    let mut s = Sequence::new((320, 180), 30);
    s.tracks.clear();
    let m = Media::from_file(Path::new("/a/talk.ramp")).unwrap();
    let id = s.add_media(m.clone());
    s.tracks.push(Track { clips: vec![Clip::new(id, 0, m.duration_ms)], ..Track::empty(TrackKind::Audio) });

    let mix = Mix::new(&s, RATE).unwrap().unwrap();
    // 40 minutes in, across a window's edge (every 10 s): the source's own time, no seam
    let from = 40 * 60_000 - 500;
    let piece = mix.render(from, RATE as usize);
    for ms in 0..1_000u64 {
        let got = piece[ms as usize * 2];
        assert!((got - hour(from + ms)).abs() < 1e-6, "{ms} ms in: {got}");
    }
    // what was read: the first window, then the two around 40 minutes (not the hour)
    assert!(READ.load(Ordering::Relaxed) <= 3 * 10 * RATE as u64, "{} frames read", READ.load(Ordering::Relaxed));
}
