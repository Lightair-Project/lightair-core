//! A sequence (docs/plans/video-editor.md): the Editor's multi-track document. Media, video and
//! audio tracks, and clips placed on them. Times are milliseconds on the timeline unless a name
//! says `in`/`out` (the medium's own time).

use std::path::{Path, PathBuf};

use lightair_core::Id;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

/// A file the sequence uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub id: Id,
    pub name: String,
    pub kind: MediaKind,
    /// The picture or sound (a take: its screen video).
    pub path: PathBuf,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    /// Its own sound (videos), or the files that play with it (a take's stems).
    #[serde(default)]
    pub audio: Vec<PathBuf>,
    /// A Studio take's folder: drawn with its screen recording look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<PathBuf>,
    /// The Library item it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Id>,
    /// A take's look (its `Edit`: zooms, pointer, backdrop), kept in the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<PathBuf>,
}

impl Media {
    /// A file as a medium: what it holds, its size and length (from FFmpeg).
    pub fn from_file(path: &Path) -> Result<Media, crate::media::MediaError> {
        let c = crate::media::contents(path)?;
        let kind = if c.still {
            MediaKind::Image
        } else if c.video {
            MediaKind::Video
        } else {
            MediaKind::Audio
        };
        Ok(Media {
            id: Id::new(),
            name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            kind,
            path: path.to_path_buf(),
            duration_ms: c.duration_ms,
            width: c.width,
            height: c.height,
            audio: if c.audio && kind == MediaKind::Video { vec![path.to_path_buf()] } else { Vec::new() },
            take: None,
            item: None,
            look: None,
        })
    }

    /// A Studio take folder as a medium: its screen, drawn with its look, and its stems.
    pub fn from_take(folder: &Path) -> Option<Media> {
        let source = crate::export::Source::of_take(folder)?;
        let c = crate::media::contents(&source.video).ok()?;
        Some(Media {
            id: Id::new(),
            name: folder.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            kind: MediaKind::Video,
            path: source.video,
            duration_ms: c.duration_ms,
            width: c.width,
            height: c.height,
            audio: source.audio,
            take: Some(folder.to_path_buf()),
            item: None,
            look: None,
        })
    }
}

/// Where a picture sits: its centre's offset from the frame's centre (result pixels), its scale
/// (1: as large as fits the frame), its turn (degrees) and opacity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub rotation: f32,
    pub opacity: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform { x: 0.0, y: 0.0, scale: 1.0, rotation: 0.0, opacity: 1.0 }
    }
}

/// What a generated clip draws (no file behind it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    Text(TextStyle),
    Shape(ShapeStyle),
}

/// A text clip's words and look. Sizes are result pixels (at the sequence's size).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    pub text: String,
    pub size: f32,
    /// 0xRRGGBB.
    pub color: u32,
    pub bold: bool,
    /// 0 left, 1 centre, 2 right.
    pub align: u8,
    /// A box behind the words (0xRRGGBB, and its opacity).
    #[serde(default)]
    pub background: Option<(u32, f32)>,
    /// An outline around the letters.
    #[serde(default)]
    pub outline: Option<u32>,
    /// The widest a line goes before wrapping (result pixels).
    pub width: f32,
}

impl TextStyle {
    pub fn title(text: &str) -> TextStyle {
        TextStyle { text: text.into(), size: 96.0, color: 0xFFFFFF, bold: true, align: 1, background: None, outline: None, width: 1600.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    Rectangle,
    Ellipse,
    Arrow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapeStyle {
    pub shape: ShapeKind,
    pub width: f32,
    pub height: f32,
    /// 0xRRGGBB, and an opacity; `None`: not filled.
    pub fill: Option<(u32, f32)>,
    /// Line colour and width.
    pub stroke: Option<(u32, f32)>,
    /// Corner radius (rectangles).
    pub radius: f32,
}

/// How a clip arrives from the clip before it on its track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    Crossfade,
    DipToBlack,
    Slide,
    Zoom,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration_ms: u64,
}

/// Colour and look adjustments; zero (and 1 for contrast and saturation) is none.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// -1..1
    pub brightness: f32,
    /// 0..2
    pub contrast: f32,
    /// 0..2 (0: grey)
    pub saturation: f32,
    /// -1 cool .. 1 warm
    pub temperature: f32,
    /// 0..1
    pub vignette: f32,
    /// 0..1
    pub blur: f32,
}

impl Default for Filter {
    fn default() -> Self {
        Filter { brightness: 0.0, contrast: 1.0, saturation: 1.0, temperature: 0.0, vignette: 0.0, blur: 0.0 }
    }
}

impl Filter {
    /// Lightair's looks, by name.
    pub const PRESETS: &[(&str, Filter)] = &[
        ("None", Filter { brightness: 0.0, contrast: 1.0, saturation: 1.0, temperature: 0.0, vignette: 0.0, blur: 0.0 }),
        ("Vivid", Filter { brightness: 0.02, contrast: 1.12, saturation: 1.35, temperature: 0.0, vignette: 0.0, blur: 0.0 }),
        ("Warm", Filter { brightness: 0.02, contrast: 1.05, saturation: 1.08, temperature: 0.45, vignette: 0.15, blur: 0.0 }),
        ("Cool", Filter { brightness: 0.0, contrast: 1.05, saturation: 0.95, temperature: -0.45, vignette: 0.0, blur: 0.0 }),
        ("Mono", Filter { brightness: 0.0, contrast: 1.15, saturation: 0.0, temperature: 0.0, vignette: 0.2, blur: 0.0 }),
        ("Faded", Filter { brightness: 0.06, contrast: 0.82, saturation: 0.75, temperature: 0.1, vignette: 0.0, blur: 0.0 }),
        ("Cinematic", Filter { brightness: -0.03, contrast: 1.2, saturation: 0.85, temperature: -0.15, vignette: 0.35, blur: 0.0 }),
    ];

