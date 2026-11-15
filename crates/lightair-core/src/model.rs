//! The project document (docs/07-scenes-layers-canvas.md, docs/14-data-persistence-settings.md).
//!
//! Snapshots are immutable and share structure: scenes and inputs sit behind `Arc`, so cloning
//! a `Project` to apply a command copies pointers, and unchanged parts compare by pointer.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for every entity. UUIDv7: unique and roughly time-ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(pub Uuid);

impl Id {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// The id of nothing (a placeholder where an id is needed but none applies).
    pub const fn nil() -> Self {
        Self(Uuid::nil())
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A complete show setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    /// Output canvas shapes. The first is the primary format (libobs main canvas).
    pub formats: Vec<Format>,
    pub inputs: BTreeMap<Id, Arc<Input>>,
    /// Scenes in display order.
    pub scenes: Vec<Arc<Scene>>,
    /// The scene on air.
    pub program: Option<Id>,
    /// Global audio tracks in mixer order (docs/10-audio.md).
    #[serde(default)]
    pub tracks: Vec<Arc<Track>>,
    /// Buses, mixes and the master.
    #[serde(default)]
    pub audio: AudioSetup,
    /// Recording and streaming settings (docs/11-outputs-streaming-recording.md). Stream keys
    /// are not here: they live in the OS credential store, keyed by destination id.
    #[serde(default)]
    pub outputs: OutputSettings,
    /// How scene switches look (docs/07-scenes-layers-canvas.md, "Transitions").
    #[serde(default)]
    pub transition: TransitionSettings,
    /// The stinger video used by the Stinger transition.
    #[serde(default)]
    pub stinger: Option<Stinger>,
}

/// A stinger: a video (usually with alpha) that plays over the switch; the scene changes
/// underneath at `cut_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stinger {
    pub path: String,
    pub cut_ms: u32,
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            formats: vec![Format::landscape()],
            inputs: BTreeMap::new(),
            scenes: Vec::new(),
            program: None,
            tracks: Vec::new(),
            audio: AudioSetup::default(),
            outputs: OutputSettings::default(),
            transition: TransitionSettings::default(),
            stinger: None,
        }
    }

    /// The same project (id, name, formats) with no content: the baseline for a full rebuild,
    /// `diff(&p.empty_like(), &p)`.
    pub fn empty_like(&self) -> Self {
        Self {
            id: self.id,
            name: self.name.clone(),
            formats: self.formats.clone(),
            inputs: BTreeMap::new(),
            scenes: Vec::new(),
            program: None,
            tracks: Vec::new(),
            audio: AudioSetup::default(),
            outputs: self.outputs.clone(),
            transition: self.transition,
            stinger: self.stinger.clone(),
        }
    }

    pub fn primary_format(&self) -> &Format {
        &self.formats[0]
    }

    pub fn scene(&self, id: Id) -> Option<&Arc<Scene>> {
        self.scenes.iter().find(|s| s.id == id)
    }

    pub fn scene_index(&self, id: Id) -> Option<usize> {
        self.scenes.iter().position(|s| s.id == id)
    }

    pub fn format(&self, id: Id) -> Option<&Format> {
        self.formats.iter().find(|f| f.id == id)
    }

    pub fn track(&self, id: Id) -> Option<&Arc<Track>> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_index(&self, id: Id) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// Layers anywhere in the project that use `input`.
    pub fn layers_using(&self, input: Id) -> impl Iterator<Item = (&Scene, &Layer)> {
        self.scenes
            .iter()
            .flat_map(move |s| s.layers.iter().filter(move |l| l.input == Some(input)).map(move |l| (&**s, l)))
    }
}

