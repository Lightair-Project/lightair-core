//! Lightair's PNGtubers (lightair docs/plans/pngtuber.md): an avatar made of pictures that talks
//! with the microphone, blinks on its own, bounces and breathes, and changes expression on a
//! shortcut.
//!
//! - [`model`]: the avatar as saved: layers with roles, anchors, expressions, mic, motion, blink;
//! - [`file`]: the `.lairava` file (docs/pngtuber-format.md): a ZIP of `avatar.json` and PNGs;
//! - [`runtime`]: the avatar moving, frame by frame, from the mic's level.
//!
//! Written for Lightair from nothing: no other app's code or files. Drawing is the app's (the
//! desktop's libobs source, the phones' own renderers).

pub mod file;
pub mod model;
pub mod runtime;

pub use file::{Loaded, Picture};
pub use model::{Anchor, Avatar, Blink, Expression, Layer, Mic, Motion, Role};
pub use runtime::{Inputs, LayerPose, Pose, Puppet};
