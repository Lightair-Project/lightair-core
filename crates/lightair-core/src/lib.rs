//! Lightair's document model. Pure: no I/O, no libobs.

pub mod clipboard;
pub mod command;
pub mod diff;
pub mod document;
pub mod hierarchy;
pub mod model;

pub use clipboard::Clip;
pub use command::{Command, CommandError};
pub use diff::{LayerPatch, Patch, diff, diff_layers};
pub use document::{Change, Document};
pub use hierarchy::Frame;
pub use model::{
    ClipSettings, Crop, Destination, Direction, Effect, Fit, Format, Id, Input, Layer, Layout, OutputSettings, Project,
    RecordingSettings, Scene, Shape, ShapeKind, Stinger, Track, TransitionKind, TransitionSettings,
};
