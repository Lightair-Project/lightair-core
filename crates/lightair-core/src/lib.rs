//! Lightair's document model. Pure: no I/O, no libobs.

pub mod command;
pub mod diff;
pub mod document;
pub mod model;

pub use command::{Command, CommandError};
pub use diff::{LayerPatch, Patch, diff, diff_layers};
pub use document::{Change, Document};
pub use model::{
    Crop, Destination, Effect, Fit, Format, Id, Input, Layer, Layout, OutputSettings, Project, RecordingSettings,
    Scene, Track,
};
