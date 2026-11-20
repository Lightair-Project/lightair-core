//! The program preview for phones (lightair-live's docs/05 §4.2, Program preview):
//! `wss://host:4456/preview`, a WebSocket of its own so frames never queue behind replies.
//!
//! | Message | Direction | Content |
//! |---|---|---|
//! | `{"start": {"max_width": 720, "max_fps": 10, "canvas": id}}` | phone → desktop | Starts or changes the stream; `canvas` (optional) picks a format other than the primary one |
//! | binary frame | desktop → phone | 16-byte header (`u8 kind` 1 = JPEG, `u8 flags`, `u16 width`, `u16 height`, `u16 reserved`, `u64 pts_us`, big-endian) + the JPEG |
//! | `{"ack": pts_us}` | phone → desktop | After each frame it showed: the next comes after an ack or 500 ms, so a slow network gets fewer frames, never a backlog |
//! | `{"stop": true}` | phone → desktop | |
//!
//! Frames come from the app's `preview.frame` method (`max_width` and `canvas` in; `mime`, `width`, `height`,
//! base64 `data` out), called on this connection's thread only while a phone watches.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use tungstenite::protocol::WebSocket;
use tungstenite::{Error, Message};

use crate::Shared;
use crate::http::Request;

/// How long the desktop waits for an ack before sending the next frame anyway.
const ACK_WITHIN: Duration = Duration::from_millis(500);

pub(crate) fn run(
    stream: crate::secure::Tls,
    req: &Request,
    shared: &Arc<Shared>,
    peer: SocketAddr,
    client: &'static str,
) -> io::Result<()> {
    stream.sock.set_read_timeout(Some(Duration::from_millis(20)))?;
    let ws = crate::ws::accept(stream, req, None)?;
    let _joined = shared.join("Lightair phone preview", format!("{client} ({peer})"));
    stream_frames(ws, shared, client)
}

fn stream_frames<T: Read + Write>(mut ws: WebSocket<T>, shared: &Arc<Shared>, client: &'static str) -> io::Result<()> {
    let started = Instant::now();
    let mut watching: Option<(u64, u64, Value)> = None; // (max_width, max_fps, canvas)
    let mut sent: Option<(u64, Instant)> = None; // (pts, when), awaiting its ack
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    loop {
        if shared.stopping() {
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(());
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                if let Some(start) = v.get("start") {
                    let width = start.get("max_width").and_then(Value::as_u64).unwrap_or(720).clamp(160, 1280);
                    let fps = start.get("max_fps").and_then(Value::as_u64).unwrap_or(10).clamp(1, 15);
                    let canvas = start.get("canvas").filter(|c| c.is_string()).cloned().unwrap_or(Value::Null);
                    watching = Some((width, fps, canvas));
                    sent = None;
                } else if v.get("stop").is_some() {
                    watching = None;
                } else if let Some(pts) = v.get("ack").and_then(Value::as_u64)
                    && sent.is_some_and(|(p, _)| p == pts)
                {
                    sent = None;
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
        if let Some((width, fps, canvas)) = &watching {
            let (width, fps) = (*width, *fps);
            let due = last_frame.elapsed() >= Duration::from_millis(1000 / fps);
            let free = sent.is_none_or(|(_, at)| at.elapsed() >= ACK_WITHIN);
            if due && free {
                last_frame = Instant::now();
                match shared.call(client, "preview.frame", json!({"max_width": width, "canvas": canvas})) {
                    Ok(frame) => {
                        if let Some(bytes) = encode(&frame, started.elapsed().as_micros() as u64) {
                            let pts = u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes"));
                            ws.send(Message::binary(bytes)).map_err(|e| io::Error::other(e.to_string()))?;
                            sent = Some((pts, Instant::now()));
                        }
                    }
                    Err(e) => tracing::debug!("preview frame: {}", e.message),
                }
            }
        }
        match ws.flush() {
            Ok(()) => {}
            Err(Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
    }
}

/// The 16-byte header and the JPEG.
fn encode(frame: &Value, pts_us: u64) -> Option<Vec<u8>> {
    if frame.get("mime").and_then(Value::as_str) != Some("image/jpeg") {
        return None;
    }
    let data = base64::engine::general_purpose::STANDARD.decode(frame.get("data")?.as_str()?).ok()?;
    let width = frame.get("width").and_then(Value::as_u64).unwrap_or(0).min(u16::MAX as u64) as u16;
    let height = frame.get("height").and_then(Value::as_u64).unwrap_or(0).min(u16::MAX as u64) as u16;
    let mut out = Vec::with_capacity(16 + data.len());
    out.push(1); // JPEG
    out.push(0);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&pts_us.to_be_bytes());
    out.extend_from_slice(&data);
    Some(out)
}
