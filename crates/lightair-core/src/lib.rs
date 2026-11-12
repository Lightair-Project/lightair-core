//! Lightair's document model. Pure: no I/O, no libobs.

pub mod catalog;
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
    AudioMix, AudioSetup, Bus, ClipSettings, Crop, Destination, Direction, Effect, Encoding, Fit, Format, Id, Input,
    Layer, Layout, Master, Route, Send, TrackChannels,
    OutputSettings, Project, Reconnect, RecordingSettings, Scene, Shape, ShapeKind, Stinger, Track, TransitionKind,
    TransitionSettings, VideoEncoder,
};
