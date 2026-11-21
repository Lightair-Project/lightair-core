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
pub use diff::{ObjectPatch, Patch, diff, diff_objects};
pub use document::{Change, Document};
pub use hierarchy::Frame;
pub use model::{
    AudioMix, AudioSetup, Bus, Canvas, ClipSettings, Crop, Destination, Direction, Effect, Encoding, Fit, Id, Layout,
    MASTER_ID, Master, MixerStrip, MultitrackFormat, MultitrackPoint, Object, OutputSettings, Reconnect,
    RecordingSettings, Route, Scene, ScreenZoomPointer, Send, Shape, ShapeKind, Source, Stinger, Track, TrackChannels,
    TransitionKind, TransitionSettings, VideoEncoder, VirtualCameraSettings, Workspace,
};

/// `name` made safe for a file name (a canvas, track or workspace name in a file's name): the
/// characters macOS and Windows don't allow become "-".
pub fn file_safe(name: &str) -> String {
    name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect()
}