    pub fn is_none(&self) -> bool {
        *self == Filter::default()
    }
}

/// The transform at a moment of a clip (from its start, timeline time).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    pub at_ms: u64,
    pub transform: Transform,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: Id,
    pub media: Id,
    /// Where it starts on the timeline.
    pub start_ms: u64,
    /// The piece of the medium it plays.
    pub in_ms: u64,
    pub out_ms: u64,
    pub speed: f32,
    /// 1: as recorded.
    pub volume: f32,
    pub muted: bool,
    #[serde(default)]
    pub transform: Transform,
    /// Generated content (text, a shape); `media` is unused then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    /// How it arrives from the clip just before it on its track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
    #[serde(default, skip_serializing_if = "Filter::is_none")]
    pub filter: Filter,
    /// The transform over time, when it moves (in time order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyframes: Vec<Keyframe>,
    /// Its sound faded in from silence and out to it (timeline ms).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_in_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_out_ms: u64,
    /// The volume over time, when it changes (in time order; `volume` otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_keys: Vec<VolumeKey>,
    /// A voice cleaned up (noise, harshness, level) before it's mixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<VoiceClean>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// The volume at a moment of a clip (from its start, timeline time).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VolumeKey {
    pub at_ms: u64,
    pub volume: f32,
}

/// Voice clean-up: the audio engine's Voice Clarity chain, run on the file once.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VoiceClean {
    /// 0 light .. 1 strong.
    pub amount: f32,
    /// Warmer and denser, for narration.
    #[serde(default)]
    pub podcast: bool,
}

impl Default for VoiceClean {
    fn default() -> Self {
        VoiceClean { amount: 0.5, podcast: false }
    }
}

/// Smooth in and out.
fn ease(k: f32) -> f32 {
    let k = k.clamp(0.0, 1.0);
    k * k * (3.0 - 2.0 * k)
}

fn mix_transform(a: &Transform, b: &Transform, k: f32) -> Transform {
    let l = |x: f32, y: f32| x + (y - x) * k;
    Transform { x: l(a.x, b.x), y: l(a.y, b.y), scale: l(a.scale, b.scale), rotation: l(a.rotation, b.rotation), opacity: l(a.opacity, b.opacity) }
}

impl Clip {
    pub fn new(media: Id, start_ms: u64, len_ms: u64) -> Clip {
        Clip {
            id: Id::new(),
            media,
            start_ms,
            in_ms: 0,
            out_ms: len_ms,
            speed: 1.0,
            volume: 1.0,
            muted: false,
            transform: Transform::default(),
            content: None,
            transition: None,
            filter: Filter::default(),
            keyframes: Vec::new(),
            fade_in_ms: 0,
            fade_out_ms: 0,
            volume_keys: Vec::new(),
            voice: None,
        }
    }

    /// Its volume at timeline time `t` (its volume keys, straight between, or its volume).
    pub fn volume_at(&self, t: u64) -> f32 {
        let at = t.saturating_sub(self.start_ms);
        let k = &self.volume_keys;
        match (k.first(), k.last()) {
            (None, _) | (_, None) => self.volume,
            (Some(f), _) if at <= f.at_ms => f.volume,
            (_, Some(l)) if at >= l.at_ms => l.volume,
            _ => {
                let i = k.partition_point(|f| f.at_ms <= at);
                let (a, b) = (&k[i - 1], &k[i]);
                let x = (at - a.at_ms) as f32 / (b.at_ms - a.at_ms).max(1) as f32;
                a.volume + (b.volume - a.volume) * x
            }
        }
    }