/// An output canvas shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Format {
    pub id: Id,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl Format {
    pub fn landscape() -> Self {
        Self { id: Id::new(), name: "Landscape".into(), width: 1920, height: 1080, fps_num: 60, fps_den: 1 }
    }
    pub fn vertical() -> Self {
        Self { id: Id::new(), name: "Vertical".into(), width: 1080, height: 1920, fps_num: 60, fps_den: 1 }
    }
    pub fn square() -> Self {
        Self { id: Id::new(), name: "Square".into(), width: 1080, height: 1080, fps_num: 60, fps_den: 1 }
    }
    /// 4:5, the tallest feed post most platforms show uncropped.
    pub fn portrait() -> Self {
        Self { id: Id::new(), name: "Portrait".into(), width: 1080, height: 1350, fps_num: 60, fps_den: 1 }
    }
    /// Any size, rounded to even numbers (encoders need them) and kept within 16..=7680.
    pub fn custom(name: impl Into<String>, width: u32, height: u32) -> Self {
        let even = |v: u32| (v.clamp(16, 7680) / 2) * 2;
        Self { id: Id::new(), name: name.into(), width: even(width), height: even(height), fps_num: 60, fps_den: 1 }
    }
}

/// A media source, opened once and used by any number of layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub id: Id,
    pub name: String,
    /// libobs source type id (e.g. `screen_capture`, `av_capture_input_v2`, `image_source`).
    pub kind: String,
    /// Source settings as libobs `obs_data` JSON (opaque for plugin sources).
    pub settings: serde_json::Value,
    /// Input effects: applied wherever the input appears (docs/08-video-effects.md).
    #[serde(default)]
    pub effects: Vec<Effect>,
}

/// An ordered stack of layers. Index 0 is the bottom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: Id,
    pub name: String,
    pub layers: Vec<Layer>,
    /// Screen Studio's look, live: how the scene's screen captures zoom, and how the pointer
    /// and clicks are drawn on them (`None`: as captured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenLook>,
}

/// How a scene's screen captures zoom and show the pointer, live (docs/plans/screen-studio.md,
/// "Zoom" and "Cursor"). The engine draws it on every screen capture in the scene.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenLook {
    /// When it zooms: [`ScreenLook::ZOOM_OFF`], `ZOOM_CLICKS` (in to a click, out after
    /// `zoom_hold_s` without one), `ZOOM_ALWAYS` (always in, following the pointer) or
    /// `ZOOM_TOGGLE` (in, following the pointer, while a shortcut has turned it on).
    pub zoom: i64,
    /// How far (1.25–4×).
    pub zoom_scale: f32,
    /// How long it stays in after the last click (s).
    pub zoom_hold_s: f32,
    /// How it moves: 0 focused (quick) … 1 smooth (slow).
    pub zoom_motion: f32,
    /// Lightair draws the pointer (bigger, smoothed, styled); else the capture's own.
    pub cursor: bool,
    /// No pointer at all (neither drawn nor the capture's own; no clicks drawn).
    pub cursor_hidden: bool,
    /// 0 pointer, 1 dot, 2 ring.
    pub cursor_style: i64,
    pub cursor_size: f32,
    /// 0 none … 1 a slow glide.
    pub cursor_smoothing: f32,
    /// Hidden after this long still (s; 0 never).
    pub cursor_idle_s: f32,
    pub cursor_tilt: bool,
    /// 0xAABBGGRR (the dot, ring and click colour).
    pub cursor_color: u32,
    /// 0 none, 1 circle, 2 ripple.
    pub clicks: i64,
}

impl ScreenLook {
    pub const ZOOM_OFF: i64 = 0;
    pub const ZOOM_CLICKS: i64 = 1;
    pub const ZOOM_ALWAYS: i64 = 2;
    pub const ZOOM_TOGGLE: i64 = 3;
}

impl Default for ScreenLook {
    fn default() -> Self {
        ScreenLook {
            zoom: ScreenLook::ZOOM_CLICKS,
            zoom_scale: 2.0,
            zoom_hold_s: 2.0,
            zoom_motion: 0.5,
            cursor: true,
            cursor_hidden: false,
            cursor_style: 0,
            cursor_size: 1.5,
            cursor_smoothing: 0.5,
            cursor_idle_s: 3.0,
            cursor_tilt: true,
            cursor_color: 0xFFFF_9000,
            clicks: 2,
        }
    }
}

impl Scene {
    pub fn new(name: impl Into<String>) -> Self {
        Self { id: Id::new(), name: name.into(), layers: Vec::new(), screen: None }
    }
    pub fn layer(&self, id: Id) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }
    pub fn layer_index(&self, id: Id) -> Option<usize> {
        self.layers.iter().position(|l| l.id == id)
    }
}

