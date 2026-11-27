//! Gemini's text to speech (`streamGenerateContent`, 16-bit PCM at 24 kHz). No timings: the mouth
//! follows the sound itself. Free-tier keys get a few requests per model per day, so a model that
//! answers 429 (or 404) is skipped for the rest of the run.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::{Chunk, Engine, Sink, Utterance};

/// Gemini's speech models, best first.
pub const MODELS: &[&str] = &[
    "gemini-3.8-flash-tts",
    "gemini-3.1-flash-tts-preview",
    "gemini-2.5-flash-preview-tts",
    "gemini-3.8-flash-lite-tts",
    "gemini-2.5-pro-preview-tts",
];

/// The voice when none is chosen.
pub const DEFAULT_VOICE: &str = "Puck";

static EXHAUSTED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

pub struct Gemini {
    key: String,
    voice: String,
}

impl Gemini {
    pub fn new(key: String, voice: &str) -> Gemini {
        Gemini { key, voice: if voice.is_empty() { DEFAULT_VOICE.into() } else { voice.to_string() } }
    }
}

impl Engine for Gemini {
    fn utterance(&self, sink: Sink) -> Box<dyn Utterance> {
        let (key, voice) = (self.key.clone(), self.voice.clone());
        Box::new(crate::worker::spawn(
            "gemini",
            sink,
            Box::new(move |text, stop, out| speak(&key, &voice, text, stop, out)),
        ))
    }
}

/// One request: its sound streamed out as it comes.
fn speak(key: &str, voice: &str, text: &str, stop: &AtomicBool, out: &mut dyn FnMut(Chunk)) -> Result<(), String> {
    let agent = crate::net::agent(Duration::from_secs(60));
    let body = json!({
        "contents": [{"parts": [{"text": text}]}],
        "generationConfig": {"responseModalities": ["AUDIO"], "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice}}}},
    });
    let models: Vec<&'static str> = {
        let g = EXHAUSTED.lock().unwrap_or_else(|e| e.into_inner());
        MODELS.iter().copied().filter(|m| g.as_ref().is_none_or(|x| !x.contains(m))).collect()
    };
    for model in models {
        let url =
            format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse");
        let mut res = agent
            .post(&url)
            .header("x-goog-api-key", key)
            .header("content-type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("Gemini: {e}"))?;
        let status = res.status().as_u16();
        if status == 429 || status == 404 {
            tracing::warn!(model, status, "voice: Gemini model unavailable; trying the next");
            EXHAUSTED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashSet::new).insert(model);
            continue;
        }
        if !(200..300).contains(&status) {
            let text = res.body_mut().read_to_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|j| j["error"]["message"].as_str().map(str::to_string));
            return Err(match status {
                400 | 401 | 403 => "Gemini didn't accept the key (Settings › Connections › Google Gemini)".into(),
                _ => format!("Gemini answered {status}{}", msg.map(|m| format!(": {m}")).unwrap_or_default()),
            });
        }
        let reader = BufReader::new(res.body_mut().as_reader());
        for line in reader.lines() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let line = line.map_err(|e| format!("Gemini: {e}"))?;
            let Some(data) = line.strip_prefix("data: ") else { continue };
            let Ok(j) = serde_json::from_str::<Value>(data) else { continue };
            for p in j["candidates"][0]["content"]["parts"].as_array().into_iter().flatten() {
                if let Some(b64) = p["inlineData"]["data"].as_str()
                    && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
                {
                    out(Chunk { pcm: crate::samples(&bytes), rate: 24_000, marks: Vec::new() });
                }
            }
        }
        return Ok(());
    }
    Err("Gemini's voices are over their quota for today".into())
}
