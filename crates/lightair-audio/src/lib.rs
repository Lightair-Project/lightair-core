//! Lightair's real-time audio engine (docs/plans/audio-engine.md).
//!
//! The mixer is a graph compiled from a [`GraphSpec`]: **tracks** (an input's channel or pair,
//! trim, inserts, fader, pan, sends) feed **buses** (groups, FX returns) and the **program**
//! (the master); **mixes** take per-source sends (headphones, VOD, mix-minus). Everything runs
//! at one sample rate in 32-bit float, in blocks of up to [`MAX_BLOCK`] frames.
//!
//! Real-time rules: [`Graph::process`] never allocates, locks or blocks. Specs are compiled into
//! a new [`Graph`] off the audio thread and swapped in through [`Engine`]; effect state and
//! levels carry across the swap ([`Graph::adopt`]), so edits never click or reset an effect.

pub mod dsp;
mod engine;
mod graph;
pub mod meter;
mod smooth;
mod spec;

pub use engine::{Engine, PROGRAM, Renderer};
pub use graph::{Graph, InputBlock, Inputs, Tap};
pub use spec::*;

/// The most frames processed in one call.
pub const MAX_BLOCK: usize = 1024;

/// Decibels to a linear gain (−∞ and below −144 dB: silence).
pub fn db_to_gain(db: f32) -> f32 {
    if db <= -144.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// A linear gain to decibels (silence: −∞).
pub fn gain_to_db(g: f32) -> f32 {
    if g <= 0.0 { f32::NEG_INFINITY } else { 20.0 * g.log10() }
}