/// An input placed in a scene, arranged per format; or, without an input, an *empty* holding
/// other layers (see `hierarchy`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub id: Id,
    pub name: String,
    /// The picture; `None` for an empty.
    pub input: Option<Id>,
    /// The layer this one sits in (its layout is in that layer's space).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Id>,
    /// The prefab this layer (and what's under it) was placed from: the asset's file name in
    /// the project's `assets` folder. Only an instance's root carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefab: Option<String>,
    /// Layout per format id. A format without an entry hides the layer in that format.
    pub layouts: BTreeMap<Id, Layout>,
    pub locked: bool,
    /// The same thing as the layers with this link in other scenes, though it shows another
    /// input (each screen recording scene's own backdrop): the Move transition glides between
    /// them. Layers showing the same input glide anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Id>,
}

/// Where and how a layer appears in one format.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// Top-left of the layer's box, in canvas pixels.
    pub x: f32,
    pub y: f32,
    /// Box size in canvas pixels; the input is fitted into it (`fit`).
    pub width: f32,
    pub height: f32,
    /// Degrees clockwise around the box centre.
    pub rotation: f32,
    pub crop: Crop,
    pub fit: Fit,
    pub visible: bool,
    pub opacity: f32,
    /// A cut-out of the (cropped) picture: rounded box, circle or pill.
    #[serde(default)]
    pub shape: Option<Shape>,
    /// Mirror the picture left↔right / top↔bottom.
    #[serde(default)]
    pub flip_h: bool,
    #[serde(default)]
    pub flip_v: bool,
    /// 3D tilt in degrees, with perspective: around the horizontal axis (positive: the bottom
    /// recedes) and around the vertical axis (positive: the left side recedes).
    #[serde(default)]
    pub tilt_x: f32,
    #[serde(default)]
    pub tilt_y: f32,
    /// Scale of this layer's space: its children grow with it (an empty's size, in effect).
    #[serde(default = "one")]
    pub scale_x: f32,
    #[serde(default = "one")]
    pub scale_y: f32,
}

fn one() -> f32 {
    1.0
}

impl Layout {
    pub fn boxed(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            rotation: 0.0,
            crop: Crop::default(),
            fit: Fit::Fit,
            visible: true,
            opacity: 1.0,
            shape: None,
            flip_h: false,
            flip_v: false,
            tilt_x: 0.0,
            tilt_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }

    /// Whether libobs draws this layer through a group with Lightair's filters (shape, flip, 3D).
    pub fn needs_filters(&self) -> bool {
        self.shape.is_some() || self.has_transform()
    }

    /// Flip or 3D tilt.
    pub fn has_transform(&self) -> bool {
        self.flip_h || self.flip_v || self.tilt_x.abs() > 0.01 || self.tilt_y.abs() > 0.01
    }
}

