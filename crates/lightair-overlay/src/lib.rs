//! Studio's overlays drawn on the CPU, without the scene renderer: the native widgets
//! and the alert rules. The engine draws them into its scene; the iOS 17–26 broadcast
//! extension draws them over the screen while the app is away.

pub mod alerts;
pub mod text;
pub mod widgets;