    /// Sets its volume at `t`: a key there when it has keys, else the volume.
    pub fn set_volume_at(&mut self, t: u64, v: f32) {
        if self.volume_keys.is_empty() {
            self.volume = v;
            return;
        }
        let at = t.saturating_sub(self.start_ms).min(self.len_ms());
        match self.volume_keys.iter_mut().find(|k| k.at_ms.abs_diff(at) < 40) {
            Some(k) => k.volume = v,
            None => {
                self.volume_keys.push(VolumeKey { at_ms: at, volume: v });
                self.volume_keys.sort_by_key(|k| k.at_ms);
            }
        }
    }

    /// How loud it is at `t`: its volume, faded in and out (0 when muted).
    pub fn gain_at(&self, t: u64) -> f32 {
        if self.muted {
            return 0.0;
        }
        let at = t.saturating_sub(self.start_ms) as f32;
        let left = self.end_ms().saturating_sub(t) as f32;
        let fin = if self.fade_in_ms > 0 { (at / self.fade_in_ms as f32).min(1.0) } else { 1.0 };
        let fout = if self.fade_out_ms > 0 { (left / self.fade_out_ms as f32).min(1.0) } else { 1.0 };
        self.volume_at(t) * fin * fout
    }

    /// Whether the volume ever changes along the clip (keys, fades).
    pub fn sound_shaped(&self) -> bool {
        !self.volume_keys.is_empty() || self.fade_in_ms > 0 || self.fade_out_ms > 0
    }

    /// Its transform at timeline time `t` (its keyframes eased between, or its one transform).
    pub fn transform_at(&self, t: u64) -> Transform {
        let at = t.saturating_sub(self.start_ms);
        let k = &self.keyframes;
        match (k.first(), k.last()) {
            (None, _) | (_, None) => self.transform,
            (Some(f), _) if at <= f.at_ms => f.transform,
            (_, Some(l)) if at >= l.at_ms => l.transform,
            _ => {
                let i = k.partition_point(|f| f.at_ms <= at);
                let (a, b) = (&k[i - 1], &k[i]);
                let span = (b.at_ms - a.at_ms).max(1) as f32;
                mix_transform(&a.transform, &b.transform, ease((at - a.at_ms) as f32 / span))
            }
        }
    }

    /// Sets its transform at `t`: a keyframe there when it has keyframes, else the transform.
    pub fn set_transform_at(&mut self, t: u64, tr: Transform) {
        if self.keyframes.is_empty() {
            self.transform = tr;
            return;
        }
        let at = t.saturating_sub(self.start_ms).min(self.len_ms());
        match self.keyframes.iter_mut().find(|k| k.at_ms.abs_diff(at) < 40) {
            Some(k) => k.transform = tr,
            None => {
                self.keyframes.push(Keyframe { at_ms: at, transform: tr });
                self.keyframes.sort_by_key(|k| k.at_ms);
            }
        }
    }

    /// The medium's time at `t`, held at the clip's first or last frame outside it (for
    /// transitions, which show a clip a little beyond its edge).
    pub fn source_held(&self, t: u64) -> u64 {
        let t = t.clamp(self.start_ms, self.end_ms().saturating_sub(1).max(self.start_ms));
        self.source_at(t).unwrap_or(self.in_ms)
    }