impl Layout {
    /// Where the whole, uncropped source would sit at this layer's scale: what crop mode shows
    /// (dimmed outside the crop). `source` is the source's pixel size. The result is stretched
    /// to its box, uncropped and unshaped.
    pub fn uncropped(&self, source: (f32, f32)) -> Layout {
        let c = self.crop;
        let vis_w = (source.0 - (c.left + c.right) as f32).max(1.0);
        let vis_h = (source.1 - (c.top + c.bottom) as f32).max(1.0);
        // Canvas pixels per source pixel.
        let (sx, sy) = (self.width / vis_w, self.height / vis_h);
        let (l, r) = (c.left as f32 * sx, c.right as f32 * sx);
        let (t, b) = (c.top as f32 * sy, c.bottom as f32 * sy);
        let (w, h) = (self.width + l + r, self.height + t + b);
        // The centre moves by half the difference, in the layer's own (rotated) frame.
        let (ox, oy) = ((r - l) / 2.0, (b - t) / 2.0);
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        let (wx, wy) = (ox * cos - oy * sin, ox * sin + oy * cos);
        let (cx, cy) = (self.x + self.width / 2.0 + wx, self.y + self.height / 2.0 + wy);
        Layout {
            x: cx - w / 2.0,
            y: cy - h / 2.0,
            width: w,
            height: h,
            crop: Crop::default(),
            fit: Fit::Stretch,
            shape: None,
            ..*self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShapeKind {
    Rounded,
    /// The largest circle that fits, centred.
    Circle,
    /// Fully rounded short ends.
    Pill,
}

/// How a layer is cut out (docs/07-scenes-layers-canvas.md, "Masks").
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    pub kind: ShapeKind,
    /// Rounded: corner radius as a fraction of half the short side (0..1).
    pub radius: f32,
    /// Soft edge, in source pixels.
    pub feather: f32,
    /// Border width in source pixels (0 = none) and its colour (0xAABBGGRR).
    pub border: f32,
    pub border_color: u32,
}

impl Shape {
    pub fn new(kind: ShapeKind) -> Self {
        Self { kind, radius: 0.25, feather: 1.0, border: 0.0, border_color: 0xFFFF_FFFF }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fit {
    /// Scale to fit inside the box, keep aspect ratio.
    Fit,
    /// Scale to cover the box, keep aspect ratio, crop the overflow.
    Fill,
    /// Stretch to the box.
    Stretch,
    /// Native size, centred in the box.
    Original,
}

/// A channel strip in the mixer: one audio input, its effects, level and routing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Id,
    pub name: String,
    pub input: Id,
    /// Fader level in dB (−∞ is represented by `f32::NEG_INFINITY`, serialized as null).
    #[serde(with = "db")]
    pub volume_db: f32,
    pub muted: bool,
    /// Sent to the cue (monitoring) mix.
    pub cue: bool,
    /// Insert effects, in order.
    pub effects: Vec<Effect>,
    /// Which of the input's channels (a multi-channel interface: one mic per channel).
    #[serde(default)]
    pub channels: TrackChannels,
    /// −1 (left) .. 1 (right).
    #[serde(default)]
    pub pan: f32,
    /// Input trim (dB), before the effects.
    #[serde(default)]
    pub trim_db: f32,
    /// Polarity.
    #[serde(default)]
    pub invert: bool,
    /// Sum to mono.
    #[serde(default)]
    pub mono: bool,
    /// Shift against the video (ms, ±2000).
    #[serde(default)]
    pub sync_ms: i32,
    /// Where the fader's output goes.
    #[serde(default)]
    pub route: Route,
    /// Sends to mixes (headphones, VOD, mix-minus).
    #[serde(default)]
    pub sends: Vec<Send>,
    /// Recorded to its own audio file in multitrack recordings.
    #[serde(default)]
    pub record: bool,
    /// The strip's colour in the mixer (0xRRGGBB).
    #[serde(default)]
    pub color: Option<u32>,
    /// The user named it (renamed it): it keeps its name when its source changes; otherwise it
    /// takes the new source's name.
    #[serde(default)]
    pub named: bool,
}

impl Track {
    pub fn new(name: impl Into<String>, input: Id) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            input,
            volume_db: 0.0,
            muted: false,
            cue: false,
            effects: Vec::new(),
            channels: TrackChannels::All,
            pan: 0.0,
            trim_db: 0.0,
            invert: false,
            mono: false,
            sync_ms: 0,
            route: Route::Program,
            sends: Vec::new(),
            record: false,
            color: None,
            named: false,
        }
    }
}

/// Which channels of its input a track takes (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackChannels {
    /// The input as it comes (mono inputs on both sides).
    #[default]
    All,
    Mono(u16),
    Stereo(u16, u16),
}

/// Where a strip's output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The master: what the stream and the recording's main track carry.
    #[default]
    Program,
    /// A bus (a group, or an FX return).
    Bus(Id),
    /// Only its sends (heard in a mix, never on air).
    None,
}

/// A send to a mix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Send {
    pub mix: Id,
    pub level_db: f32,
    /// Before the fader (a monitor mix that doesn't follow the main faders) or after.
    #[serde(default)]
    pub pre_fader: bool,
}

/// A bus: a strip fed by tracks routed to it ("Guests", "Music") or by sends (an FX return).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bus {
    pub id: Id,
    pub name: String,
    #[serde(with = "db")]
    pub volume_db: f32,
    pub muted: bool,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub route: Route,
    #[serde(default)]
    pub sends: Vec<Send>,
    #[serde(default)]
    pub record: bool,
    /// Gain before its effects.
    #[serde(default)]
    pub trim_db: f32,
    /// Heard in headphones (the cue).
    #[serde(default)]
    pub cue: bool,
    /// The strip's colour in the mixer (0xRRGGBB).
    #[serde(default)]
    pub color: Option<u32>,
}

