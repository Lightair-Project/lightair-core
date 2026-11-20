//! Media read through another backend than FFmpeg's (`media::set_backend`), as a phone reads
//! them through its platform's decoders: a made-up one here, whose "files" are solid colours,
//! so this runs without FFmpeg. Its own test program: the backend is the process's.

use std::path::Path;
use std::sync::Arc;

use lightair_render::media::{self, Backend, Contents, MediaError, Probe, VideoReader};
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{Media, Sequence};

/// `<rrggbb>.solid`: a 2 s, 320×180, 30 fps video of that colour; and a second of silence.
struct Solid;

fn colour(path: &Path) -> Result<[u8; 3], MediaError> {
    let hex = path.file_stem().and_then(|s| s.to_str()).filter(|_| path.extension().is_some_and(|e| e == "solid"));
    let v = hex.and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| MediaError::Unreadable(path.into()))?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

struct Frames {
    rgb: [u8; 3],
    size: (u32, u32),
    next: u64,
    step: f64,
    read: u64,
}

impl VideoReader for Frames {
    fn next_ms(&self) -> u64 {
        self.next + (self.read as f64 * self.step) as u64
    }

    fn next(&mut self, buf: &mut Vec<u8>) -> bool {
        if self.next_ms() >= 2_000 {
            return false;
        }
        self.read += 1;
        let [r, g, b] = self.rgb;
        *buf = [r, g, b, 255].repeat((self.size.0 * self.size.1) as usize);
        true
    }
}

impl Backend for Solid {
    fn contents(&self, path: &Path) -> Result<Contents, MediaError> {
        colour(path)?;
        Ok(Contents { video: true, audio: true, still: false, width: 320, height: 180, duration_ms: 2_000 })
    }

    fn probe(&self, path: &Path) -> Result<Probe, MediaError> {
        colour(path)?;
        Ok(Probe { width: 320, height: 180, fps: 30.0, duration_ms: 2_000 })
    }

    fn video(
        &self,
        path: &Path,
        start_ms: u64,
        fps: f64,
        size: (u32, u32),
    ) -> Result<Box<dyn VideoReader>, MediaError> {
        Ok(Box::new(Frames { rgb: colour(path)?, size, next: start_ms, step: 1000.0 / fps, read: 0 }))
    }

    fn audio(&self, path: &Path, _track: Option<u32>, rate: u32, channels: u32) -> Result<Vec<f32>, MediaError> {
        colour(path)?;
        Ok(vec![0.0; (rate * channels * 2) as usize])
    }
}

#[test]
fn media_come_through_the_backend_set() {
    media::set_backend(Arc::new(Solid));
    let red = Path::new("/nowhere/ff0000.solid");
    let blue = Path::new("/nowhere/0000ff.solid");

    // what a file holds, a frame, its sound: no FFmpeg, no file
    let m = Media::from_file(red).unwrap();
    assert_eq!((m.width, m.height, m.duration_ms), (320, 180, 2_000));
    assert_eq!(&media::frame_at(blue, 500, (4, 2)).unwrap()[..4], &[0, 0, 255, 255]);
    assert_eq!(lightair_render::sound::decode(red, 48_000, 2).unwrap().len(), 48_000 * 2 * 2);
    assert!(Media::from_file(Path::new("/nowhere/picture.png")).is_err());

    // a sequence of the two, drawn
    let mut seq = Sequence::new((320, 180), 30);
    for f in [red, blue] {
        let id = seq.add_media(Media::from_file(f).unwrap());
        seq.append(id).unwrap();
    }
    let mut r = match SequenceRenderer::new(&seq) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no GPU here ({e}); skipping the drawing");
            return;
        }
    };
    let at = |f: &[u8]| [f[(90 * 320 + 160) * 4], f[(90 * 320 + 160) * 4 + 1], f[(90 * 320 + 160) * 4 + 2]];
    assert_eq!(at(&r.frame(1_000)), [255, 0, 0]);
    assert_eq!(at(&r.frame(3_000)), [0, 0, 255]);
}