    /// Its length on the timeline.
    pub fn len_ms(&self) -> u64 {
        ((self.out_ms.saturating_sub(self.in_ms)) as f64 / self.speed.max(0.05) as f64).round() as u64
    }

    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.len_ms()
    }

    /// The medium's time at timeline time `t` (`None` outside the clip).
    pub fn source_at(&self, t: u64) -> Option<u64> {
        (t >= self.start_ms && t < self.end_ms())
            .then(|| (self.in_ms + ((t - self.start_ms) as f64 * self.speed.max(0.05) as f64) as u64).min(self.out_ms.saturating_sub(1)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Id,
    pub kind: TrackKind,
    /// Clips in time order, never overlapping.
    pub clips: Vec<Clip>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub locked: bool,
    /// The track captions were made on (made again in place).
    #[serde(default)]
    pub captions: bool,
}

impl Track {
    fn new(kind: TrackKind) -> Track {
        Track { id: Id::new(), kind, clips: Vec::new(), muted: false, hidden: false, locked: false, captions: false }
    }

    /// Whether `start..end` is free on this track (ignoring `except`).
    pub fn free(&self, start: u64, end: u64, except: Option<Id>) -> bool {
        self.clips.iter().filter(|c| Some(c.id) != except).all(|c| end <= c.start_ms || start >= c.end_ms())
    }

    fn sort(&mut self) {
        self.clips.sort_by_key(|c| c.start_ms);
    }
}

/// The sequence. Tracks are in display order: video tracks first, the top one in front; then
/// audio tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub version: u32,
    pub size: (u32, u32),
    pub fps: u32,
    pub media: Vec<Media>,
    pub tracks: Vec<Track>,
}

impl Sequence {
    /// Empty: one video track and one audio track.
    pub fn new(size: (u32, u32), fps: u32) -> Sequence {
        Sequence { version: 1, size, fps, media: Vec::new(), tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)] }
    }

    pub fn load(path: &Path) -> std::io::Result<Sequence> {
        serde_json::from_slice(&std::fs::read(path)?).map_err(std::io::Error::other)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn duration_ms(&self) -> u64 {
        self.tracks.iter().flat_map(|t| t.clips.iter().map(Clip::end_ms)).max().unwrap_or(0)
    }

    pub fn media(&self, id: Id) -> Option<&Media> {
        self.media.iter().find(|m| m.id == id)
    }

    /// Adds a medium (once per file); its id.
    pub fn add_media(&mut self, m: Media) -> Id {
        if let Some(e) = self.media.iter().find(|e| e.path == m.path) {
            return e.id;
        }
        let id = m.id;
        self.media.push(m);
        id
    }

    /// (track, clip) indices of a clip.
    pub fn find(&self, clip: Id) -> Option<(usize, usize)> {
        self.tracks.iter().enumerate().find_map(|(t, tr)| tr.clips.iter().position(|c| c.id == clip).map(|c| (t, c)))
    }

    pub fn clip(&self, id: Id) -> Option<&Clip> {
        self.find(id).map(|(t, c)| &self.tracks[t].clips[c])
    }

    pub fn clip_mut(&mut self, id: Id) -> Option<&mut Clip> {
        self.find(id).map(|(t, c)| &mut self.tracks[t].clips[c])
    }

    fn kind_for(m: &Media) -> TrackKind {
        if m.kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video }
    }

    /// A track of `kind` free over `start..end`, the preferred one first; a new one if none is
    /// (video tracks go above the others, audio tracks below).
    fn free_track(&mut self, kind: TrackKind, preferred: Option<usize>, start: u64, end: u64) -> usize {
        if let Some(p) = preferred.filter(|p| self.tracks.get(*p).is_some_and(|t| t.kind == kind && t.free(start, end, None))) {
            return p;
        }
        // the bottom-most free video track (the main one), the top-most free audio track
        let found = match kind {
            TrackKind::Video => self.tracks.iter().rposition(|t| t.kind == kind && t.free(start, end, None)),
            TrackKind::Audio => self.tracks.iter().position(|t| t.kind == kind && t.free(start, end, None)),
        };
        if let Some(i) = found {
            return i;
        }
        match kind {
            TrackKind::Video => {
                self.tracks.insert(0, Track::new(TrackKind::Video));
                0
            }
            TrackKind::Audio => {
                self.tracks.push(Track::new(TrackKind::Audio));
                self.tracks.len() - 1
            }
        }
    }

    /// Places all of a medium at `start` (on `track` if it's free there); the new clip's id.
    pub fn place(&mut self, media: Id, start: u64, track: Option<usize>) -> Option<Id> {
        let m = self.media(media)?.clone();
        let len = if m.kind == MediaKind::Image { 5_000 } else { m.duration_ms.max(1) };
        let clip = Clip::new(media, start, len);
        let t = self.free_track(Self::kind_for(&m), track, start, start + len);
        let id = clip.id;
        self.tracks[t].clips.push(clip);
        self.tracks[t].sort();
        Some(id)
    }

    /// Places generated content (text, a shape) at `start` for `len` on the top free video track
    /// (a new one above if none is free); its id.
    pub fn place_content(&mut self, content: Content, start: u64, len: u64) -> Id {
        self.place_content_on(content, start, len, None)
    }

    /// A made picture placed on `track` when that's a video track free then, else as
    /// [`Sequence::place_content`] does.
    pub fn place_content_on(&mut self, content: Content, start: u64, len: u64, track: Option<usize>) -> Id {
        let mut clip = Clip::new(Id::nil(), start, len);
        clip.content = Some(content);
        let wanted = track.filter(|t| self.tracks.get(*t).is_some_and(|tr| tr.kind == TrackKind::Video && !tr.captions && tr.free(start, start + len, None)));
        // over the pictures: the top free video track
        let t = match wanted.or_else(|| self.tracks.iter().position(|t| t.kind == TrackKind::Video && t.free(start, start + len, None))) {
            Some(i) if wanted == Some(i) => i,
            Some(i) if !self.tracks[i].captions && self.tracks[i].clips.iter().all(|c| c.content.is_some()) => i,
            _ => {
                self.tracks.insert(0, Track::new(TrackKind::Video));
                0
            }
        };
        let id = clip.id;
        self.tracks[t].clips.push(clip);
        self.tracks[t].sort();
        id
    }

    /// A video clip's sound as a clip of its own on an audio track (the video clip muted). The
    /// new clip's id; `None` for clips without sound.
    pub fn detach_sound(&mut self, clip: Id) -> Option<Id> {
        let (t, c) = self.find(clip)?;
        let cl = self.tracks[t].clips[c].clone();
        let m = self.media(cl.media)?;
        if self.tracks[t].kind != TrackKind::Video || cl.content.is_some() || m.audio.is_empty() || cl.muted {
            return None;
        }
        let at = self.free_track(TrackKind::Audio, None, cl.start_ms, cl.end_ms());
        let sound = Clip {
            id: Id::new(),
            transform: Transform::default(),
            transition: None,
            filter: Filter::default(),
            keyframes: Vec::new(),
            ..cl
        };
        let id = sound.id;
        self.tracks[at].clips.push(sound);
        self.tracks[at].sort();
        if let Some(v) = self.clip_mut(clip) {
            v.muted = true;
            v.volume_keys.clear();
            v.fade_in_ms = 0;
            v.fade_out_ms = 0;
            v.voice = None;
        }
        Some(id)
    }

    /// Captions: a text clip for each line (start, end, text) in `style` at `place`, on the captions track
    /// (made at the top, or emptied first). The track's index.
    pub fn set_captions(&mut self, lines: &[(u64, u64, String)], style: &TextStyle, place: Transform) -> usize {
        let t = match self.tracks.iter().position(|t| t.captions) {
            Some(t) => t,
            None => {
                let mut track = Track::new(TrackKind::Video);
                track.captions = true;
                self.tracks.insert(0, track);
                0
            }
        };
        let mut clips: Vec<Clip> = Vec::new();
        for (a, b, text) in lines {
            // never over the line before; at least 0.4 s on screen
            let a = (*a).max(clips.last().map_or(0, Clip::end_ms));
            let b = (*b).max(a + 400);
            let mut c = Clip::new(Id::nil(), a, b - a);
            c.content = Some(Content::Text(TextStyle { text: text.clone(), ..style.clone() }));
            c.transform = place;
            clips.push(c);
        }
        // the gap before the next line closed, so they don't flicker
        for i in 1..clips.len() {
            let (start, end) = (clips[i].start_ms, clips[i - 1].end_ms());
            if start > end && start - end < 300 {
                clips[i - 1].out_ms += start - end;
            }
        }
        self.tracks[t].clips = clips;
        t
    }

    /// The caption style: white words in a dark box, low in the frame.
    pub fn caption_style(&self) -> (TextStyle, Transform) {
        let h = self.size.1 as f32;
        let style = TextStyle {
            text: String::new(),
            size: (h / 18.0).max(18.0),
            color: 0xFFFFFF,
            bold: true,
            align: 1,
            background: Some((0x000000, 0.6)),
            outline: None,
            width: self.size.0 as f32 * 0.8,
        };
        (style, Transform { y: h * 0.38, ..Transform::default() })
    }

    /// The clip just before `clip` on its track, if it ends where `clip` starts.
    pub fn before(&self, clip: Id) -> Option<&Clip> {
        let (t, c) = self.find(clip)?;
        let me = &self.tracks[t].clips[c];
        c.checked_sub(1).map(|p| &self.tracks[t].clips[p]).filter(|p| p.end_ms() == me.start_ms)
    }

    /// Places a medium at the end of the main track (the bottom video track, or the first audio
    /// track for sound).
    pub fn append(&mut self, media: Id) -> Option<Id> {
        let kind = Self::kind_for(self.media(media)?);
        let t = match kind {
            TrackKind::Video => self.tracks.iter().rposition(|t| t.kind == kind),
            TrackKind::Audio => self.tracks.iter().position(|t| t.kind == kind),
        };
        let end = t.and_then(|t| self.tracks[t].clips.last().map(Clip::end_ms)).unwrap_or(0);
        self.place(media, end, t)
    }

    /// Moves a clip to `start` on `track` (same kind). False (and nothing moves) if it would
    /// overlap another clip there.
    pub fn move_clip(&mut self, clip: Id, track: usize, start: u64) -> bool {
        let Some((t, c)) = self.find(clip) else { return false };
        let Some(dest) = self.tracks.get(track) else { return false };
        let len = self.tracks[t].clips[c].len_ms();
        if dest.kind != self.tracks[t].kind || dest.locked || !dest.free(start, start + len, Some(clip)) {
            return false;
        }
        let mut moved = self.tracks[t].clips.remove(c);
        moved.start_ms = start;
        self.tracks[track].clips.push(moved);
        self.tracks[track].sort();
        true
    }

    /// Moves a clip's left edge to timeline time `at` (its in point follows), within its medium
    /// and its neighbours.
    pub fn trim_start(&mut self, clip: Id, at: u64) {
        let Some((t, c)) = self.find(clip) else { return };
        let prev_end = if c > 0 { self.tracks[t].clips[c - 1].end_ms() } else { 0 };
        let cl = &mut self.tracks[t].clips[c];
        let speed = cl.speed.max(0.05) as f64;
        let end = cl.end_ms();
        let was = cl.start_ms;
        if cl.content.is_some() {
            // made pictures have no medium to run out of
            let at = at.clamp(prev_end, end.saturating_sub(100));
            cl.start_ms = at;
            cl.in_ms = 0;
            cl.out_ms = ((end - at) as f64 * speed).round() as u64;
        } else {
            // the earliest the edge can go: the medium's start, the clip before
            let earliest = cl.start_ms.saturating_sub((cl.in_ms as f64 / speed) as u64).max(prev_end);
            let at = at.clamp(earliest, end.saturating_sub(100));
            let in_ms = cl.in_ms as i64 + ((at as i64 - cl.start_ms as i64) as f64 * speed) as i64;
            cl.in_ms = in_ms.max(0) as u64;
            cl.start_ms = at;
        }
        // keyframes stay where they were on the timeline
        let moved = cl.start_ms as i64 - was as i64;
        for k in &mut cl.keyframes {
            k.at_ms = (k.at_ms as i64 - moved).max(0) as u64;
        }
        for k in &mut cl.volume_keys {
            k.at_ms = (k.at_ms as i64 - moved).max(0) as u64;
        }
    }

    /// Moves a clip's right edge to timeline time `at` (its out point follows), within its medium
    /// (images stretch freely) and the clip after.
    pub fn trim_end(&mut self, clip: Id, at: u64) {
        let Some((t, c)) = self.find(clip) else { return };
        let next_start = self.tracks[t].clips.get(c + 1).map_or(u64::MAX, |n| n.start_ms);
        let media = self.tracks[t].clips[c].media;
        let (limit, image) = self.media(media).map_or((u64::MAX, false), |m| (m.duration_ms, m.kind == MediaKind::Image));
        let cl = &mut self.tracks[t].clips[c];
        let speed = cl.speed.max(0.05) as f64;
        let at = at.clamp(cl.start_ms + 100, next_start);
        let out = cl.in_ms + ((at - cl.start_ms) as f64 * speed) as u64;
        cl.out_ms = if image { out } else { out.min(limit) };
    }

    /// Cuts a clip at timeline time `at`: the second half becomes a clip of its own (its id).
    pub fn split(&mut self, clip: Id, at: u64) -> Option<Id> {
        let (t, c) = self.find(clip)?;
        let cl = self.tracks[t].clips[c].clone();
        let s = cl.source_at(at).filter(|_| at > cl.start_ms)?;
        // keyframes on either side of the cut, with the transform at the cut on both
        let cut = at - cl.start_ms;
        let here = cl.transform_at(at);
        let (mut before, mut after) = (Vec::new(), Vec::new());
        if !cl.keyframes.is_empty() {
            before = cl.keyframes.iter().filter(|k| k.at_ms < cut).copied().collect();
            before.push(Keyframe { at_ms: cut, transform: here });
            after.push(Keyframe { at_ms: 0, transform: here });
            after.extend(cl.keyframes.iter().filter(|k| k.at_ms > cut).map(|k| Keyframe { at_ms: k.at_ms - cut, transform: k.transform }));
        }
        let loud = cl.volume_at(at);
        let (mut vbefore, mut vafter) = (Vec::new(), Vec::new());
        if !cl.volume_keys.is_empty() {
            vbefore = cl.volume_keys.iter().filter(|k| k.at_ms < cut).copied().collect();
            vbefore.push(VolumeKey { at_ms: cut, volume: loud });
            vafter.push(VolumeKey { at_ms: 0, volume: loud });
            vafter.extend(cl.volume_keys.iter().filter(|k| k.at_ms > cut).map(|k| VolumeKey { at_ms: k.at_ms - cut, volume: k.volume }));
        }
        let first = &mut self.tracks[t].clips[c];
        first.out_ms = s;
        first.keyframes = before;
        first.volume_keys = vbefore;
        // a fade belongs to the end it was on
        first.fade_out_ms = 0;
        // a transition belongs to the cut it was on: the first half's start
        let second = Clip { id: Id::new(), start_ms: at, in_ms: s, keyframes: after, volume_keys: vafter, transition: None, fade_in_ms: 0, ..cl };
        let id = second.id;
        self.tracks[t].clips.insert(c + 1, second);
        Some(id)
    }

    /// Cuts every unlocked clip under the playhead (CapCut's split with nothing selected).
    pub fn split_all(&mut self, at: u64) -> Vec<Id> {
        let under: Vec<Id> = self
            .tracks
            .iter()
            .filter(|t| !t.locked)
            .flat_map(|t| t.clips.iter().filter(|c| c.start_ms < at && at < c.end_ms()).map(|c| c.id))
            .collect();
        under.into_iter().filter_map(|c| self.split(c, at)).collect()
    }

    /// Sets a clip's speed (its length follows). False if it would then run into the next clip.
    pub fn set_speed(&mut self, clip: Id, speed: f32) -> bool {
        let Some((t, c)) = self.find(clip) else { return false };
        let mut changed = self.tracks[t].clips[c].clone();
        changed.speed = speed.clamp(0.1, 16.0);
        let next = self.tracks[t].clips.get(c + 1).map_or(u64::MAX, |n| n.start_ms);
        if changed.end_ms() > next {
            return false;
        }
        self.tracks[t].clips[c] = changed;
        true
    }

    pub fn remove(&mut self, clip: Id) {
        if let Some((t, c)) = self.find(clip) {
            self.tracks[t].clips.remove(c);
        }
    }

    /// Removes empty tracks beyond one of each kind.
    pub fn tidy(&mut self) {
        for kind in [TrackKind::Video, TrackKind::Audio] {
            while self.tracks.iter().filter(|t| t.kind == kind).count() > 1 {
                let Some(i) = self.tracks.iter().position(|t| t.kind == kind && t.clips.is_empty()) else { break };
                self.tracks.remove(i);
            }
        }
    }

    /// Times things snap to: every clip's edges, and the start.
    pub fn snap_points(&self, except: Option<Id>) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().filter(|c| Some(c.id) != except).flat_map(|c| [c.start_ms, c.end_ms()]))
            .collect();
        v.push(0);
        v.sort();
        v.dedup();
        v
    }

    /// The video clips showing at `t`, back to front, with their track.
    pub fn video_at(&self, t: u64) -> Vec<(usize, &Clip)> {
        let mut out: Vec<(usize, &Clip)> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Video && !tr.hidden)
            .filter_map(|(i, tr)| tr.clips.iter().find(|c| c.start_ms <= t && t < c.end_ms()).map(|c| (i, c)))
            .collect();
        // the top track (index 0) is in front: drawn last
        out.reverse();
        out
    }
}

