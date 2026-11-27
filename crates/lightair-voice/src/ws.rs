//! A WebSocket kept open on a thread of its own: messages to send go in through a channel, text
//! messages come out to a callback. It connects when first needed (or warmed), stays warm with a
//! keep-alive, and connects again on the next message after a drop.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use tungstenite::Message;

use crate::net;

enum Cmd {
    Send(String),
    Warm,
    Close,
}

/// What to connect to.
pub struct Spec {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    /// Every so often while open: a text message to send, or a ping when `None`.
    pub keepalive: Option<(Duration, Option<String>)>,
}

/// Text in; the connection closed (with why, when it failed).
pub type OnText = Box<dyn FnMut(&str) + Send>;
pub type OnClose = Box<dyn FnMut(Option<String>) + Send>;

/// A handle on the connection; clones share it, and it closes when the last one goes.
#[derive(Clone)]
pub struct Conn {
    tx: Sender<Cmd>,
    _guard: std::sync::Arc<Guard>,
}

pub struct Guard(Sender<Cmd>);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.send(Cmd::Close);
    }
}

impl Conn {
    pub fn start(name: &'static str, spec: Spec, on_text: OnText, on_close: OnClose) -> Conn {
        let (tx, rx) = channel();
        let _ =
            std::thread::Builder::new().name(format!("voice-{name}")).spawn(move || run(spec, rx, on_text, on_close));
        Conn { tx: tx.clone(), _guard: std::sync::Arc::new(Guard(tx)) }
    }

    pub fn send(&self, text: String) {
        let _ = self.tx.send(Cmd::Send(text));
    }

    /// Connects ahead of time.
    pub fn warm(&self) {
        let _ = self.tx.send(Cmd::Warm);
    }
}

fn run(spec: Spec, rx: Receiver<Cmd>, mut on_text: OnText, mut on_close: OnClose) {
    let headers: Vec<(&'static str, &str)> = spec.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut socket: Option<net::Socket> = None;
    let mut last_keepalive = Instant::now();
    loop {
        // what to send
        let first = if socket.is_some() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        let mut pending: Vec<Cmd> = first.into_iter().collect();
        while let Ok(c) = rx.try_recv() {
            pending.push(c);
        }
        for cmd in pending {
            match cmd {
                Cmd::Close => {
                    if let Some(mut s) = socket.take() {
                        let _ = s.close(None);
                    }
                    return;
                }
                Cmd::Warm | Cmd::Send(_) if socket.is_none() => {
                    match net::connect(&spec.url, &headers, Duration::from_millis(20)) {
                        Ok(s) => {
                            socket = Some(s);
                            last_keepalive = Instant::now();
                            if let Cmd::Send(text) = cmd
                                && let Some(s) = socket.as_mut()
                                && s.send(Message::text(text)).is_err()
                            {
                                socket = None;
                                on_close(Some("the connection dropped".into()));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "voice: couldn't connect");
                            on_close(Some(e));
                        }
                    }
                }
                Cmd::Warm => {}
                Cmd::Send(text) => {
                    if let Some(s) = socket.as_mut()
                        && s.send(Message::text(text)).is_err()
                    {
                        socket = None;
                        on_close(Some("the connection dropped".into()));
                    }
                }
            }
        }
        // what came in
        let Some(s) = socket.as_mut() else { continue };
        if let Some((every, text)) = &spec.keepalive
            && last_keepalive.elapsed() > *every
        {
            last_keepalive = Instant::now();
            let m = match text {
                Some(t) => Message::text(t.clone()),
                None => Message::Ping(Vec::new().into()),
            };
            let _ = s.send(m);
        }
        loop {
            match net::read(s) {
                Ok(Some(Message::Text(t))) => on_text(t.as_str()),
                Ok(Some(Message::Close(_))) => {
                    socket = None;
                    on_close(None);
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "voice: connection closed");
                    socket = None;
                    on_close(None);
                    break;
                }
            }
        }
    }
}
