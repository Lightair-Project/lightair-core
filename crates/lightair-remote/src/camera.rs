//! The phone camera's control channel (lightair-live's docs/05 §6): `wss://host:4456/camera?
//! source=<id>`, JSON messages both ways, for a device paired as the camera of that source.
//!
//! The app registers a [`CameraHost`] on the [`crate::SecureListener`]; each phone that connects
//! reaches it as a [`CameraChannel`] to send on (`configure`, `control`, `tally`, `keyframe`,
//! `media`), and its messages (`hello`, `state`, `health`) arrive at [`CameraHost::message`].
//! Clock sync is answered here: the phone sends `{"type": "time", "t1": …}` and gets `t2` (when
//! it arrived) and `t3` (when the answer left) on the host's clock, in microseconds. A source has
//! one phone at a time; a new connection for it replaces the old one.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tungstenite::protocol::WebSocket;
use tungstenite::{Error, Message};

use crate::Shared;
use crate::http::Request;
use crate::pairing::Device;

/// What the app does with phone cameras: the desktop's Phone camera source kind.
pub trait CameraHost: Send + Sync {
    /// A phone connected as the camera for `source`. Return false to refuse it (no such source).
    fn connected(&self, source: &str, device: &Device, channel: CameraChannel) -> bool;
    /// A message from that phone (`hello`, `state`, `health`, …), as it came.
    fn message(&self, source: &str, message: &Value);
    /// The phone went away (or another took its place).
    fn disconnected(&self, source: &str);
    /// The host's clock for time sync, in microseconds: the clock the media's timestamps are
    /// mapped to. By default, time since the remote API started.
    fn now_us(&self) -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
    }
}

/// Sends to one connected phone camera. Cheap to clone; sending after it left does nothing.
#[derive(Clone)]
pub struct CameraChannel {
    tx: Sender<Value>,
    /// Which connection this is, so a replaced one can't speak for its source.
    pub(crate) serial: u64,
}

impl CameraChannel {
    /// Sends a message (`{"type": "tally", …}` and the like). False once the phone is gone.
    pub fn send(&self, message: Value) -> bool {
        self.tx.send(message).is_ok()
    }
}

impl std::fmt::Debug for CameraChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraChannel").field("serial", &self.serial).finish()
    }
}

/// Which connection holds each source, so a replaced one doesn't report its source gone.
#[derive(Default)]
pub(crate) struct Cameras {
    pub host: Mutex<Option<Arc<dyn CameraHost>>>,
    holders: Mutex<Vec<(String, u64)>>,
    next: Mutex<u64>,
}

impl Cameras {
    fn take(&self, source: &str) -> u64 {
        let mut n = self.next.lock();
        *n += 1;
        let mut h = self.holders.lock();
        h.retain(|(s, _)| s != source);
        h.push((source.to_owned(), *n));
        *n
    }

    fn holds(&self, source: &str, serial: u64) -> bool {
        self.holders.lock().iter().any(|(s, n)| s == source && *n == serial)
    }

    fn release(&self, source: &str, serial: u64) -> bool {
        let mut h = self.holders.lock();
        let before = h.len();
        h.retain(|(s, n)| !(s == source && *n == serial));
        h.len() != before
    }
}

/// The `source` query parameter.
pub(crate) fn source_of(req: &Request) -> Option<String> {
    let query = req.path.split_once('?')?.1;
    query.split('&').find_map(|kv| kv.strip_prefix("source=")).map(percent_decode).filter(|s| !s.is_empty())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn run(
    stream: crate::secure::Tls,
    req: &Request,
    shared: &Arc<Shared>,
    cameras: &Arc<Cameras>,
    peer: SocketAddr,
    device: Device,
    source: String,
) -> io::Result<()> {
    let Some(host) = cameras.host.lock().clone() else {
        let mut stream = stream;
        return crate::http::respond(&mut stream, 404, "text/plain", b"This computer has no phone cameras", &[]);
    };
    stream.sock.set_read_timeout(Some(Duration::from_millis(20)))?;
    let ws = crate::ws::accept(stream, req, None)?;
    let _joined = shared.join("Lightair phone camera", format!("{} ({peer})", device.name));
    let serial = cameras.take(&source);
    let (tx, rx) = channel();
    if !host.connected(&source, &device, CameraChannel { tx, serial }) {
        cameras.release(&source, serial);
        let mut ws = ws;
        let _ = ws.close(None);
        let _ = ws.flush();
        return Ok(());
    }
    let result = talk(ws, shared, cameras, &host, &source, serial, rx);
    if cameras.release(&source, serial) {
        host.disconnected(&source);
    }
    result
}

fn talk<T: Read + Write>(
    mut ws: WebSocket<T>,
    shared: &Arc<Shared>,
    cameras: &Cameras,
    host: &Arc<dyn CameraHost>,
    source: &str,
    serial: u64,
    rx: Receiver<Value>,
) -> io::Result<()> {
    let send = |ws: &mut WebSocket<T>, v: Value| {
        ws.send(Message::text(v.to_string())).map_err(|e| io::Error::other(e.to_string()))
    };
    let mut heard = Instant::now();
    loop {
        // replaced by a newer connection for the same source, or the app is closing
        if shared.stopping() || !cameras.holds(source, serial) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(());
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                heard = Instant::now();
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                if v.get("type").and_then(Value::as_str) == Some("time") {
                    let t2 = host.now_us();
                    send(&mut ws, json!({"type": "time", "t1": v.get("t1"), "t2": t2, "t3": host.now_us()}))?;
                } else {
                    host.message(source, &v);
                }
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_)) => heard = Instant::now(),
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
        // the phone syncs time every 2 s and reports health every 1 s: silence means it's gone
        if heard.elapsed() > Duration::from_secs(10) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the phone camera went quiet"));
        }
        while let Ok(v) = rx.try_recv() {
            send(&mut ws, v)?;
        }
        match ws.flush() {
            Ok(()) => {}
            Err(Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
    }
}
