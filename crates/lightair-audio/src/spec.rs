//! What the mixer should be: plain data, compiled into a [`crate::Graph`].

use crate::dsp::Params;

/// Identifies tracks, buses, mixes, inputs and effects (the document's ids).
pub type NodeId = u128;

/// Which of an input's channels a track takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    /// One channel (0-based), heard on both sides, placed by the pan.
    Mono(u16),
    /// Two channels as left and right.
    Stereo(u16, u16),
}

/// Where a strip's post-fader signal goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Program,
    Bus(NodeId),
    /// Only its sends (e.g. a track heard in headphones but never on air).
    Nowhere,
}

/// A send from a strip to a mix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Send {
    pub mix: NodeId,
    pub level_db: f32,
    /// Taken before the fader (a monitor mix unaffected by the main mix) or after.
    pub pre_fader: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    /// Takes `channels` of input `input`.
    Track { input: NodeId, channels: Channels },
    /// Sums the strips routed to it.
    Bus,
}

/// An insert effect.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectSpec {
    pub id: NodeId,
    pub enabled: bool,
    pub params: Params,
}

/// A channel strip: a track or a bus.
#[derive(Debug, Clone, PartialEq)]
pub struct Strip {
    pub id: NodeId,
    pub name: String,
    pub role: Role,
    /// Input trim, before the inserts.
    pub trim_db: f32,
    /// Polarity.
    pub invert: bool,
    /// Sum to mono (before the pan).
    pub mono: bool,
    pub inserts: Vec<EffectSpec>,
    /// The fader.
    pub gain_db: f32,
    /// −1 (left) .. 1 (right); unity in both sides at the centre.
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub dest: Dest,
    pub sends: Vec<Send>,
}

impl Strip {
    pub fn track(id: NodeId, input: NodeId, channels: Channels) -> Self {
        Self::new(id, Role::Track { input, channels })
    }

    pub fn bus(id: NodeId) -> Self {
        Self::new(id, Role::Bus)
    }

    fn new(id: NodeId, role: Role) -> Self {
        Self {
            id,
            name: String::new(),
            role,
            trim_db: 0.0,
            invert: false,
            mono: false,
            inserts: Vec::new(),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            dest: Dest::Program,
            sends: Vec::new(),
        }
    }
}

/// A mix: the sum of the sends to it, through its own inserts and fader. Its own output, and
/// (when `to_program`) also part of Program, like an effects return.
#[derive(Debug, Clone, PartialEq)]
pub struct Mix {
    pub id: NodeId,
    pub name: String,
    pub inserts: Vec<EffectSpec>,
    pub gain_db: f32,
    pub mute: bool,
    pub to_program: bool,
    /// Balance, −1 (left) .. 1 (right): unity in both sides at the centre.
    pub pan: f32,
    /// Gain before its inserts.
    pub trim_db: f32,
    /// Also heard in the cue mix ([`GraphSpec::cue`]).
    pub to_cue: bool,
}

impl Mix {
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            name: String::new(),
            inserts: Vec::new(),
            gain_db: 0.0,
            mute: false,
            to_program: false,
            pan: 0.0,
            trim_db: 0.0,
            to_cue: false,
        }
    }
}

/// The master: everything routed to Program, through its inserts and fader.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Master {
    pub inserts: Vec<EffectSpec>,
    pub gain_db: f32,
    pub mute: bool,
    /// Gain before its inserts (drive the limiter).
    pub trim_db: f32,
    /// Also heard in the cue mix: what the stream carries.
    pub to_cue: bool,
}

/// The whole mixer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphSpec {
    pub strips: Vec<Strip>,
    pub mixes: Vec<Mix>,
    pub program: Master,
    /// The mix that's the cue (headphones): finished last, after Program, so mixes and Program
    /// can be heard in it.
    pub cue: Option<NodeId>,
}

/// Problems the compiler corrected (the graph still runs).
#[derive(Debug, Clone, PartialEq)]
pub enum Warning {
    /// A strip routed to a bus that doesn't exist, or in a loop: sent to Program instead.
    BadRoute { strip: NodeId },
    /// A send to a mix that doesn't exist: dropped.
    BadSend { strip: NodeId, mix: NodeId },
}
