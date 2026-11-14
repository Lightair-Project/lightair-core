//! A sequence rendered (docs/plans/video-editor.md, phase 1): the frame at any moment (every
//! visible track's clip, back to front, placed by its transform), playback, and export with every
//! clip's sound cut, sped, set to its volume, placed in time and mixed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lightair_core::Id;

use crate::compose::{Compositor, Layer, fit};
use crate::edit::{Edit, Look};
use crate::export::{Renderer, Source};
use crate::media::{self, Codec, Encoder, MediaError, Stream};
use crate::sequence::{Clip, Media, MediaKind, Sequence, TrackKind};

pub struct SequenceRenderer {
    seq: Sequence,
    compositor: Compositor,
    fps: f64,
    /// Videos read in time order, by clip (one clip, one reader: two clips of a file at once).
    streams: HashMap<Id, Stream>,
    /// Stills, by medium: RGBA and size.
    stills: HashMap<Id, (Vec<u8>, (u32, u32))>,
    /// Takes, by medium: drawn with their screen recording look at the sequence's size.
    takes: HashMap<Id, Renderer>,
}

fn even(v: f32) -> u32 {
    ((v.round() as u32).max(2)) & !1
}

impl SequenceRenderer {
    pub fn new(seq: &Sequence) -> Result<SequenceRenderer, MediaError> {
        let compositor = Compositor::new(seq.size, Look::default()).map_err(MediaError::Failed)?;
        Ok(SequenceRenderer {
            seq: seq.clone(),
            compositor,
            fps: seq.fps.max(1) as f64,
            streams: HashMap::new(),
            stills: HashMap::new(),
            takes: HashMap::new(),
        })
    }

