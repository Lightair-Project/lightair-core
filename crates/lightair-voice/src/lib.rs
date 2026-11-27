//! Lightair's voices and hearing.
//!
//! **Voices** turn text into speech, streamed as it's made: 16-bit mono PCM at the vendor's rate,
//! with mouth shapes when the vendor gives timings. One [`Voice`] per [`Profile`]; each
//! [`Utterance`] takes text pieces in order (a sentence at a time), so the first sound comes before
//! the whole text is known. Vendors: the system's own voice (free, offline, through
//! `lightair-platform`), Cartesia, ElevenLabs and Gemini.
//!
//! **Hearing** ([`hear`]) turns a track's sound into words with a cloud engine (ElevenLabs Scribe,
//! OpenRouter's transcription models). On-device hearing stays with the platform's own helper.
//!
//! **The filter** ([`filter`]) keeps slurs, personal data and links out of what's said aloud.

mod cartesia;
mod elevenlabs;
pub mod filter;
mod gemini;
pub mod hear;
mod net;
mod system;
mod worker;
mod ws;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use hear::{HearConfig, HearEvent, Hearer};

/// Who makes the sound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    /// The operating system's voice: free, offline, no mouth timings.
    #[default]
    System,
    Cartesia,
    #[serde(rename = "elevenlabs")]
    ElevenLabs,
    Gemini,
}

impl Vendor {
    pub const ALL: [Vendor; 4] = [Vendor::System, Vendor::Cartesia, Vendor::ElevenLabs, Vendor::Gemini];

    pub fn id(self) -> &'static str {
        match self {
            Vendor::System => "system",
            Vendor::Cartesia => "cartesia",
            Vendor::ElevenLabs => "elevenlabs",
            Vendor::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Option<Vendor> {
        Vendor::ALL.into_iter().find(|v| v.id() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Vendor::System => "System voice",
            Vendor::Cartesia => "Cartesia",
            Vendor::ElevenLabs => "ElevenLabs",
            Vendor::Gemini => "Gemini",
        }
    }

    /// The key it needs, by its id in Settings › Connections (`None`: none).
    pub fn key_service(self) -> Option<&'static str> {
        match self {
            Vendor::System => None,
            Vendor::Cartesia => Some("cartesia"),
            Vendor::ElevenLabs => Some("elevenlabs"),
            Vendor::Gemini => Some("google"),
        }
    }
}

/// A voice set up once and picked by whatever speaks: who makes it, which of their voices, how
/// fast, in what language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub vendor: Vendor,
    /// The vendor's voice id (empty: its default).
    pub voice: String,
    /// The voice's own name, for showing.
    pub voice_name: String,
    /// 1.0 is normal speed.
    pub speed: f32,
    /// A language code ("en", "ja") or name ("English"); empty: the voice's own.
    pub language: String,
}

impl Default for Profile {
    fn default() -> Profile {
        Profile {
            id: "system".into(),
            name: "System voice".into(),
            vendor: Vendor::System,
            voice: String::new(),
            voice_name: String::new(),
            speed: 1.0,
            language: String::new(),
        }
    }
}

/// A mouth shape a vendor's timings gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    A,
    I,
    U,
    E,
    O,
    /// Lips closed (m, b, p).
    M,
    Rest,
}

impl Shape {
    pub fn as_str(self) -> &'static str {
        match self {
            Shape::A => "A",
            Shape::I => "I",
            Shape::U => "U",
            Shape::E => "E",
            Shape::O => "O",
            Shape::M => "M",
            Shape::Rest => "rest",
        }
    }

    /// The shape for a phoneme, IPA or ARPAbet.
    pub fn of_phoneme(p: &str) -> Option<Shape> {
        if ["m", "b", "p", "ɱ", "M", "B", "P"].contains(&p) {
            return Some(Shape::M);
        }
        let table: [(Shape, &[&str]); 5] = [
            (Shape::I, &["i", "ɪ", "j", "IY", "IH", "Y"]),
            (Shape::E, &["e", "ɛ", "EH", "EY"]),
            (Shape::A, &["a", "ɑ", "æ", "ʌ", "ə", "ɐ", "AA", "AE", "AH", "AW", "AY", "ER", "ɚ", "ɝ"]),
            (Shape::O, &["o", "ɔ", "ɒ", "OW", "OY", "AO"]),
            (Shape::U, &["u", "ʊ", "w", "UW", "UH", "W"]),
        ];
        table.into_iter().find(|(_, starts)| starts.iter().any(|s| p.starts_with(s))).map(|(s, _)| s)
    }

    /// The shape for a letter (for vendors that time characters).
    pub fn of_letter(c: char) -> Option<Shape> {
        Some(match c.to_ascii_lowercase() {
            'm' | 'b' | 'p' => Shape::M,
            'a' => Shape::A,
            'e' => Shape::E,
            'i' | 'y' => Shape::I,
            'o' => Shape::O,
            'u' | 'w' => Shape::U,
            ' ' | '.' | ',' | '!' | '?' | ';' | ':' => Shape::Rest,
            _ => return None,
        })
    }
}

