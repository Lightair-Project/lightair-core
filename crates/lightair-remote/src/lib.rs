//! Lightair's remote API (docs/12-chat-alerts-integrations.md, "Remote API"): one local server,
//! three ways in.
//!
//! - `ws://host:port/`: obs-websocket v5, so tools made for OBS (Stream Deck, Touch Portal, bots)
//!   work unchanged ([`obs`]).
//! - `ws://host:port/rpc`: Lightair's own JSON-RPC 2.0, every method and event ([`rpc`]).
//! - `http://host:port/mcp`: the Model Context Protocol (Streamable HTTP), so agents use Lightair
//!   as a set of tools ([`mcp`]); `lightair mcp` bridges it to stdio.
//!
//! - `wss://host:4456/rpc`: the same JSON-RPC for paired phones, over TLS with a pinned
//!   certificate and a key per device ([`secure`], [`pairing`]); advertised over mDNS ([`mdns`]).
//!
//! Each request becomes a [`Call`] the app answers with its [`Handler`] (Lightair's methods are
//! the app's business; this crate only carries them). Events the app [`Server::emit`]s reach every
//! client in its own protocol. It listens on this machine only unless LAN access is on, and
//! with a password every way in asks for it.

pub mod camera;
mod http;
pub mod identity;
pub mod mcp;
pub mod mdns;
mod obs;
pub mod pairing;
mod preview;
mod rpc;
pub mod secure;
pub mod transfer;
mod ws;

pub use camera::{CameraChannel, CameraHost};
pub use identity::Identity;
pub use pairing::{DesktopInfo, Device, DeviceRequest, Pairing, Scope};
pub use secure::{SECURE_PORT, SecureConfig, SecureListener};
pub use transfer::{FileSpec, Finished, Transfer, Transfers};

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde_json::Value;

/// obs-websocket's port, so OBS tools find Lightair where they look.
pub const DEFAULT_PORT: u16 = 4455;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub port: u16,
    /// Other devices on the network may connect (otherwise this machine only).
    pub lan: bool,
    /// Asked of every client when set.
    pub password: Option<String>,
}

/// A request for the app: a Lightair method (`scene.switch`, …) and its parameters.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub params: Value,
    /// Who asked ("MCP", "obs-websocket", "JSON-RPC"), for undo labels and logs.
    pub client: &'static str,
    /// The paired device that asked, on the phones' TLS port (`None` elsewhere): transfers and
    /// checkouts are tied to it.
    pub device: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No such method.
    Unknown,
    /// Missing or wrong parameters.
    Invalid,
    /// No such scene, object, source or track.
    NotFound,
    /// Not now (already recording, not live, …).
    State,
    /// It was tried and didn't work.
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    pub fn state(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::State, message)
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failed, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Reply = Result<Value, ApiError>;

/// Answers calls (on a connection's thread: it may block until the app has answered).
pub type Handler = Arc<dyn Fn(Call) -> Reply + Send + Sync>;

/// Something that happened in Lightair (`scene.program`, `record.state`, …).
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub data: Value,
}

/// A connected client, for Settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientInfo {
    pub protocol: &'static str,
    pub peer: String,
}

struct Client {
    id: u64,
    info: ClientInfo,
    events: crossbeam_channel::Sender<Event>,
}

pub(crate) struct Shared {
    config: Config,
    handler: Handler,
    stop: AtomicBool,
    clients: Mutex<Vec<Client>>,
    next: AtomicU64,
    transfers: Arc<Transfers>,
}

impl Shared {
    pub(crate) fn call(&self, client: &'static str, method: &str, params: Value) -> Reply {
        (self.handler)(Call { method: method.to_string(), params, client, device: None })
    }

    /// A call from a paired device.
    pub(crate) fn call_from(&self, client: &'static str, device: &str, method: &str, params: Value) -> Reply {
        (self.handler)(Call { method: method.to_string(), params, client, device: Some(device.to_owned()) })
    }

