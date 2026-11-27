//! Cartesia Sonic over its WebSocket: raw 16-bit PCM at 24 kHz, one context per utterance with
//! `continue` so sentences flow into each other, phoneme timestamps turned into mouth shapes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::ws::{Conn, Spec};
use crate::{Chunk, Engine, Event, Shape, Sink, Utterance};

const VERSION: &str = "2026-08-14";
const MODEL: &str = "sonic-3.6";
const RATE: u32 = 24_000;

struct Ctx {
    sink: Sink,
    /// Bytes of sound so far (to place the marks).
    bytes: usize,
    /// Marks not yet in a chunk: (ms from the utterance's start, shape).
    pending: Vec<(f32, Shape)>,
}

type Ctxs = Arc<Mutex<HashMap<String, Ctx>>>;

pub struct Cartesia {
    conn: Conn,
    ctxs: Ctxs,
    key: String,
    voice: Mutex<String>,
    speed: f32,
    language: String,
}

static NEXT: AtomicU64 = AtomicU64::new(1);

impl Cartesia {
    pub fn new(key: String, voice: &str, speed: f32, language: &str) -> Cartesia {
        let ctxs: Ctxs = Arc::default();
        let (on_text, on_close) = (ctxs.clone(), ctxs.clone());
        let conn = Conn::start(
            "cartesia",
            Spec {
                url: format!("wss://api.cartesia.ai/tts/websocket?cartesia_version={VERSION}"),
                headers: vec![("X-API-Key", key.clone())],
                keepalive: Some((Duration::from_secs(20), None)),
            },
            Box::new(move |text| message(&on_text, text)),
            Box::new(move |why| {
                for (_, mut c) in on_close.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                    (c.sink)(match &why {
                        Some(w) => Event::Failed(format!("Cartesia: {w}")),
                        None => Event::Done,
                    });
                }
            }),
        );
        let language = crate::language_code(language);
        Cartesia { conn, ctxs, key, voice: Mutex::new(voice.to_string()), speed, language }
    }

    /// The voice to use: the profile's, else the account's first.
    fn voice_id(&self) -> Result<String, String> {
        let mut v = self.voice.lock().unwrap_or_else(|e| e.into_inner());
        if !v.is_empty() {
            return Ok(v.clone());
        }
        let mut res = crate::net::agent(Duration::from_secs(10))
            .get("https://api.cartesia.ai/voices?limit=1")
            .header("X-API-Key", &self.key)
            .header("Cartesia-Version", VERSION)
            .call()
            .map_err(|e| format!("Cartesia: {e}"))?;
        let j: Value =
            serde_json::from_str(&res.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
        let list = j.as_array().cloned().or_else(|| j["data"].as_array().cloned()).unwrap_or_default();
        *v = list.first().and_then(|x| x["id"].as_str()).unwrap_or("").to_string();
        if v.is_empty() {
            return Err("Cartesia: no voice to use; pick one in the voice's settings".into());
        }
        Ok(v.clone())
    }
}

fn message(ctxs: &Ctxs, text: &str) {
    let Ok(m) = serde_json::from_str::<Value>(text) else { return };
    let id = m["context_id"].as_str().unwrap_or("");
    let mut all = ctxs.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = all.get_mut(id) else { return };
    match m["type"].as_str() {
        Some("chunk") => {
            let Some(data) = m["data"].as_str() else { return };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else { return };
            let start = c.bytes as f32 / 2.0 / RATE as f32 * 1000.0;
            let end = start + bytes.len() as f32 / 2.0 / RATE as f32 * 1000.0;
            let marks = c.pending.iter().filter(|k| k.0 >= start && k.0 < end).map(|k| (k.0 - start, k.1)).collect();
            c.pending.retain(|k| k.0 >= end);
            c.bytes += bytes.len();
            (c.sink)(Event::Audio(Chunk { pcm: crate::samples(&bytes), rate: RATE, marks }));
        }
        Some("phoneme_timestamps") => {
            let p = &m["phoneme_timestamps"];
            if let (Some(ph), Some(st)) = (p["phonemes"].as_array(), p["start"].as_array()) {
                for (ph, st) in ph.iter().zip(st) {
                    if let (Some(s), Some(t)) = (ph.as_str().and_then(Shape::of_phoneme), st.as_f64()) {
                        c.pending.push((t as f32 * 1000.0, s));
                    }
                }
            }
        }
        Some("done") => {
            (c.sink)(Event::Done);
            all.remove(id);
        }
        Some("error") => {
            let why = m["error"].as_str().or(m["message"].as_str()).unwrap_or("it refused the request").to_string();
            tracing::warn!(%why, "cartesia");
            (c.sink)(Event::Failed(format!("Cartesia: {why}")));
            all.remove(id);
        }
        _ => {}
    }
}

impl Engine for Cartesia {
    fn utterance(&self, mut sink: Sink) -> Box<dyn Utterance> {
        let id = format!("u{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let base = match self.voice_id() {
            Ok(voice) => {
                let mut base = json!({
                    "model_id": MODEL,
                    "voice": {"mode": "id", "id": voice},
                    "output_format": {"container": "raw", "encoding": "pcm_s16le", "sample_rate": RATE},
                    "context_id": id,
                    "add_phoneme_timestamps": true,
                });
                if !self.language.is_empty() {
                    base["language"] = json!(self.language);
                }
                if (self.speed - 1.0).abs() > 0.01 {
                    base["generation_config"] = json!({"speed": self.speed});
                }
                self.ctxs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), Ctx { sink, bytes: 0, pending: Vec::new() });
                Some(base)
            }
            Err(why) => {
                sink(Event::Failed(why));
                None
            }
        };
        Box::new(Say { id, base, conn: self.conn.clone(), ctxs: self.ctxs.clone(), done: false })
    }

    fn warm(&self) {
        self.conn.warm();
    }
}

struct Say {
    id: String,
    /// The request's fixed fields (`None`: it failed before it began).
    base: Option<Value>,
    conn: Conn,
    ctxs: Ctxs,
    done: bool,
}

impl Utterance for Say {
    fn say(&mut self, piece: &str) {
        let (Some(base), false) = (&self.base, self.done) else { return };
        if piece.trim().is_empty() {
            return;
        }
        let mut m = base.clone();
        m["transcript"] = json!(format!("{piece} "));
        m["continue"] = json!(true);
        self.conn.send(m.to_string());
    }

    fn end(&mut self) {
        let (Some(base), false) = (&self.base, self.done) else { return };
        let mut m = base.clone();
        m["transcript"] = json!("");
        m["continue"] = json!(false);
        self.conn.send(m.to_string());
    }

    fn cancel(&mut self) {
        if self.done || self.base.is_none() {
            return;
        }
        self.done = true;
        self.ctxs.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
        self.conn.send(json!({"context_id": self.id, "cancel": true}).to_string());
    }
}