/// A piece of speech: samples at `rate`, with mouth shapes at ms offsets from its start.
#[derive(Clone, Debug, Default)]
pub struct Chunk {
    pub pcm: Vec<i16>,
    pub rate: u32,
    pub marks: Vec<(f32, Shape)>,
}

/// What an utterance gives.
#[derive(Clone, Debug)]
pub enum Event {
    Audio(Chunk),
    /// The last piece came (after `end`).
    Done,
    /// It can't go on, and why (a sentence for the user).
    Failed(String),
}

/// Where an utterance's events go (called on the vendor's own thread).
pub type Sink = Box<dyn FnMut(Event) + Send>;

/// One thing said: text pieces in order, then `end`.
pub trait Utterance: Send {
    fn say(&mut self, piece: &str);
    fn end(&mut self);
    /// Stops now; no more events.
    fn cancel(&mut self);
}

trait Engine: Send + Sync {
    fn utterance(&self, sink: Sink) -> Box<dyn Utterance>;
    fn warm(&self) {}
}

/// A voice made from a profile, ready to speak.
#[derive(Clone)]
pub struct Voice {
    engine: Arc<dyn Engine>,
    profile: Profile,
}

impl Voice {
    /// The voice for `profile`; `key` is the vendor's (see [`Vendor::key_service`]).
    pub fn new(profile: &Profile, key: Option<String>) -> Result<Voice, String> {
        let need = |key: Option<String>| {
            key.filter(|k| !k.trim().is_empty()).ok_or_else(|| {
                format!("Add your {} key in Settings › Connections to use this voice.", profile.vendor.name())
            })
        };
        let speed = if profile.speed > 0.0 { profile.speed.clamp(0.5, 2.0) } else { 1.0 };
        let engine: Arc<dyn Engine> = match profile.vendor {
            Vendor::System => Arc::new(system::System::new(&profile.voice, speed)),
            Vendor::Cartesia => Arc::new(cartesia::Cartesia::new(need(key)?, &profile.voice, speed, &profile.language)),
            Vendor::ElevenLabs => {
                if profile.voice.is_empty() {
                    return Err("Choose an ElevenLabs voice first.".into());
                }
                Arc::new(elevenlabs::ElevenLabs::new(need(key)?, &profile.voice, speed))
            }
            Vendor::Gemini => Arc::new(gemini::Gemini::new(need(key)?, &profile.voice)),
        };
        Ok(Voice { engine, profile: profile.clone() })
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Starts an utterance; its events go to `sink`.
    pub fn utterance(&self, sink: Sink) -> Box<dyn Utterance> {
        self.engine.utterance(sink)
    }

    /// Opens the vendor's connection ahead of time, so the first sound comes sooner.
    pub fn warm(&self) {
        self.engine.warm();
    }

    /// Says all of `text` (split at sentence ends, so it starts sooner).
    pub fn say_all(&self, text: &str, sink: Sink) -> Box<dyn Utterance> {
        let mut u = self.utterance(sink);
        for piece in sentences(text) {
            u.say(&piece);
        }
        u.end();
        u
    }
}

/// `text` cut at sentence ends (and line ends), each piece trimmed.
pub fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
            continue;
        }
        cur.push(c);
        let end = matches!(c, '.' | '!' | '?' | '…') && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if end && cur.trim().chars().count() > 3 {
            out.push(cur.trim().to_string());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A language name or code as a two-letter code ("English" → "en"); empty stays empty.
pub fn language_code(language: &str) -> String {
    let l = language.trim().to_lowercase();
    let map = [
        ("english", "en"),
        ("spanish", "es"),
        ("french", "fr"),
        ("german", "de"),
        ("portuguese", "pt"),
        ("japanese", "ja"),
        ("korean", "ko"),
        ("chinese", "zh"),
        ("italian", "it"),
        ("dutch", "nl"),
        ("indonesian", "id"),
        ("hindi", "hi"),
        ("russian", "ru"),
        ("polish", "pl"),
        ("turkish", "tr"),
        ("swedish", "sv"),
    ];
    if let Some((_, c)) = map.iter().find(|(n, _)| *n == l) {
        return (*c).to_string();
    }
    l.split(['-', '_']).next().unwrap_or("").to_string()
}

/// 16-bit little-endian PCM bytes as samples (an odd last byte is dropped).
pub(crate) fn samples(bytes: &[u8]) -> Vec<i16> {
    bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// A WAV file's samples and rate (16-bit PCM, any channel count: the first channel is kept).
pub fn read_wav(bytes: &[u8]) -> Result<(Vec<i16>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut at, mut rate, mut channels, mut bits) = (12, 0u32, 1u16, 16u16);
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes([body[2], body[3]]).max(1);
            rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            if bits != 16 {
                return Err(format!("{bits}-bit WAV isn't read"));
            }
            let all = samples(body);
            let pcm = all.chunks(channels as usize).map(|f| f[0]).collect();
            return Ok((pcm, rate));
        }
        at += 8 + len + (len & 1);
    }
    Err("the WAV file has no sound".into())
}

/// Samples as a 16-bit mono WAV file.
pub fn write_wav(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}
