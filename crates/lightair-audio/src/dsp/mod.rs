//! Built-in effects. Each is a [`Processor`]: made off the audio thread from its [`Params`],
//! updated in place with new params (no allocation), processing planar stereo blocks.

mod gain;

pub use gain::GainParams;

/// An effect's settings: the kind and its values.
#[derive(Debug, Clone, PartialEq)]
pub enum Params {
    Gain(GainParams),
}

impl Params {
    /// The kind, as the document names it ("audio.gain", …).
    pub fn kind(&self) -> &'static str {
        match self {
            Params::Gain(_) => "audio.gain",
        }
    }

    /// A processor for these params at `sample_rate`.
    pub fn make(&self, sample_rate: f32) -> Box<dyn Processor> {
        match self {
            Params::Gain(p) => Box::new(gain::Gain::new(*p, sample_rate)),
        }
    }
}

/// What a processor gets besides its audio.
pub struct Ctx<'a> {
    pub sample_rate: f32,
    /// A sidechain key (mono), for effects that react to another strip (ducking).
    pub key: Option<&'a [f32]>,
}

pub trait Processor: Send {
    fn kind(&self) -> &'static str;
    /// New settings (audio thread: must not allocate).
    fn set(&mut self, params: &Params);
    /// Processes a block in place.
    fn process(&mut self, l: &mut [f32], r: &mut [f32], ctx: &Ctx);
    /// Forgets its history (after a discontinuity).
    fn reset(&mut self) {}
    /// Current gain reduction in dB (≤ 0), for the strip's meter.
    fn reduction_db(&self) -> f32 {
        0.0
    }
}
