//! Hearing with a cloud engine: a track's sound in (16 kHz mono 16-bit), words out.
//!
//! - **ElevenLabs Scribe** realtime, over a WebSocket: words as they're said, utterances ended by
//!   its own voice detection; v3 follows a switch between languages mid-sentence.
//! - **OpenRouter**'s transcription models: one request per utterance, cut out of the sound here
//!   by its level (OpenRouter's speech-to-text doesn't stream).
//!
//! Events are the same as the on-device helper's lines: ready, words so far, done, language,
//! error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use tungstenite::Message;

/// The sample rate hearing takes.
pub const RATE: u32 = 16_000;

/// What to hear with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HearConfig {
    /// "elevenlabs" or "openrouter".
    pub engine: String,
    /// The engine's model (empty: its default).
    pub model: String,
    /// A language code (empty: any).
    pub language: String,
    pub key: String,
}

/// What hearing says.
#[derive(Clone, Debug, PartialEq)]
pub enum HearEvent {
    /// It's listening (what with).
    Ready(String),
    /// The words of this utterance so far.
    Text(String),
    /// The utterance ended (the last `Text` is what was said).
    Done,
    /// The language it heard last.
    Lang(String),
    /// It can't hear right now, and why; it keeps trying.
    Error(String),
}

pub type HearSink = Box<dyn FnMut(HearEvent) + Send>;

/// Hearing one stream of sound; stops when dropped.
pub struct Hearer {
    tx: Sender<Vec<i16>>,
    stop: Arc<AtomicBool>,
}

impl Hearer {
    pub fn start(config: HearConfig, sink: HearSink) -> Hearer {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let _ = std::thread::Builder::new().name(format!("hear-{}", config.engine)).spawn(move || {
            let mut sink = Errors::new(sink);
            if config.key.trim().is_empty() {
                let name = if config.engine == "openrouter" { "OpenRouter" } else { "ElevenLabs" };
                sink.send(HearEvent::Error(format!("Add your {name} key in Settings › Connections to hear with it.")));
            }
            match config.engine.as_str() {
                "openrouter" => openrouter(config, rx, s, sink),
                _ => scribe(config, rx, s, sink),
            }
        });
        Hearer { tx, stop }
    }

    /// More sound.
    pub fn push(&self, pcm: &[i16]) {
        let _ = self.tx.send(pcm.to_vec());
    }
}

impl Drop for Hearer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The sink, saying each problem once until something works again.
struct Errors {
    sink: HearSink,
    last: String,
}

impl Errors {
    fn new(sink: HearSink) -> Errors {
        Errors { sink, last: String::new() }
    }

    fn send(&mut self, e: HearEvent) {
        match &e {
            HearEvent::Error(why) if *why == self.last => return,
            HearEvent::Error(why) => self.last = why.clone(),
            HearEvent::Ready(_) | HearEvent::Done => self.last.clear(),
            _ => {}
        }
        (self.sink)(e);
    }
}

// ---------------------------------------------------------------- ElevenLabs Scribe

