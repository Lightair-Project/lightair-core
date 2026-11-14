//! A take (or an ordinary recording) and its edit, rendered: one frame for the Editor's preview,
//! or the whole result into a file.

use std::path::{Path, PathBuf};

use lightair_store::input_log::Log;

use crate::compose::{Compositor, Frame};
use crate::edit::Edit;
use crate::media::{self, Codec, Decoder, Encoder, MediaError, Probe};
use crate::pointer::Track;
use crate::timeline;
use crate::zoom::{Camera, View};

/// What's rendered: the screen's video, the audio to mix, and the input log (takes).
#[derive(Debug, Clone)]
pub struct Source {
    pub video: PathBuf,
    pub audio: Vec<PathBuf>,
    pub log: Option<PathBuf>,
}

impl Source {
    /// A take folder: its screen, its stems and its input log (from take.json).
    pub fn of_take(folder: &Path) -> Option<Source> {
        let take: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("take.json")).ok()?).ok()?;
        let video = take["video"].as_array()?.iter().find(|v| v["name"].as_str().is_some_and(|n| n.starts_with("Screen")))
            .or_else(|| take["video"].as_array()?.first())?;
        let audio = take["audio"].as_array().map(|a| a.iter().filter_map(|f| f.as_str().map(|f| folder.join(f))).collect()).unwrap_or_default();
        let log = folder.join("input.bin");
        Some(Source { video: folder.join(video["file"].as_str()?), audio, log: log.exists().then_some(log) })
    }

    /// An ordinary recording: its own audio, no pointer (it's in the picture already).
    pub fn of_file(path: &Path) -> Source {
        Source { video: path.to_path_buf(), audio: vec![path.to_path_buf()], log: None }
    }
}

/// A source and its edit, ready to draw.
pub struct Renderer {
    pub source: Source,
    pub probe: Probe,
    edit: Edit,
    camera: Camera,
    pointer: Option<Track>,
    compositor: Compositor,
    /// The size the screen is decoded at (enough for a 2× zoom of its box).
    decode_size: (u32, u32),
}

fn even(v: f32) -> u32 {
    ((v.round() as u32).max(2)) & !1
}

impl Renderer {
    pub fn new(source: Source, edit: &Edit) -> Result<Renderer, MediaError> {
        let probe = media::probe(&source.video)?;
        let log = source.log.as_ref().and_then(|p| Log::read(p).ok());
        let pointer = log.as_ref().map(|l| Track::build(l, probe.duration_ms, edit.pointer.smoothing));
        let compositor = Compositor::new(edit.size, edit.look).map_err(MediaError::Failed)?;
        let aspect = probe.width as f32 / probe.height.max(1) as f32;
        let bx = compositor.screen_box(aspect);
        let k = ((bx.2 * 2.0) / probe.width as f32).min(1.0);
        let decode_size = (even(probe.width as f32 * k), even(probe.height as f32 * k));
        let camera = Self::camera_of(edit, &probe, pointer.as_ref());
        Ok(Renderer { source, probe, edit: edit.clone(), camera, pointer, compositor, decode_size })
    }

    fn camera_of(edit: &Edit, probe: &Probe, pointer: Option<&Track>) -> Camera {
        Camera::build(&edit.zooms, probe.duration_ms, |t| pointer.and_then(|p| p.at(t, 1.0)).map(|p| (p.x, p.y)))
    }

    /// Takes a changed edit (zooms, cuts, look).
    pub fn set_edit(&mut self, edit: &Edit) -> Result<(), MediaError> {
        if edit.size != self.edit.size || edit.look != self.edit.look {
            self.compositor = Compositor::new(edit.size, edit.look).map_err(MediaError::Failed)?;
        }
        if edit.pointer.smoothing != self.edit.pointer.smoothing
            && let Some(l) = self.source.log.as_ref().and_then(|p| Log::read(p).ok())
        {
            self.pointer = Some(Track::build(&l, self.probe.duration_ms, edit.pointer.smoothing));
        }
        self.camera = Self::camera_of(edit, &self.probe, self.pointer.as_ref());
        self.edit = edit.clone();
        Ok(())
    }

    pub fn duration_ms(&self) -> u64 {
        timeline::duration_ms(&self.edit)
    }

    pub fn view_at(&self, source_ms: u64) -> View {
        self.camera.at(source_ms)
    }

    fn compose(&mut self, screen: &[u8], source_ms: u64) -> Vec<u8> {
        let aspect = self.probe.width as f32 / self.probe.height.max(1) as f32;
        let bx = self.compositor.screen_box(aspect);
        let p = &self.edit.pointer;
        // the pointer's size: Lightair's 22-point pointer, on a screen as wide as the box
        let pointer_px = 22.0 * p.size * bx.2 / 1512.0;
        let (pointer, clicks) = match (&self.pointer, p.hidden) {
            (Some(t), false) => (t.at(source_ms, bx.2), t.clicks_at(source_ms)),
            _ => (None, Vec::new()),
        };
        let frame = Frame {
            screen,
            screen_size: self.decode_size,
            view: self.camera.at(source_ms),
            pointer,
            pointer_px,
            pointer_style: p.style,
            click_style: p.clicks,
            clicks,
        };
        self.compositor.render(&frame)
    }

    /// The result's frame at `t_ms` (result time), RGBA at the edit's size.
    pub fn frame_at(&mut self, t_ms: u64) -> Result<Vec<u8>, MediaError> {
        let s = timeline::source_at(&self.edit, t_ms.min(self.duration_ms().saturating_sub(1))).unwrap_or(0);
        let screen = media::frame_at(&self.source.video, s, self.decode_size)?;
        Ok(self.compose(&screen, s))
    }

    /// Renders the whole result into `out` at `fps`. `progress(fraction)` returns false to stop.
    pub fn export(&mut self, out: &Path, fps: f64, codec: Codec, mut progress: impl FnMut(f32) -> bool) -> Result<(), MediaError> {
        let total = self.duration_ms();
        let frames = (total as f64 * fps / 1000.0).ceil() as u64;
        let mut enc = Encoder::start(out, self.edit.size, fps, codec, &self.source.audio, &self.edit.clips)?;
        let start = self.edit.clips.first().map_or(0, |c| c.start_ms);
        let mut dec = Decoder::open(&self.source.video, start, fps, self.decode_size)?;
        let mut current = Vec::new();
        let mut have = false;
        for i in 0..frames {
            let t = (i as f64 * 1000.0 / fps) as u64;
            let Some(s) = timeline::source_at(&self.edit, t) else { break };
            // a long cut: start reading again from there rather than through it
            if s > dec.next_ms() + 10_000 {
                dec = Decoder::open(&self.source.video, s, fps, self.decode_size)?;
                have = false;
            }
            while !have || dec.next_ms() <= s {
                if !dec.next(&mut current) {
                    break;
                }
                have = true;
            }
            if !have {
                break;
            }
            let rgba = self.compose(&current, s);
            enc.write(&rgba)?;
            if !progress((i + 1) as f32 / frames as f32) {
                drop(enc);
                let _ = std::fs::remove_file(out);
                return Err(MediaError::Failed("stopped".into()));
            }
        }
        enc.finish()
    }
}
