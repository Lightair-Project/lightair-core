//! Built-in effects. Each is a [`Processor`]: made off the audio thread from its [`Params`],
//! updated in place with new params (no allocation), processing planar stereo blocks.

pub mod biquad;
mod dynamics;
pub mod eq;
mod gain;
mod noise;
mod pitch;
mod plugin;
mod space;
mod voice;

pub use dynamics::{CompressorParams, DeEsserParams, DuckerParams, GateParams, LimiterParams, compression_db};
pub use eq::{Band, BandKind, EqParams};
pub use gain::GainParams;
pub use noise::NoiseParams;
pub use pitch::PitchParams;
pub use plugin::{Plugin, PluginParams};
pub use space::{ChannelParams, DelayParams, ReverbParams};
pub use voice::VoiceParams;

/// An effect's settings: the kind and its values.
#[derive(Debug, Clone, PartialEq)]
pub enum Params {
    Gain(GainParams),
    Eq(EqParams),
    Compressor(CompressorParams),
    Gate(GateParams),
    Limiter(LimiterParams),
    DeEsser(DeEsserParams),
    Ducker(DuckerParams),
    Noise(NoiseParams),
    Reverb(ReverbParams),
    Delay(DelayParams),
    Channel(ChannelParams),
    Voice(VoiceParams),
    Pitch(PitchParams),
    /// A hosted plugin (AU, VST3).
    Plugin(PluginParams),
}

impl Params {
    /// The kind, as the document names it ("audio.gain", …).
    pub fn kind(&self) -> &'static str {
        match self {
            Params::Gain(_) => "audio.gain",
            Params::Eq(_) => "audio.eq",
            Params::Compressor(_) => "audio.compressor",
            Params::Gate(_) => "audio.gate",
            Params::Limiter(_) => "audio.limiter",
            Params::DeEsser(_) => "audio.deesser",
            Params::Ducker(_) => "audio.ducker",
            Params::Noise(_) => "audio.noise",
            Params::Reverb(_) => "audio.reverb",
            Params::Delay(_) => "audio.delay",
            Params::Channel(_) => "audio.channel",
            Params::Voice(_) => "audio.voice",
            Params::Pitch(_) => "audio.pitch",
            Params::Plugin(_) => "audio.plugin",
        }
    }

    /// A processor for these params at `sample_rate` (allocates: not on the audio thread).
    pub fn make(&self, sample_rate: f32) -> Box<dyn Processor> {
        match self {
            Params::Plugin(p) => Box::new(plugin::Hosted::new(p)),
            &Params::Gain(p) => Box::new(gain::Gain::new(p, sample_rate)),
            &Params::Eq(p) => Box::new(eq::Eq::new(p, sample_rate)),
            &Params::Compressor(p) => Box::new(dynamics::Compressor::new(p, sample_rate)),
            &Params::Gate(p) => Box::new(dynamics::Gate::new(p, sample_rate)),
            &Params::Limiter(p) => Box::new(dynamics::Limiter::new(p, sample_rate)),
            &Params::DeEsser(p) => Box::new(dynamics::DeEsser::new(p, sample_rate)),
            &Params::Ducker(p) => Box::new(dynamics::Ducker::new(p, sample_rate)),
            &Params::Noise(p) => Box::new(noise::Noise::new(p, sample_rate)),
            &Params::Reverb(p) => Box::new(space::Reverb::new(p, sample_rate)),
            &Params::Delay(p) => Box::new(space::Delay::new(p, sample_rate)),
            &Params::Channel(p) => Box::new(space::ChannelTools::new(p, sample_rate)),
            &Params::Voice(p) => Box::new(voice::Voice::new(p, sample_rate)),
            &Params::Pitch(p) => Box::new(pitch::Pitch::new(p, sample_rate)),
        }
    }

    /// The strip whose signal this effect listens to (a ducker's source).
    pub fn sidechain(&self) -> Option<crate::NodeId> {
        match self {
            Params::Ducker(p) => Some(p.source),
            _ => None,
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
    /// Frames of delay it adds (look-ahead, framing), for aligning recordings.
    fn latency(&self) -> usize {
        0
    }
    /// What it is beyond its kind: a hosted plugin's instance. A graph swap keeps a processor's
    /// state only when kind and identity match.
    fn identity(&self) -> usize {
        0
    }
}
