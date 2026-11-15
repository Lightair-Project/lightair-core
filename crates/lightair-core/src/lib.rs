//! Lightair's document model. Pure: no I/O, no libobs.

pub mod catalog;
pub mod clipboard;
pub mod command;
pub mod diff;
pub mod document;
pub mod hierarchy;
pub mod kinds;
pub mod model;
pub mod screen_layouts;
pub mod templates;

pub use clipboard::{Clip, SceneAsset};
pub use command::{Command, CommandError};
pub use diff::{LayerPatch, Patch, diff, diff_layers};
pub use document::{Change, Document};
pub use hierarchy::Frame;
pub use model::{
    AudioMix, AudioSetup, Bus, ClipSettings, Crop, Destination, Direction, Effect, Encoding, Fit, Canvas, Id, Source,
    Layer, Layout, MASTER_ID, Master, MixerStrip, Route, Send, StemFormat, StemPoint, TrackChannels,
    OutputSettings, Workspace, Reconnect, RecordingSettings, Scene, ScreenLook, Shape, ShapeKind, Stinger, Track, TransitionKind,
    TransitionSettings, VideoEncoder,
};

/// `name` made safe for a file name (a canvas, track or workspace name in a file's name): the
/// characters macOS and Windows don't allow become "-".
pub fn file_safe(name: &str) -> String {
    name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect()
}
