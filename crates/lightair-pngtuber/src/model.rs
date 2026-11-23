//! A PNGtuber as it's saved (`avatar.json` in a `.lairava`, docs/pngtuber-format.md): pictures
//! placed on a design canvas, each with a role, and what moves them.
//!
//! Coordinates are the design canvas's pixels, x right, y down, (0, 0) at the top left.

use serde::{Deserialize, Serialize};

/// The format's name in `avatar.json`.
pub const FORMAT: &str = "lightair-pngtuber";
/// The version this crate writes; it reads this one and older.
pub const VERSION: u32 = 1;

/// What a layer is for: how the runtime shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Always shown (body, head, hair, a background part).
    #[default]
    Part,
    /// Shown while the eyes are open; hidden while blinking.
    EyesOpen,
    /// Shown only while blinking.
    EyesClosed,
    /// One of the mouth's frames ([`Layer::frame`]: 0 shut, then more open).
    Mouth,
    /// Shown only while talking.
    Talking,
    /// Shown only while quiet.
    Quiet,
    /// Shown only when an expression or costume shows it.
    Extra,
}

impl Role {
    pub const ALL: [Role; 7] =
        [Role::Part, Role::EyesOpen, Role::EyesClosed, Role::Mouth, Role::Talking, Role::Quiet, Role::Extra];

    /// As people read it.
    pub fn label(self) -> &'static str {
        match self {
            Role::Part => "Always",
            Role::EyesOpen => "Eyes open",
            Role::EyesClosed => "Eyes shut",
            Role::Mouth => "Mouth",
            Role::Talking => "While talking",
            Role::Quiet => "While quiet",
            Role::Extra => "Only in an expression",
        }
    }

    /// A role suggested by a picture's name ("mouth_open" is a mouth frame, "eyes_closed" eyes
    /// shut, "blush" an extra…).
    pub fn suggested(name: &str) -> (Role, u8) {
        let n = name.to_lowercase();
        let has = |w: &[&str]| w.iter().any(|w| n.contains(w));
        if has(&["mouth", "mund", "kuchi"]) {
            let frame = if has(&["closed", "shut", "close"]) {
                0
            } else if has(&["wide", "yell", "shout", "big"]) {
                3
            } else if has(&["half", "small", "mid"]) {
                1
            } else if has(&["open"]) {
                2
            } else {
                n.chars().filter_map(|c| c.to_digit(10)).next_back().map_or(0, |d| d.min(4) as u8)
            };
            (Role::Mouth, frame)
        } else if has(&["eye"]) && has(&["closed", "shut", "close", "blink"]) {
            (Role::EyesClosed, 0)
        } else if has(&["eye"]) {
            (Role::EyesOpen, 0)
        } else if has(&["talk", "speak"]) {
            (Role::Talking, 0)
        } else if has(&["idle", "quiet", "silent"]) {
            (Role::Quiet, 0)
        } else if has(&["blush", "angry", "sad", "happy", "hat", "glasses", "tear", "sweat"]) {
            (Role::Extra, 0)
        } else {
            (Role::Part, 0)
        }
    }
}

/// A picture on the design canvas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    /// Unique in the avatar; expressions and anchors name layers by it.
    pub id: String,
    pub name: String,
    /// Its PNG in the file ("layers/<id>.png").
    pub image: String,
    /// Its box on the design canvas: top left and size.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The point it turns and grows about (design canvas pixels).
    pub pivot: [f32; 2],
    #[serde(default)]
    pub role: Role,
    /// A mouth's frame: 0 shut, then each more open.
    #[serde(default)]
    pub frame: u8,
    /// How much it moves with the avatar's motion (0 still, 1 fully): a background part can stay put.
    #[serde(default = "one")]
    pub motion: f32,
    #[serde(default = "one")]
    pub opacity: f32,
}

fn one() -> f32 {
    1.0
}

/// A named point that follows a layer (or the whole avatar): what things are pinned to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub name: String,
    pub x: f32,
    pub y: f32,
    /// The layer it moves with (`None`: the avatar as a whole).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

/// An expression or a costume: layers shown and hidden while it's on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Expression {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub show: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<String>,
    /// A costume rather than a mood (listed apart; the same otherwise).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub costume: bool,
}

/// How the microphone drives it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mic {
    /// The mouth opens above this level (dBFS) …
    pub open_db: f32,
    /// … and closes below this one (lower, so it doesn't chatter).
    pub close_db: f32,
    /// Below this, it's silence (room noise).
    pub gate_db: f32,
    /// How fast the mouth follows a louder and a quieter voice (milliseconds).
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Default for Mic {
    fn default() -> Self {
        Mic { open_db: -34.0, close_db: -42.0, gate_db: -58.0, attack_ms: 30.0, release_ms: 120.0 }
    }
}

/// How much it moves (each 0 off, 1 as designed, up to 2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Motion {
    /// A bounce while talking, with the voice.
    pub bounce: f32,
    /// A squash and stretch while talking.
    pub squash: f32,
    /// A slow breath while quiet.
    pub breathing: f32,
    /// A little hop when it starts talking or changes expression.
    pub hop: f32,
}

impl Default for Motion {
    fn default() -> Self {
        Motion { bounce: 1.0, squash: 1.0, breathing: 1.0, hop: 1.0 }
    }
}

/// Blinking on its own.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Blink {
    /// About this often (seconds; it varies), 0: never.
    pub every_s: f32,
    /// How long the eyes stay shut (milliseconds).
    pub length_ms: f32,
    /// How often a blink comes twice (0..1).
    pub double: f32,
}

impl Default for Blink {
    fn default() -> Self {
        Blink { every_s: 4.0, length_ms: 140.0, double: 0.15 }
    }
}

/// A PNGtuber.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Avatar {
    pub format: String,
    pub version: u32,
    pub name: String,
    /// The design canvas, pixels.
    pub width: u32,
    pub height: u32,
    /// Back to front.
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
    #[serde(default)]
    pub expressions: Vec<Expression>,
    #[serde(default)]
    pub mic: Mic,
    #[serde(default)]
    pub motion: Motion,
    #[serde(default)]
    pub blink: Blink,
}

impl Avatar {
    pub fn new(name: &str, width: u32, height: u32) -> Avatar {
        Avatar {
            format: FORMAT.into(),
            version: VERSION,
            name: name.into(),
            width: width.max(1),
            height: height.max(1),
            layers: Vec::new(),
            anchors: Vec::new(),
            expressions: Vec::new(),
            mic: Mic::default(),
            motion: Motion::default(),
            blink: Blink::default(),
        }
    }

    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// A layer id not taken yet, from a name ("Mouth open" → "mouth-open", "mouth-open-2").
    pub fn free_id(&self, name: &str) -> String {
        let base: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let base = if base.is_empty() { "layer".to_string() } else { base };
        if self.layer(&base).is_none() {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|id| self.layer(id).is_none()).unwrap_or(base)
    }

    /// How many mouth frames it has (the highest frame + 1; 0 without a mouth).
    pub fn mouth_frames(&self) -> usize {
        self.layers.iter().filter(|l| l.role == Role::Mouth).map(|l| l.frame as usize + 1).max().unwrap_or(0)
    }
}
