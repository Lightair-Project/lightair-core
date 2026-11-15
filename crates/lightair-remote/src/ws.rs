//! WebSocket sessions: the handshake, then messages in and events out on one thread.

use std::io::{self, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine;
use tungstenite::protocol::{CloseFrame, Role, WebSocket, frame::coding::CloseCode};
use tungstenite::{Error, Message};

use crate::http::Request;
use crate::{Event, Joined};

/// How often a quiet session looks for events to send and for the server stopping.
const POLL: Duration = Duration::from_millis(50);

/// Answers the upgrade (with `protocol` if the client offered it).
pub(crate) fn accept(mut stream: TcpStream, req: &Request, protocol: Option<&str>) -> io::Result<WebSocket<TcpStream>> {
    let key = req.header("sec-websocket-key").ok_or_else(|| io::Error::other("no Sec-WebSocket-Key"))?;
    let mut sha = sha1_smol::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64::engine::general_purpose::STANDARD.encode(sha.digest().bytes());
    let offered = req.header("sec-websocket-protocol").unwrap_or("");
    let chosen = protocol.filter(|p| offered.split(',').any(|o| o.trim() == *p));
    let mut head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(p) = chosen {
        head.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.set_read_timeout(Some(POLL))?;
    Ok(WebSocket::from_raw_socket(stream, Role::Server, None))
}

/// What a protocol does with a session.
pub(crate) trait Protocol {
    /// A text message: the messages to send back. `Err` closes with that code and reason.
    fn message(&mut self, text: &str) -> Result<Vec<String>, (u16, String)>;
    /// An event from the app: what to send for it (nothing if this client doesn't want it).
    fn event(&mut self, event: &Event) -> Option<String>;
    /// Sent first (obs-websocket's Hello).
    fn greeting(&mut self) -> Option<String> {
        None
    }
}

pub(crate) fn run(mut ws: WebSocket<TcpStream>, joined: Joined, shared: &crate::Shared, mut p: impl Protocol) -> io::Result<()> {
    if let Some(g) = p.greeting() {
        send(&mut ws, g)?;
    }
    loop {
        if shared.stopping() {
            let _ = ws.close(Some(CloseFrame { code: CloseCode::Away, reason: "Lightair stopped the remote API".into() }));
            let _ = ws.flush();
            return Ok(());
        }
        match ws.read() {
            Ok(Message::Text(t)) => match p.message(t.as_str()) {
                Ok(out) => {
                    for m in out {
                        send(&mut ws, m)?;
                    }
                }
                Err((code, reason)) => {
                    let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.into() }));
                    let _ = ws.flush();
                    return Ok(());
                }
            },
            Ok(Message::Binary(_)) => {
                let _ = ws.close(Some(CloseFrame { code: CloseCode::Unsupported, reason: "text messages only".into() }));
                let _ = ws.flush();
                return Ok(());
            }
            Ok(_) => {}
            Err(Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
        while let Ok(event) = joined.events.try_recv() {
            if let Some(m) = p.event(&event) {
                send(&mut ws, m)?;
            }
        }
        // pongs and close replies tungstenite queued
        match ws.flush() {
            Ok(()) => {}
            Err(Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(Error::ConnectionClosed | Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
    }
}

fn send(ws: &mut WebSocket<TcpStream>, text: String) -> io::Result<()> {
    ws.send(Message::text(text)).map_err(|e| io::Error::other(e.to_string()))
}