    pub(crate) fn transfers(&self) -> &Arc<Transfers> {
        &self.transfers
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Lists a client until the returned guard goes.
    pub(crate) fn join(self: &Arc<Self>, protocol: &'static str, peer: String) -> Joined {
        let (tx, rx) = crossbeam_channel::unbounded();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().push(Client { id, info: ClientInfo { protocol, peer }, events: tx });
        Joined { shared: self.clone(), id, events: rx }
    }

    /// The password matches (constant time), or none is asked.
    pub(crate) fn password_ok(&self, given: Option<&str>) -> bool {
        match &self.config.password {
            None => true,
            Some(p) => given.is_some_and(|g| same(g.as_bytes(), p.as_bytes())),
        }
    }

    pub(crate) fn password(&self) -> Option<&str> {
        self.config.password.as_deref()
    }
}

pub(crate) struct Joined {
    shared: Arc<Shared>,
    id: u64,
    pub events: crossbeam_channel::Receiver<Event>,
}

impl Drop for Joined {
    fn drop(&mut self) {
        self.shared.clients.lock().retain(|c| c.id != self.id);
    }
}

/// Compares without telling by its timing how much matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Random bytes, base64.
pub(crate) fn random_b64(n: usize) -> String {
    use base64::Engine;
    let mut b = vec![0u8; n];
    let _ = getrandom::fill(&mut b);
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// A new password: 24 letters and digits (easy to copy, ~143 bits).
pub fn new_password() -> String {
    const ABC: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut b = [0u8; 24];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| ABC[*x as usize % ABC.len()] as char).collect()
}

/// The running server. Dropping it stops it and disconnects everyone.
pub struct Server {
    shared: Arc<Shared>,
    addr: SocketAddr,
}

impl Server {
    pub fn start(config: Config, handler: Handler) -> io::Result<Server> {
        let host = if config.lan { "0.0.0.0" } else { "127.0.0.1" };
        let listener = TcpListener::bind((host, config.port))?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            config,
            handler,
            stop: AtomicBool::new(false),
            clients: Mutex::new(vec![]),
            next: AtomicU64::new(1),
            transfers: Arc::new(Transfers::default()),
        });
        let s = shared.clone();
        std::thread::Builder::new().name("lightair-remote".into()).spawn(move || {
            for stream in listener.incoming() {
                if s.stopping() {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let s = s.clone();
                let _ = std::thread::Builder::new().name("lightair-remote-client".into()).spawn(move || {
                    if let Err(e) = serve(stream, &s) {
                        tracing::debug!(%e, "remote API connection ended");
                    }
                });
            }
        })?;
        tracing::info!(%addr, "remote API listening");
        Ok(Server { shared, addr })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    /// Tells every client (in its protocol, if it has a name for it there).
    pub fn emit(&self, name: &str, data: Value) {
        let event = Event { name: name.to_string(), data };
        for c in self.shared.clients.lock().iter() {
            let _ = c.events.send(event.clone());
        }
    }

    /// Files on offer to paired devices, and uploads they send (docs/05 §7): served on the
    /// phones' TLS port.
    pub fn transfers(&self) -> Arc<Transfers> {
        self.shared.transfers.clone()
    }

    pub fn clients(&self) -> Vec<ClientInfo> {
        self.shared.clients.lock().iter().map(|c| c.info.clone()).collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // wakes the accept loop so it sees it's stopping
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.addr.port())),
            std::time::Duration::from_millis(200),
        );
    }
}

/// One connection: an HTTP request, then a WebSocket session or an answer.
fn serve(mut stream: TcpStream, shared: &Arc<Shared>) -> io::Result<()> {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    stream.set_nodelay(true).ok();
    loop {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        let Some(req) = http::read_request(&mut stream)? else { return Ok(()) };
        if shared.stopping() {
            return Ok(());
        }
        // a page elsewhere can't reach a server on this machine through its own name
        // (DNS rebinding), nor can a page on another site use it without the password
        if !shared.config.lan && !http::local_host(req.header("host")) {
            return http::respond(&mut stream, 403, "text/plain", b"Forbidden host", &[]);
        }
        let upgrade = req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
        let path = req.path.split('?').next().unwrap_or("/");
        match (req.method.as_str(), path) {
            ("GET", "/") if upgrade => return obs::run(stream, &req, shared, peer),
            ("GET", "/rpc") if upgrade => return rpc::run(stream, &req, shared, peer),
            ("POST", "/mcp") => {
                if !mcp::authorized(&req, shared) {
                    let body = br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"Lightair's remote API password is missing or wrong (Settings > Remote API)"}}"#;
                    return http::respond(
                        &mut stream,
                        401,
                        "application/json",
                        body,
                        &[("WWW-Authenticate", "Bearer")],
                    );
                }
                match mcp::handle(&req.body, shared) {
                    Some(body) => http::respond(&mut stream, 200, "application/json", body.as_bytes(), &[])?,
                    None => http::respond(&mut stream, 202, "application/json", b"", &[])?,
                }
                if !req.keep_alive() {
                    return Ok(());
                }
            }
            (_, "/mcp") => {
                return http::respond(&mut stream, 405, "text/plain", b"POST only", &[("Allow", "POST")]);
            }
            ("GET", "/") => {
                let body = serde_json::json!({
                    "name": "Lightair",
                    "version": env!("CARGO_PKG_VERSION"),
                    "ways_in": {
                        "obs-websocket v5": "ws://this-address/",
                        "JSON-RPC 2.0": "ws://this-address/rpc",
                        "MCP (Streamable HTTP)": "http://this-address/mcp",
                    },
                });
                return http::respond(&mut stream, 200, "application/json", body.to_string().as_bytes(), &[]);
            }
            _ => return http::respond(&mut stream, 404, "text/plain", b"Not found", &[]),
        }
    }
}

#[cfg(test)]
mod secure_tests;
#[cfg(test)]
mod tests;