/// An Editor project (docs/plans/video-editor.md): a named sequence, one file in the workspace's
/// `edits` folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditProject {
    pub id: Id,
    pub name: String,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub sequence: Sequence,
}

/// A project as the list shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectInfo {
    pub id: Id,
    pub name: String,
    pub modified_ms: i64,
    pub duration_ms: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl EditProject {
    pub fn new(name: &str, size: (u32, u32), fps: u32) -> EditProject {
        let now = now_ms();
        EditProject { id: Id::new(), name: name.into(), created_ms: now, modified_ms: now, sequence: Sequence::new(size, fps) }
    }

    pub fn dir(workspace: &Path) -> PathBuf {
        workspace.join("edits")
    }

    fn path(workspace: &Path, id: Id) -> PathBuf {
        Self::dir(workspace).join(format!("{id}.json"))
    }

    /// The workspace's projects, the latest changed first.
    pub fn list(workspace: &Path) -> Vec<ProjectInfo> {
        let mut out: Vec<ProjectInfo> = std::fs::read_dir(Self::dir(workspace))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice::<EditProject>(&std::fs::read(e.path()).ok()?).ok())
            .map(|p| ProjectInfo { id: p.id, name: p.name.clone(), modified_ms: p.modified_ms, duration_ms: p.sequence.duration_ms() })
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.modified_ms));
        out
    }

    pub fn load(workspace: &Path, id: Id) -> std::io::Result<EditProject> {
        serde_json::from_slice(&std::fs::read(Self::path(workspace, id))?).map_err(std::io::Error::other)
    }

    /// Saves it (whole, through a temporary file), marked changed now.
    pub fn save(&mut self, workspace: &Path) -> std::io::Result<()> {
        self.modified_ms = now_ms();
        let path = Self::path(workspace, self.id);
        std::fs::create_dir_all(Self::dir(workspace))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Removes the project (its file; the media it uses stay).
    pub fn delete(workspace: &Path, id: Id) -> std::io::Result<()> {
        std::fs::remove_file(Self::path(workspace, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_files_in_the_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let mut a = EditProject::new("Intro", (1920, 1080), 30);
        a.save(ws.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut b = EditProject::new("Tutorial", (1080, 1920), 60);
        b.save(ws.path()).unwrap();
        let list = EditProject::list(ws.path());
        assert_eq!(list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Tutorial", "Intro"], "latest first");
        assert_eq!(EditProject::load(ws.path(), a.id).unwrap().name, "Intro");
        EditProject::delete(ws.path(), a.id).unwrap();
        assert_eq!(EditProject::list(ws.path()).len(), 1);
    }

    fn video(seq: &mut Sequence, name: &str, ms: u64) -> Id {
        seq.add_media(Media {
            id: Id::new(),
            name: name.into(),
            kind: MediaKind::Video,
            path: PathBuf::from(format!("/m/{name}.mp4")),
            duration_ms: ms,
            width: 1920,
            height: 1080,
            audio: vec![PathBuf::from(format!("/m/{name}.mp4"))],
            take: None,
            item: None,
            look: None,
        })
    }

    #[test]
    fn clips_go_end_to_end_and_overlaps_go_up_a_track() {
        let mut s = Sequence::new((1920, 1080), 30);
        let a = video(&mut s, "a", 10_000);
        let b = video(&mut s, "b", 4_000);
        let ca = s.append(a).unwrap();
        let cb = s.append(b).unwrap();
        assert_eq!(s.clip(cb).unwrap().start_ms, 10_000, "after the first");
        assert_eq!(s.duration_ms(), 14_000);
        // placed over the first clip: a new track above it
        let over = s.place(b, 2_000, None).unwrap();
        let (t_over, _) = s.find(over).unwrap();
        let (t_main, _) = s.find(ca).unwrap();
        assert!(t_over < t_main, "above (in front)");
        let shown: Vec<Id> = s.video_at(3_000).iter().map(|(_, c)| c.id).collect();
        assert_eq!(shown, vec![ca, over], "back to front");
        // a move onto an occupied place is refused
        assert!(!s.move_clip(over, t_main, 1_000));
        assert!(s.move_clip(over, t_main, 14_000));
        s.tidy();
        assert_eq!(s.tracks.iter().filter(|t| t.kind == TrackKind::Video).count(), 1);
    }

    #[test]
    fn trims_and_splits_keep_the_medium_in_step() {
        let mut s = Sequence::new((1920, 1080), 30);
        let a = video(&mut s, "a", 10_000);
        let c = s.append(a).unwrap();
        s.trim_start(c, 2_000);
        let cl = s.clip(c).unwrap().clone();
        assert_eq!((cl.start_ms, cl.in_ms, cl.out_ms), (2_000, 2_000, 10_000));
        s.trim_end(c, 50_000);
        assert_eq!(s.clip(c).unwrap().out_ms, 10_000, "not past the medium's end");
        s.clip_mut(c).unwrap().speed = 2.0;
        assert_eq!(s.clip(c).unwrap().len_ms(), 4_000);
        let second = s.split(c, 4_000).unwrap();
        let (first, second) = (s.clip(c).unwrap().clone(), s.clip(second).unwrap().clone());
        assert_eq!((first.in_ms, first.out_ms), (2_000, 6_000));
        assert_eq!((second.start_ms, second.in_ms, second.out_ms), (4_000, 6_000, 10_000));
        assert_eq!(second.source_at(5_000), Some(8_000));
        assert!(s.snap_points(None).contains(&4_000));
    }

    #[test]
    fn titles_keyframes_split_and_trim() {
        let mut s = Sequence::new((1920, 1080), 30);
        let t = s.place_content(Content::Text(TextStyle::title("Hi")), 1_000, 4_000);
        assert_eq!(s.tracks[0].kind, TrackKind::Video);
        let c = s.clip_mut(t).unwrap();
        c.keyframes = vec![
            Keyframe { at_ms: 0, transform: Transform { x: 0.0, ..Transform::default() } },
            Keyframe { at_ms: 4_000, transform: Transform { x: 400.0, ..Transform::default() } },
        ];
        assert_eq!(c.transform_at(3_000).x, 200.0, "halfway, eased");
        assert!(c.transform_at(1_500).x < 50.0, "slow at first");
        let second = s.split(t, 3_000).unwrap();
        assert_eq!(s.clip(t).unwrap().transform_at(3_000).x, 200.0);
        assert_eq!(s.clip(second).unwrap().transform_at(3_000).x, 200.0);
        assert_eq!(s.clip(second).unwrap().transform_at(5_000).x, 400.0);
        // a made picture stretches either way; its keyframes stay put on the timeline
        s.trim_start(t, 0);
        let first = s.clip(t).unwrap();
        assert_eq!((first.start_ms, first.end_ms()), (0, 3_000));
        assert_eq!(first.transform_at(1_000).x, 0.0);
        assert_eq!(first.transform_at(3_000).x, 200.0);
        s.trim_end(second, 9_000);
        assert_eq!(s.clip(second).unwrap().end_ms(), 9_000);
    }
}
