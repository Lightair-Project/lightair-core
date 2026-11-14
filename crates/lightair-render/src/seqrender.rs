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
use crate::sequence::{Clip, Media, MediaKind, Sequence, TrackKind, TransitionKind};

/// A layer, before it's handed to the compositor.
struct Pic {
    rgba: Vec<u8>,
    size: (u32, u32),
    center: (f32, f32),
    draw: (f32, f32),
    rotation: f32,
    opacity: f32,
    premultiplied: bool,
    filter: crate::sequence::Filter,
}

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
    /// Text and shapes, made once per content.
    made: crate::generate::Cache,
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
            made: Default::default(),
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

    /// Where a clip's picture sits (its centre and drawn size, result pixels) at `t`: by its
    /// transform there, the medium fitted to the frame (made pictures at their own size).
    pub fn placement(&self, clip: &Clip, media: Option<&Media>, made: Option<(u32, u32)>, t: u64) -> ((f32, f32), (f32, f32)) {
        let (w, h) = (self.seq.size.0 as f32, self.seq.size.1 as f32);
        let tr = clip.transform_at(t);
        let (bw, bh) = match (made, media) {
            (Some(s), _) => (s.0 as f32, s.1 as f32),
            (None, Some(m)) if m.take.is_none() && m.width > 0 => {
                let (_, _, bw, bh) = fit(m.width as f32 / m.height.max(1) as f32, 0.0, 0.0, w, h);
                (bw, bh)
            }
            _ => (w, h),
        };
        ((w / 2.0 + tr.x, h / 2.0 + tr.y), (bw * tr.scale, bh * tr.scale))
    }

    /// The picture of one clip at timeline time `t` (held at its edge frame outside it):
    /// RGBA, its size, and whether it's premultiplied (made pictures).
    fn picture(&mut self, clip: &Clip, t: u64) -> Option<(Vec<u8>, (u32, u32), bool)> {
        if let Some(content) = &clip.content {
            let r = self.made.get(content);
            return Some((r.rgba, r.size, true));
        }
        let media = self.seq.media(clip.media)?.clone();
        let s = clip.source_held(t);
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
            return Some((frame, self.seq.size, false));
        }
        if media.kind == MediaKind::Image {
            if let std::collections::hash_map::Entry::Vacant(e) = self.stills.entry(media.id) {
                let k = (2048.0 / media.width.max(media.height).max(1) as f32).min(1.0);
                let size = (even(media.width as f32 * k), even(media.height as f32 * k));
                e.insert((media::frame_at(&media.path, 0, size).ok()?, size));
            }
            return self.stills.get(&media.id).cloned().map(|(r, s)| (r, s, false));
        }
        // the size it's drawn at (enough for its largest scale), never more than the file's own
        let (_, draw) = self.placement(clip, Some(&media), None, t);
        let biggest = clip.keyframes.iter().map(|k| k.transform.scale).fold(clip.transform.scale, f32::max);
        let k = (draw.0 / clip.transform_at(t).scale.max(0.01) * biggest / media.width.max(1) as f32).min(1.0);
        let size = (even(media.width as f32 * k), even(media.height as f32 * k));
        if self.streams.get(&clip.id).is_none_or(|s| s.size() != size) {
            self.streams.insert(clip.id, Stream::new(&media.path, size, self.fps));
        }
        let rgba = self.streams.get_mut(&clip.id)?.frame(s)?.to_vec();
        Some((rgba, size, false))
    }

    /// One clip as a layer at `t`: faded by `opacity`, moved by `dx`, scaled by `grow`.
    fn layer_of(&mut self, clip: &Clip, t: u64, opacity: f32, dx: f32, grow: f32, out: &mut Vec<Pic>) {
        let Some((rgba, size, premultiplied)) = self.picture(clip, t) else { return };
        let media = self.seq.media(clip.media).cloned();
        let made = clip.content.as_ref().map(|_| size);
        let (center, draw) = self.placement(clip, media.as_ref(), made, t);
        let tr = clip.transform_at(t);
        out.push(Pic {
            rgba,
            size,
            center: (center.0 + dx, center.1),
            draw: (draw.0 * grow, draw.1 * grow),
            rotation: tr.rotation,
            opacity: tr.opacity * opacity,
            premultiplied,
            filter: clip.filter,
        });
    }

    /// The frame at timeline time `t`: RGBA at the sequence's size.
    pub fn frame(&mut self, t: u64) -> Vec<u8> {
        let w = self.seq.size.0 as f32;
        let tracks: Vec<Vec<Clip>> = self
            .seq
            .tracks
            .iter()
            .filter(|tr| tr.kind == TrackKind::Video && !tr.hidden)
            .rev()
            .map(|tr| tr.clips.clone())
            .collect();
        let mut pics: Vec<Pic> = Vec::new();
        for clips in tracks {
            let Some(i) = clips.iter().position(|c| c.start_ms <= t && t < c.end_ms()) else { continue };
            let c = &clips[i];
            // a transition across a cut: half in the clip before it, half in the clip after
            let across = match (c.transition, i.checked_sub(1).map(|p| &clips[p])) {
                (Some(tn), Some(prev)) if prev.end_ms() == c.start_ms && t < c.start_ms + tn.duration_ms / 2 => {
                    Some((prev.clone(), c.clone(), 0.5 + (t - c.start_ms) as f32 / tn.duration_ms.max(1) as f32, tn.kind))
                }
                _ => clips.get(i + 1).and_then(|n| {
                    let tn = n.transition?;
                    (n.start_ms == c.end_ms() && t + tn.duration_ms / 2 >= c.end_ms())
                        .then(|| (c.clone(), n.clone(), 0.5 - (c.end_ms() - t) as f32 / tn.duration_ms.max(1) as f32, tn.kind))
                }),
            };
            match across {
                None => self.layer_of(c, t, 1.0, 0.0, 1.0, &mut pics),
                Some((a, b, k, kind)) => {
                    let k = k.clamp(0.0, 1.0);
                    match kind {
                        TransitionKind::Crossfade => {
                            self.layer_of(&a, t, 1.0, 0.0, 1.0, &mut pics);
                            self.layer_of(&b, t, k, 0.0, 1.0, &mut pics);
                        }
                        TransitionKind::DipToBlack => {
                            if k < 0.5 {
                                self.layer_of(&a, t, 1.0 - 2.0 * k, 0.0, 1.0, &mut pics);
                            } else {
                                self.layer_of(&b, t, 2.0 * k - 1.0, 0.0, 1.0, &mut pics);
                            }
                        }
                        TransitionKind::Slide => {
                            let e = k * k * (3.0 - 2.0 * k);
                            self.layer_of(&a, t, 1.0, -e * w, 1.0, &mut pics);
                            self.layer_of(&b, t, 1.0, (1.0 - e) * w, 1.0, &mut pics);
                        }
                        TransitionKind::Zoom => {
                            self.layer_of(&a, t, 1.0, 0.0, 1.0 + 0.15 * k, &mut pics);
                            self.layer_of(&b, t, k, 0.0, 1.25 - 0.25 * k, &mut pics);
                        }
                    }
                }
            }
        }
        let layers: Vec<Layer> = pics
            .iter()
            .map(|p| Layer {
                rgba: &p.rgba,
                size: p.size,
                center: p.center,
                draw: p.draw,
                rotation: p.rotation,
                opacity: p.opacity,
                premultiplied: p.premultiplied,
                filter: p.filter,
            })
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
        mix_of(&self.seq)
    }

    /// Renders the sequence into `out`. `progress(fraction)` returns false to stop.
    pub fn export(&mut self, out: &Path, codec: Codec, mut progress: impl FnMut(f32) -> bool) -> Result<(), MediaError> {
        let total = self.seq.duration_ms().max(1);
        // voices to clean up, first
        crate::sound::prepare(&self.seq)?;
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

pub use crate::sound::{mix_of, sound_to};
