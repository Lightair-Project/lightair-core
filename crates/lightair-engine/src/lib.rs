//! The live engine: Studio's projects, the program
//! clock locked to audio, frame rings, the scene renderer on the phone's GPU, the audio mix,
//! the encoders' queues and recording. No UniFFI here: `lightair-live-core`'s `studio` module
//! wraps it for Swift and Kotlin and exports the hot paths (`ffi`).

/// Moved to `lightair-overlay` (shared with the broadcast extension); the paths stay.
pub use lightair_overlay::{alerts, widgets};
pub mod audio;
pub mod background;
pub mod chat;
pub mod clips;
pub mod clock;
pub mod device;
pub mod effects;
pub mod encode;
pub mod feeds;
pub mod ffi;
pub mod gesture;
pub mod governor;
pub mod gpu;
pub mod host;
pub mod hot;
pub mod ingest;
pub mod keying;
pub mod looks;
pub mod moderation;
pub mod outputs;
pub mod pictures;
pub mod platforms;
pub mod project;
pub mod record;
pub mod remote;
pub mod render;
pub mod session;
pub mod templates;
#[cfg(all(test, target_os = "macos"))]
mod tests;
pub mod thumb;
pub mod web;
pub mod whip;

pub use device::DeviceInfo;
pub use host::*;
pub use project::{ProjectError, Projects, Summary};
pub use session::{LayoutPatch, NewLayer, Session, ShapeKind, ShapeSpec, State, TextSpec, TimerMode, TimerSpec};
