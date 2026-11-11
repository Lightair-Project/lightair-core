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

    /// Layers anywhere in the project that use `input`.
    pub fn layers_using(&self, input: Id) -> impl Iterator<Item = (&Scene, &Layer)> {
        self.scenes.iter().flat_map(move |s| s.layers.iter().filter(move |l| l.input == input).map(move |l| (&**s, l)))
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
}

/// An ordered stack of layers. Index 0 is the bottom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: Id,
    pub name: String,
    pub layers: Vec<Layer>,
}

impl Scene {
    pub fn new(name: impl Into<String>) -> Self {
        Self { id: Id::new(), name: name.into(), layers: Vec::new() }
    }
    pub fn layer(&self, id: Id) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }
    pub fn layer_index(&self, id: Id) -> Option<usize> {
        self.layers.iter().position(|l| l.id == id)
    }
}

/// An input placed in a scene, arranged per format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub id: Id,
    pub name: String,
    pub input: Id,
    /// Layout per format id. A format without an entry hides the layer in that format.
    pub layouts: BTreeMap<Id, Layout>,
    pub locked: bool,
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
}

impl Layout {
    pub fn boxed(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height, rotation: 0.0, crop: Crop::default(), fit: Fit::Fit, visible: true, opacity: 1.0 }
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