impl Bus {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            volume_db: 0.0,
            muted: false,
            pan: 0.0,
            effects: Vec::new(),
            route: Route::Program,
            sends: Vec::new(),
            record: false,
            trim_db: 0.0,
            cue: false,
            color: None,
        }
    }
}

/// A mix: what the sends to it add up to, through its own effects and fader (headphones, a
/// Twitch VOD mix, a mix-minus for a remote guest, a music-free stream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioMix {
    pub id: Id,
    pub name: String,
    #[serde(with = "db")]
    pub volume_db: f32,
    pub muted: bool,
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// Also part of Program (an effects return), besides its own output.
    #[serde(default)]
    pub to_program: bool,
    /// Balance, −1 (left) .. 1 (right).
    #[serde(default)]
    pub pan: f32,
    /// Gain before its effects.
    #[serde(default)]
    pub trim_db: f32,
    /// Heard in headphones (to check what it sends out).
    #[serde(default)]
    pub cue: bool,
    /// The strip's colour in the mixer (0xRRGGBB).
    #[serde(default)]
    pub color: Option<u32>,
}

impl AudioMix {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            volume_db: 0.0,
            muted: false,
            effects: Vec::new(),
            to_program: false,
            pan: 0.0,
            trim_db: 0.0,
            cue: false,
            color: None,
        }
    }
}

/// The master strip (Program).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Master {
    #[serde(with = "db")]
    pub volume_db: f32,
    pub muted: bool,
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// Gain before its effects (drives the limiter).
    #[serde(default)]
    pub trim_db: f32,
    /// Heard in headphones: exactly what the stream carries.
    #[serde(default)]
    pub cue: bool,
    /// The strip's colour in the mixer (0xRRGGBB).
    #[serde(default)]
    pub color: Option<u32>,
}

impl Default for Master {
    fn default() -> Self {
        Self { volume_db: 0.0, muted: false, effects: Vec::new(), trim_db: 0.0, cue: false, color: None }
    }
}

/// The id that stands for the master strip (Program) where a strip id is expected.
pub const MASTER_ID: Id = Id(uuid::Uuid::nil());

/// A strip of the mixer, for commands that work on any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixerStrip {
    Track(Id),
    Bus(Id),
    Mix(Id),
    Master,
}

impl MixerStrip {
    /// The strip with this id in `project` (tracks, buses, mixes; [`MASTER_ID`]: the master).
    pub fn find(project: &Project, id: Id) -> Option<Self> {
        if id == MASTER_ID {
            Some(MixerStrip::Master)
        } else if project.track(id).is_some() {
            Some(MixerStrip::Track(id))
        } else if project.audio.bus(id).is_some() {
            Some(MixerStrip::Bus(id))
        } else if project.audio.mix(id).is_some() {
            Some(MixerStrip::Mix(id))
        } else {
            None
        }
    }

    pub fn id(self) -> Id {
        match self {
            MixerStrip::Track(i) | MixerStrip::Bus(i) | MixerStrip::Mix(i) => i,
            MixerStrip::Master => MASTER_ID,
        }
    }
}

/// The mixer beyond its tracks (docs/plans/audio-engine.md).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioSetup {
    #[serde(default)]
    pub buses: Vec<Bus>,
    #[serde(default)]
    pub mixes: Vec<AudioMix>,
    #[serde(default)]
    pub master: Master,
}

impl AudioSetup {
    pub fn bus(&self, id: Id) -> Option<&Bus> {
        self.buses.iter().find(|b| b.id == id)
    }
    pub fn mix(&self, id: Id) -> Option<&AudioMix> {
        self.mixes.iter().find(|m| m.id == id)
    }
}

/// An audio or video processor in an effect chain. `kind` is the libobs filter id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub id: Id,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub settings: serde_json::Value,
}

impl Effect {
    pub fn new(name: impl Into<String>, kind: impl Into<String>, settings: serde_json::Value) -> Self {
        Self { id: Id::new(), name: name.into(), kind: kind.into(), enabled: true, settings }
    }
}

