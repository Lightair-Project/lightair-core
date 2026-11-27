//! ElevenLabs Flash over the multi-context WebSocket: 16-bit PCM at 24 kHz, character timings
//! turned into mouth shapes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::ws::{Conn, Spec};
use crate::{Chunk, Engine, Event, Shape, Sink, Utterance};

const MODEL: &str = "eleven_flash_v2_5";
const RATE: u32 = 24_000;

type Ctxs = Arc<Mutex<HashMap<String, Sink>>>;

pub struct ElevenLabs {
    conn: Conn,
    ctxs: Ctxs,
    speed: f32,
}

static NEXT: AtomicU64 = AtomicU64::new(1);

impl ElevenLabs {
    pub fn new(key: String, voice: &str, speed: f32) -> ElevenLabs {
        let ctxs: Ctxs = Arc::default();
        let (on_text, on_close) = (ctxs.clone(), ctxs.clone());
        let conn = Conn::start(
            "elevenlabs",
            Spec {
                url: format!(
                    "wss://api.elevenlabs.io/v1/text-to-speech/{voice}/multi-stream-input?model_id={MODEL}&output_format=pcm_{RATE}&inactivity_timeout=180&sync_alignment=true"
                ),
                headers: vec![("xi-api-key", key)],
                // a context with only a space keeps the connection open between utterances
                keepalive: Some((
                    Duration::from_secs(15),
                    Some(json!({"text": " ", "context_id": "keepalive"}).to_string()),
                )),
            },
            Box::new(move |text| message(&on_text, text)),
            Box::new(move |why| {
                for (_, mut sink) in on_close.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                    sink(match &why {
                        Some(w) => Event::Failed(format!("ElevenLabs: {w}")),
                        None => Event::Done,
                    });
                }
            }),
        );
        ElevenLabs { conn, ctxs, speed }
    }
}

fn message(ctxs: &Ctxs, text: &str) {
    let Ok(m) = serde_json::from_str::<Value>(text) else { return };
    if let Some(e) = m["error"].as_str() {
        tracing::warn!(error = e, "elevenlabs");
    }
    let id = m["contextId"].as_str().or(m["context_id"].as_str()).unwrap_or("");
    let mut all = ctxs.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sink) = all.get_mut(id) else { return };
    if let Some(audio) = m["audio"].as_str()
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(audio)
    {
        let mut marks = Vec::new();
        let a = if m["alignment"].is_object() { &m["alignment"] } else { &m["normalizedAlignment"] };
        if let (Some(chars), Some(starts)) = (a["chars"].as_array(), a["charStartTimesMs"].as_array()) {
            for (c, t) in chars.iter().zip(starts) {
                if let (Some(s), Some(t)) =
                    (c.as_str().and_then(|c| c.chars().next()).and_then(Shape::of_letter), t.as_f64())
                {
                    marks.push((t as f32, s));
                }
            }
        }
        sink(Event::Audio(Chunk { pcm: crate::samples(&bytes), rate: RATE, marks }));
    }
    if m["isFinal"].as_bool() == Some(true) {
        sink(Event::Done);
        all.remove(id);
    }
}

impl Engine for ElevenLabs {
    fn utterance(&self, sink: Sink) -> Box<dyn Utterance> {
        let id = format!("u{}", NEXT.fetch_add(1, Ordering::Relaxed));
        self.ctxs.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), sink);
        Box::new(Say {
            id,
            conn: self.conn.clone(),
            ctxs: self.ctxs.clone(),
            speed: self.speed,
            first: true,
            done: false,
        })
    }

    fn warm(&self) {
        self.conn.warm();
    }
}

struct Say {
    id: String,
    conn: Conn,
    ctxs: Ctxs,
    speed: f32,
    first: bool,
    done: bool,
}

impl Utterance for Say {
    fn say(&mut self, piece: &str) {
        if self.done || piece.trim().is_empty() {
            return;
        }
        if self.first {
            self.first = false;
            self.conn.send(
                json!({"text": " ", "context_id": self.id, "voice_settings": {"stability": 0.5, "similarity_boost": 0.8, "speed": self.speed}})
                    .to_string(),
            );
        }
        self.conn.send(json!({"text": format!("{piece} "), "context_id": self.id, "flush": true}).to_string());
    }

    fn end(&mut self) {
        if self.done {
            return;
        }
        if self.first {
            // nothing was said: it's simply done
            if let Some(mut sink) = self.ctxs.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id) {
                sink(Event::Done);
            }
            return;
        }
        self.conn.send(json!({"context_id": self.id, "close_context": true}).to_string());
    }

    fn cancel(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.ctxs.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
        if !self.first {
            self.conn.send(json!({"context_id": self.id, "close_context": true}).to_string());
        }
    }
}