fn scribe(cfg: HearConfig, rx: Receiver<Vec<i16>>, stop: Arc<AtomicBool>, mut sink: Errors) {
    let model = if cfg.model.is_empty() { "scribe_v3_live".to_string() } else { cfg.model.clone() };
    let mut url = format!(
        "wss://api.elevenlabs.io/v1/speech-to-text/realtime?model_id={model}&audio_format=pcm_16000&commit_strategy=vad&vad_silence_threshold_secs=0.7&include_timestamps=true"
    );
    if model == "scribe_v3_live" {
        url.push_str("&multilang_mode=true");
    }
    if !cfg.language.is_empty() {
        url.push_str(&format!("&language_code={}", cfg.language));
    }
    let mut retry = Duration::from_secs(1);
    let (mut last_committed, mut last_commit_at) = (String::new(), Instant::now());
    while !stop.load(Ordering::Relaxed) {
        if cfg.key.trim().is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            drain(&rx);
            continue;
        }
        let mut socket = match crate::net::connect(&url, &[("xi-api-key", cfg.key.as_str())], Duration::from_millis(20))
        {
            Ok(s) => s,
            Err(e) => {
                sink.send(HearEvent::Error(if e.contains("401") || e.contains("403") {
                    "ElevenLabs didn't accept the key: give it Speech to Text in ElevenLabs › Developers › API keys"
                        .into()
                } else {
                    format!("ElevenLabs: {e}; trying again")
                }));
                wait(&stop, &rx, retry);
                retry = (retry * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        let mut pending: Vec<i16> = Vec::new();
        'open: while !stop.load(Ordering::Relaxed) {
            // sound out, a tenth of a second at a time
            loop {
                match rx.try_recv() {
                    Ok(p) => pending.extend(p),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
            if pending.len() >= (RATE / 10) as usize {
                let bytes: Vec<u8> = pending.drain(..).flat_map(i16::to_le_bytes).collect();
                let m = json!({
                    "message_type": "input_audio_chunk",
                    "audio_base_64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "commit": false,
                    "sample_rate": RATE,
                });
                if socket.send(Message::text(m.to_string())).is_err() {
                    break 'open;
                }
            }
            // words in
            loop {
                match crate::net::read(&mut socket) {
                    Ok(Some(Message::Text(t))) => {
                        let Ok(m) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                        match m["message_type"].as_str().unwrap_or("") {
                            "session_started" => {
                                retry = Duration::from_secs(1);
                                let lang = if cfg.language.is_empty() {
                                    "any language".to_string()
                                } else {
                                    cfg.language.clone()
                                };
                                sink.send(HearEvent::Ready(format!("elevenlabs {model} {lang}")));
                            }
                            "partial_transcript" => {
                                if let Some(text) = m["text"].as_str().filter(|t| !t.is_empty()) {
                                    sink.send(HearEvent::Text(text.to_string()));
                                }
                            }
                            "committed_transcript" | "committed_transcript_with_timestamps" => {
                                if let Some(l) = m["language_code"].as_str() {
                                    sink.send(HearEvent::Lang(l.to_string()));
                                }
                                let text = m["text"].as_str().unwrap_or("").trim().to_string();
                                // with timestamps on, the same utterance may come twice
                                if text.is_empty()
                                    || (text == last_committed && last_commit_at.elapsed() < Duration::from_secs(2))
                                {
                                    continue;
                                }
                                last_committed = text.clone();
                                last_commit_at = Instant::now();
                                sink.send(HearEvent::Text(text));
                                sink.send(HearEvent::Done);
                            }
                            "auth_error" => {
                                retry = Duration::from_secs(30);
                                sink.send(HearEvent::Error(
                                    "ElevenLabs didn't accept the key: give it Speech to Text in ElevenLabs › Developers › API keys".into(),
                                ));
                            }
                            "quota_exceeded" => {
                                retry = Duration::from_secs(60);
                                sink.send(HearEvent::Error("ElevenLabs credits are used up".into()));
                            }
                            "unaccepted_terms" => {
                                retry = Duration::from_secs(60);
                                sink.send(HearEvent::Error(
                                    "Accept ElevenLabs' speech-to-text terms on elevenlabs.io first".into(),
                                ));
                            }
                            "insufficient_audio_activity" | "warning" => {}
                            _ => {
                                if let Some(e) = m["error"].as_str() {
                                    sink.send(HearEvent::Error(format!("ElevenLabs: {e}")));
                                }
                            }
                        }
                    }
                    Ok(Some(Message::Close(_))) | Err(_) => break 'open,
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        sink.send(HearEvent::Error("ElevenLabs closed the connection; trying again".into()));
        wait(&stop, &rx, retry);
        retry = (retry * 2).min(Duration::from_secs(60));
    }
}

fn drain(rx: &Receiver<Vec<i16>>) {
    while rx.try_recv().is_ok() {}
}

/// Waits (dropping sound meanwhile), unless stopped.
fn wait(stop: &AtomicBool, rx: &Receiver<Vec<i16>>, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        drain(rx);
    }
}

// ---------------------------------------------------------------- OpenRouter

/// Cuts utterances out of the sound by its level: speech starts after 60 ms above the noise floor
/// (plus 12 dB), ends after 700 ms below it or at 28 s; 300 ms before the start is kept.
struct Cutter {
    frame: Vec<i16>,
    preroll: std::collections::VecDeque<Vec<i16>>,
    speech: Vec<Vec<i16>>,
    loud_frames: u32,
    quiet_frames: u32,
    voiced_frames: u32,
    in_speech: bool,
    floor: f32,
}

const FRAME: usize = (RATE / 50) as usize; // 20 ms

impl Cutter {
    fn new() -> Cutter {
        Cutter {
            frame: Vec::with_capacity(FRAME),
            preroll: Default::default(),
            speech: Vec::new(),
            loud_frames: 0,
            quiet_frames: 0,
            voiced_frames: 0,
            in_speech: false,
            floor: -60.0,
        }
    }

    /// More sound; the utterances it finished.
    fn push(&mut self, pcm: &[i16]) -> Vec<Vec<i16>> {
        let mut out = Vec::new();
        for &s in pcm {
            self.frame.push(s);
            if self.frame.len() == FRAME {
                let f = std::mem::replace(&mut self.frame, Vec::with_capacity(FRAME));
                if let Some(u) = self.on_frame(f) {
                    out.push(u);
                }
            }
        }
        out
    }

    fn on_frame(&mut self, f: Vec<i16>) -> Option<Vec<i16>> {
        let sum: f64 = f.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let db = (10.0 * (sum / f.len() as f64 / (32768.0 * 32768.0) + 1e-12).log10()) as f32;
        // the floor follows quiet quickly and loud slowly
        self.floor += (db - self.floor) * if db < self.floor { 0.2 } else { 0.002 };
        let loud = db > (self.floor + 12.0).max(-50.0);
        if !self.in_speech {
            self.preroll.push_back(f);
            if self.preroll.len() > 15 {
                self.preroll.pop_front();
            }
            self.loud_frames = if loud { self.loud_frames + 1 } else { 0 };
            if self.loud_frames >= 3 {
                self.in_speech = true;
                self.speech = self.preroll.drain(..).collect();
                self.quiet_frames = 0;
                self.voiced_frames = self.loud_frames;
            }
            return None;
        }
        self.speech.push(f);
        if loud {
            self.quiet_frames = 0;
            self.voiced_frames += 1;
        } else {
            self.quiet_frames += 1;
        }
        if self.quiet_frames >= 35 || self.speech.len() >= 50 * 28 {
            // 400 ms of speech, not a cough
            let enough = self.voiced_frames >= 20;
            let all: Vec<i16> = self.speech.drain(..).flatten().collect();
            self.in_speech = false;
            self.loud_frames = 0;
            self.voiced_frames = 0;
            return enough.then_some(all);
        }
        None
    }
}

fn openrouter(cfg: HearConfig, rx: Receiver<Vec<i16>>, stop: Arc<AtomicBool>, sink: Errors) {
    let model = if cfg.model.is_empty() { "openai/gpt-4o-transcribe".to_string() } else { cfg.model.clone() };
    // requests on a thread of their own, in order, so cutting never waits on the network
    let (utt_tx, utt_rx) = channel::<Vec<i16>>();
    let sink = Arc::new(std::sync::Mutex::new(sink));
    {
        let (sink, cfg, model, stop) = (sink.clone(), cfg.clone(), model.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("hear-openrouter-requests".into()).spawn(move || {
            for pcm in utt_rx {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                transcribe(&cfg, &model, &pcm, &sink);
            }
        });
    }
    if !cfg.key.trim().is_empty() {
        let lang = if cfg.language.is_empty() { "any language".to_string() } else { cfg.language.clone() };
        sink.lock().unwrap_or_else(|e| e.into_inner()).send(HearEvent::Ready(format!("openrouter {model} {lang}")));
    }
    let mut cutter = Cutter::new();
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(pcm) => {
                for u in cutter.push(&pcm) {
                    if !cfg.key.trim().is_empty() {
                        let _ = utt_tx.send(u);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn transcribe(cfg: &HearConfig, model: &str, pcm: &[i16], sink: &std::sync::Mutex<Errors>) {
    let send = |e: HearEvent| sink.lock().unwrap_or_else(|e| e.into_inner()).send(e);
    let mut body = json!({
        "model": model,
        "input_audio": {"data": base64::engine::general_purpose::STANDARD.encode(crate::write_wav(pcm, RATE)), "format": "wav"},
    });
    if !cfg.language.is_empty() {
        body["language"] = json!(cfg.language);
    }
    let res = crate::net::agent(Duration::from_secs(30))
        .post("https://openrouter.ai/api/v1/audio/transcriptions")
        .header("Authorization", &format!("Bearer {}", cfg.key))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://lightair.app")
        .header("X-Title", "Lightair")
        .send(body.to_string());
    let mut res = match res {
        Ok(r) => r,
        Err(e) => return send(HearEvent::Error(format!("OpenRouter: {e}"))),
    };
    let status = res.status().as_u16();
    let j: Value = serde_json::from_str(&res.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let msg = j["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("it answered {status}"));
        return send(HearEvent::Error(match status {
            401 => "OpenRouter didn't accept the key".into(),
            402 => "OpenRouter credits are used up".into(),
            _ => format!("OpenRouter: {msg}"),
        }));
    }
    if let Some(l) = j["language"].as_str() {
        send(HearEvent::Lang(l.to_string()));
    }
    let text = j["text"].as_str().unwrap_or("").trim().to_string();
    if !text.is_empty() {
        send(HearEvent::Text(text));
        send(HearEvent::Done);
    }
}