mod db {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        if v.is_finite() { s.serialize_f32(*v) } else { s.serialize_none() }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        Ok(Option::<f32>::deserialize(d)?.unwrap_or(f32::NEG_INFINITY))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSettings {
    pub recording: RecordingSettings,
    pub destinations: Vec<Destination>,
    /// Streaming video bitrate for destinations on Automatic (kbps).
    pub stream_bitrate_kbps: u32,
    #[serde(default)]
    pub clips: ClipSettings,
    /// When a stream drops: try again this many times, this many seconds apart (0: don't).
    #[serde(default)]
    pub reconnect: Reconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconnect {
    pub retries: u32,
    pub delay_s: u32,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self { retries: 20, delay_s: 2 }
    }
}

/// Which video encoder. The hardware ones are Apple's (VideoToolbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoEncoder {
    /// H.264 on the Mac's media engine: works everywhere.
    #[default]
    H264,
    /// HEVC (H.265) on the media engine: smaller files; YouTube takes it live, most don't.
    Hevc,
    /// ProRes 422: editing quality, large files, MOV only, no bitrate.
    ProRes,
    /// x264 in software: uses the CPU.
    X264,
}

impl VideoEncoder {
    pub fn label(self) -> &'static str {
        match self {
            VideoEncoder::H264 => "H.264",
            VideoEncoder::Hevc => "HEVC",
            VideoEncoder::ProRes => "ProRes 422",
            VideoEncoder::X264 => "x264",
        }
    }
}

/// How a destination is encoded when not on Automatic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encoding {
    pub encoder: VideoEncoder,
    pub bitrate_kbps: u32,
    /// Seconds between keyframes (platforms want 2).
    pub keyframe_s: u32,
    /// Output height; the width follows the canvas. `None`: the canvas's own size.
    pub height: Option<u32>,
    pub audio_kbps: u32,
}

impl Default for Encoding {
    fn default() -> Self {
        Self { encoder: VideoEncoder::H264, bitrate_kbps: 6000, keyframe_s: 2, height: None, audio_kbps: 160 }
    }
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            recording: RecordingSettings::default(),
            destinations: Vec::new(),
            stream_bitrate_kbps: 6000,
            clips: ClipSettings::default(),
            reconnect: Reconnect::default(),
        }
    }
}

/// The replay buffer behind the Clip button.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClipSettings {
    pub enabled: bool,
    /// Seconds kept in the buffer (and saved per clip).
    pub seconds: u32,
    /// Format to clip; `None` = the primary format.
    pub format: Option<Id>,
}

impl Default for ClipSettings {
    fn default() -> Self {
        Self { enabled: false, seconds: 60, format: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingSettings {
    /// Folder for recordings; `None` means the default (`~/Movies/Lightair` on macOS, the Videos
    /// folder's `Lightair` on Windows).
    pub folder: Option<std::path::PathBuf>,
    /// `mp4` (hybrid, crash-safe), `mkv` or `mov`.
    pub container: String,
    pub bitrate_kbps: u32,
    /// Format to record; `None` = the primary format. Superseded by `formats`, kept so older
    /// projects record what they did.
    #[serde(default)]
    pub format: Option<Id>,
    /// The canvases recorded together, each to its own file. Empty: `format`, or else all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<Id>,
    #[serde(default)]
    pub encoder: VideoEncoder,
    /// Recorded height; the width follows the canvas. `None`: the canvas's own size.
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default = "default_record_audio")]
    pub audio_kbps: u32,
    /// Start a new file every this many minutes (`None`: one file).
    #[serde(default)]
    pub split_minutes: Option<u32>,
    /// Also record each track and bus marked Record (or every track, when none is) to its own
    /// audio file, sample-aligned (docs/plans/audio-engine.md, "Recording").
    #[serde(default)]
    pub multitrack: bool,
    /// Record audio only (no video): the multitrack files plus the program mix.
    #[serde(default)]
    pub audio_only: bool,
    #[serde(default)]
    pub stem_format: StemFormat,
    #[serde(default)]
    pub stem_point: StemPoint,
    /// Mixes recorded as extra audio tracks in the video file (after the program; up to 5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_mixes: Vec<Id>,
}

/// The sample format of multitrack audio files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StemFormat {
    /// 24-bit WAV: what editors and DAWs take everywhere.
    #[default]
    Pcm24,
    /// 32-bit float WAV: can't clip, for music.
    Float32,
}