    /// Takes a changed sequence (readers of clips that changed size start again).
    pub fn set(&mut self, seq: &Sequence) -> Result<(), MediaError> {
        if seq.size != self.seq.size {
            self.compositor = Compositor::new(seq.size, Look::default()).map_err(MediaError::Failed)?;
            self.takes.clear();
        }
        self.fps = seq.fps.max(1) as f64;
        self.seq = seq.clone();
        let alive: Vec<Id> = seq.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.id)).collect();
        self.streams.retain(|id, _| alive.contains(id));
        Ok(())
    }

    /// Draws a take with its look again (the look changed).
    pub fn forget_take(&mut self, media: Id) {
        self.takes.remove(&media);
    }

    pub fn sequence(&self) -> &Sequence {
        &self.seq
    }

    /// Where a clip's picture sits: its centre and drawn size (result pixels).
    pub fn placement(&self, clip: &Clip, media: &Media) -> ((f32, f32), (f32, f32)) {
        let (w, h) = (self.seq.size.0 as f32, self.seq.size.1 as f32);
        let (mw, mh) = if media.take.is_some() || media.width == 0 { (w, h) } else { (media.width as f32, media.height as f32) };
        let (_, _, bw, bh) = fit(mw / mh.max(1.0), 0.0, 0.0, w, h);
        let t = clip.transform;
        ((w / 2.0 + t.x, h / 2.0 + t.y), (bw * t.scale, bh * t.scale))
    }

    /// The picture of one clip at timeline time `t`.
    fn picture(&mut self, clip: &Clip, t: u64) -> Option<(Vec<u8>, (u32, u32))> {
        let media = self.seq.media(clip.media)?.clone();
        let s = clip.source_at(t)?;
        if let Some(folder) = &media.take {
            if !self.takes.contains_key(&media.id) {
                let look = media.look.clone().unwrap_or_else(|| folder.join("edit.json"));
                let mut edit = Edit::load(&look).unwrap_or_else(|_| Edit::new(media.duration_ms.max(1)));
                edit.size = self.seq.size;
                edit.clips = Edit::new(media.duration_ms.max(1)).clips;
                let r = Renderer::new(Source::of_take(folder)?, &edit).ok()?;
                self.takes.insert(media.id, r);
            }
            let fps = self.fps;
            let frame = self.takes.get_mut(&media.id)?.source_frame(s, fps)?;
            return Some((frame, self.seq.size));
        }
        if media.kind == MediaKind::Image {
            if let std::collections::hash_map::Entry::Vacant(e) = self.stills.entry(media.id) {
                let k = (2048.0 / media.width.max(media.height).max(1) as f32).min(1.0);
                let size = (even(media.width as f32 * k), even(media.height as f32 * k));
                e.insert((media::frame_at(&media.path, 0, size).ok()?, size));
            }
            return self.stills.get(&media.id).cloned();
        }
        // the size it's drawn at (enough for its scale), never more than the file's own
        let (_, draw) = self.placement(clip, &media);
        let k = (draw.0 / media.width.max(1) as f32).min(1.0);
        let size = (even(media.width as f32 * k), even(media.height as f32 * k));
        if self.streams.get(&clip.id).is_none_or(|s| s.size() != size) {
            self.streams.insert(clip.id, Stream::new(&media.path, size, self.fps));
        }
        let rgba = self.streams.get_mut(&clip.id)?.frame(s)?.to_vec();
        Some((rgba, size))
    }

    /// The frame at timeline time `t`: RGBA at the sequence's size.
    pub fn frame(&mut self, t: u64) -> Vec<u8> {
        let clips: Vec<Clip> = self.seq.video_at(t).into_iter().map(|(_, c)| c.clone()).collect();
        let mut pictures = Vec::new();
        for c in &clips {
            let Some(media) = self.seq.media(c.media).cloned() else { continue };
            let Some((rgba, size)) = self.picture(c, t) else { continue };
            let (center, draw) = self.placement(c, &media);
            pictures.push((rgba, size, center, draw, c.transform));
        }
        let layers: Vec<Layer> = pictures
            .iter()
            .map(|(rgba, size, center, draw, tr)| Layer { rgba, size: *size, center: *center, draw: *draw, rotation: tr.rotation, opacity: tr.opacity })
            .collect();
        self.compositor.layers(&layers)
    }

    /// Frames from `from` to the end at the sequence's rate: `each(t, rgba)` until it says stop.
    pub fn play(&mut self, from: u64, mut each: impl FnMut(u64, Vec<u8>) -> bool) {
        let end = self.seq.duration_ms();
        let step = 1000.0 / self.fps;
        let mut i = (from as f64 / step).ceil() as u64;
        loop {
            let t = (i as f64 * step) as u64;
            if t >= end {
                break;
            }
            let f = self.frame(t);
            if !each(t, f) {
                break;
            }
            i += 1;
        }
    }

    /// The sound of the whole sequence as an FFmpeg filter over `inputs` (in order, from input
    /// 1), or `None` when nothing makes a sound.
    pub fn mix(&self) -> (Vec<PathBuf>, Option<String>) {
        let mut inputs: Vec<PathBuf> = Vec::new();
        let mut parts = Vec::new();
        for track in self.seq.tracks.iter().filter(|t| !t.muted) {
            for c in track.clips.iter().filter(|c| !c.muted && c.volume > 0.0) {
                let Some(m) = self.seq.media(c.media) else { continue };
                // a picture track's clips play their own sound; stills have none
                if m.kind == MediaKind::Image || (track.kind == TrackKind::Video && m.audio.is_empty()) {
                    continue;
                }
                let files = if m.audio.is_empty() { vec![m.path.clone()] } else { m.audio.clone() };
                for f in files {
                    inputs.push(f);
                    let i = inputs.len();
                    let s = c.speed.clamp(0.25, 16.0);
                    let tempo = if s < 0.5 { format!("atempo={},atempo=0.5", s / 0.5) } else { format!("atempo={s}") };
                    parts.push(format!(
                        "[{i}:a]atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS,{tempo},volume={:.3},adelay={}|{}[s{}]",
                        c.in_ms as f64 / 1000.0,
                        c.out_ms as f64 / 1000.0,
                        c.volume,
                        c.start_ms,
                        c.start_ms,
                        parts.len()
                    ));
                }
            }
        }
        if parts.is_empty() {
            return (inputs, None);
        }
        let labels: String = (0..parts.len()).map(|k| format!("[s{k}]")).collect();
        let f = format!("{};{labels}amix=inputs={}:normalize=0:dropout_transition=0[aout]", parts.join(";"), parts.len());
        (inputs, Some(f))
    }

    /// Renders the sequence into `out`. `progress(fraction)` returns false to stop.
    pub fn export(&mut self, out: &Path, codec: Codec, mut progress: impl FnMut(f32) -> bool) -> Result<(), MediaError> {
        let total = self.seq.duration_ms().max(1);
        let (inputs, mix) = self.mix();
        let mut enc = Encoder::start_with(out, self.seq.size, self.fps, codec, &inputs, &[], None, mix.as_deref())?;
        let mut failed = None;
        let mut stopped = false;
        let step = 1000.0 / self.fps;
        self.play(0, |t, rgba| {
            if let Err(e) = enc.write(&rgba) {
                failed = Some(e);
                return false;
            }
            stopped = !progress(((t as f64 + step) / total as f64).min(1.0) as f32);
            !stopped
        });
        if let Some(e) = failed {
            return Err(e);
        }
        if stopped {
            drop(enc);
            let _ = std::fs::remove_file(out);
            return Err(MediaError::Failed("stopped".into()));
        }
        progress(1.0);
        enc.finish()
    }
}