/// Where in a strip a multitrack file listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StemPoint {
    /// Before its effects (the clean source, to process later).
    Raw,
    /// After its effects, before the fader (as heard, level untouched).
    #[default]
    PreFader,
    /// After the fader and pan (as mixed).
    PostFader,
}

fn default_record_audio() -> u32 {
    192
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            folder: None,
            container: "mp4".into(),
            bitrate_kbps: 12_000,
            format: None,
            formats: Vec::new(),
            encoder: VideoEncoder::H264,
            height: None,
            audio_kbps: 192,
            split_minutes: None,
            multitrack: false,
            audio_only: false,
            stem_format: StemFormat::Pcm24,
            stem_point: StemPoint::PreFader,
            file_mixes: Vec::new(),
        }
    }
}

impl RecordingSettings {
    /// The formats Record records, in the project's order (at least one). Until a choice is
    /// made, every canvas; an older project's single `format` stays its choice.
    pub fn targets(&self, project: &Project) -> Vec<Id> {
        let chosen: Vec<Id> = match (self.formats.is_empty(), self.format) {
            (false, _) => self.formats.clone(),
            (true, Some(f)) => vec![f],
            (true, None) => project.formats.iter().map(|f| f.id).collect(),
        };
        let out: Vec<Id> = project.formats.iter().map(|f| f.id).filter(|id| chosen.contains(id)).collect();
        if out.is_empty() { vec![project.primary_format().id] } else { out }
    }

    /// These settings with `format` recorded or not (never none at all).
    pub fn with_format(&self, project: &Project, format: Id, on: bool) -> RecordingSettings {
        let mut formats = self.targets(project);
        if on && !formats.contains(&format) {
            formats.push(format);
        } else if !on && formats.len() > 1 {
            formats.retain(|f| *f != format);
        }
        RecordingSettings { formats, format: None, ..self.clone() }
    }
}

/// A streaming destination. Custom RTMP(S) for now; platform sign-in arrives with M11.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Destination {
    pub id: Id,
    pub name: String,
    pub server: String,
    /// Included when going live.
    pub enabled: bool,
    /// Format to send (e.g. vertical for TikTok); `None` = the primary format.
    #[serde(default)]
    pub format: Option<Id>,
    /// Which platform it is ("youtube", "twitch", …, "custom"), for its badge and help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Its own encoding; `None`: Automatic (the shared bitrate, capped for the platform).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    Cut,
    Fade,
    /// Matching layers glide from their old to their new layout; the rest fade.
    Move,
    /// The new scene pushes the old one out in `direction`.
    Slide,
    /// The new scene wipes across the old one in `direction`.
    Wipe,
    /// Fade through black.
    Dip,
    /// Plays the project's stinger video over the switch.
    Stinger,
}

impl TransitionKind {
    pub const ALL: [TransitionKind; 7] =
        [Self::Cut, Self::Fade, Self::Move, Self::Slide, Self::Wipe, Self::Dip, Self::Stinger];

    pub fn label(self) -> &'static str {
        match self {
            Self::Cut => "Cut",
            Self::Fade => "Fade",
            Self::Move => "Move",
            Self::Slide => "Slide",
            Self::Wipe => "Wipe",
            Self::Dip => "Dip",
            Self::Stinger => "Stinger",
        }
    }

    /// Whether `direction` applies.
    pub fn directional(self) -> bool {
        matches!(self, Self::Slide | Self::Wipe)
    }
}

/// Which way a directional transition travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn next(self) -> Self {
        match self {
            Self::Left => Self::Up,
            Self::Up => Self::Right,
            Self::Right => Self::Down,
            Self::Down => Self::Left,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransitionSettings {
    pub kind: TransitionKind,
    pub duration_ms: u32,
    #[serde(default)]
    pub direction: Direction,
}

impl Default for TransitionSettings {
    fn default() -> Self {
        Self { kind: TransitionKind::Move, duration_ms: 500, direction: Direction::Left }
    }
}
