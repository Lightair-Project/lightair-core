//! The TLS port for paired phones (docs/05 §1 and §4 in the mobile spec): `wss://host:4456/rpc`,
//! TLS 1.3 with the desktop's pinned self-signed [`Identity`], every connection signed with a
//! paired device's key ([`Pairing`]). The JSON-RPC is the same as `/rpc` on the plain port, with
//! the same handler and events, plus:
//!
//! - `session.hello` (answered here): the desktop, protocol 2, its features, the device's scope;
//! - `pair.claim`, on a connection that asked to pair (`Lightair-Pairing: 1`);
//! - `pair.forget`: the device removes itself;
//! - scopes: a `watch` device can only read;
//! - `audio.levels` (20 Hz) only reaches sessions that subscribed to it by name;
//! - `wss://host:4456/preview`: the program picture as a stream of frames ([`crate::preview`]);
//! - `wss://host:4456/camera?source=<id>`: a phone camera's control channel, for a device paired
//!   as that source's camera ([`crate::camera`]).
//!
//! Each call reaches the app with the device's name as its client, so undo says
//! "Sam's iPhone: Mute Mic".

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::http::{self, Request};
use crate::identity::Identity;
use crate::pairing::{DesktopInfo, Device, Pairing, Scope};
use crate::rpc::{self, INVALID_REQUEST, PARSE, UNAUTHORIZED, error};
use crate::{Event, Server, Shared};

/// The port phones use.
pub const SECURE_PORT: u16 = 4456;
/// The protocol version `session.hello` speaks (docs/05 §9).
pub const PROTOCOL: u32 = 2;

/// The secure listener's settings.
#[derive(Debug, Clone)]
pub struct SecureConfig {
    /// [`SECURE_PORT`], or 0 for any (tests).
    pub port: u16,
    pub identity: Identity,
    pub desktop: DesktopInfo,
    /// The feature names this desktop answers (docs/05 §9: `audio.levels`, `scene.thumbnails`, …).
    pub features: Vec<String>,
}

/// The running TLS listener. Dropping it stops accepting phones.
pub struct SecureListener {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    cameras: Arc<crate::camera::Cameras>,
}

impl SecureListener {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Where phone cameras go (docs/05 §6). Until it's set, `/camera` answers 404.
    pub fn cameras(&self, host: Arc<dyn crate::camera::CameraHost>) {
        *self.cameras.host.lock() = Some(host);
    }
}

impl Drop for SecureListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.addr.port())),
            Duration::from_millis(200),
        );
    }
}

/// A device's name as a call's client (interned: one per name).
pub(crate) fn client_label(name: &str) -> &'static str {
    static NAMES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut names = NAMES.get_or_init(Default::default).lock();
    if let Some(n) = names.get(name) {
        return n;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(name.to_owned(), leaked);
    leaked
}

struct Secure {
    config: SecureConfig,
    pairing: Arc<Pairing>,
    tls: Arc<rustls::ServerConfig>,
    stop: Arc<AtomicBool>,
    cameras: Arc<crate::camera::Cameras>,
}

impl Server {
    /// Starts the TLS port for paired phones beside this server, sharing its handler and events.
    /// It listens on every interface: nothing works there without a pairing the person allowed.
    pub fn start_secure(&self, config: SecureConfig, pairing: Arc<Pairing>) -> io::Result<SecureListener> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(config.identity.key_der.clone().into());
        let cert = rustls::pki_types::CertificateDer::from(config.identity.cert_der.clone());
        let tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .map_err(io::Error::other)?;
        let listener = TcpListener::bind(("0.0.0.0", config.port))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let cameras = Arc::new(crate::camera::Cameras::default());
        let secure =
            Arc::new(Secure { config, pairing, tls: Arc::new(tls), stop: stop.clone(), cameras: cameras.clone() });
        let shared = self.shared.clone();
        std::thread::Builder::new().name("lightair-remote-tls".into()).spawn(move || {
            for stream in listener.incoming() {
                if secure.stop.load(Ordering::Relaxed) || shared.stopping() {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (secure, shared) = (secure.clone(), shared.clone());
                let _ = std::thread::Builder::new().name("lightair-remote-phone".into()).spawn(move || {
                    if let Err(e) = serve(stream, &secure, &shared) {
                        tracing::debug!(%e, "phone connection ended");
                    }
                });
            }
        })?;
        tracing::info!(%addr, "remote API for phones listening (TLS)");
        Ok(SecureListener { addr, stop, cameras })
    }
}

pub(crate) type Tls = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

fn serve(tcp: TcpStream, secure: &Arc<Secure>, shared: &Arc<Shared>) -> io::Result<()> {
    let peer = tcp.peer_addr()?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_secs(15)))?;
    let conn = rustls::ServerConnection::new(secure.tls.clone()).map_err(io::Error::other)?;
    let mut stream: Tls = rustls::StreamOwned::new(conn, tcp);
    let Some(req) = http::read_request(&mut stream)? else { return Ok(()) };
    let upgrade = req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    let path = req.path.split('?').next().unwrap_or("/");
    if !(req.method == "GET" && matches!(path, "/rpc" | "/preview" | "/camera") && upgrade) {
        return http::respond(&mut stream, 404, "text/plain", b"Not found", &[]);
    }
    let session = if let Some(auth) = req.header("authorization").filter(|a| a.starts_with("Lightair-Key ")) {
        match secure.pairing.authenticate(auth) {
            Ok(device) => Access::Device(Box::new(device)),
            Err(refused) => {
                let body = json!({"error": refused.0}).to_string();
                return http::respond(&mut stream, 401, "application/json", body.as_bytes(), &[]);
            }
        }
    } else if req.header("lightair-pairing") == Some("1") {
        Access::Pairing
    } else {
        let nonce = secure.pairing.challenge();
        return http::respond(
            &mut stream,
            401,
            "text/plain",
            b"Sign in with a paired device's key",
            &[("Lightair-Challenge", &nonce)],
        );
    };
    match (path, session) {
        ("/preview", Access::Device(device)) if device.scope == Scope::Camera => {
            http::respond(&mut stream, 403, "text/plain", b"A camera doesn't watch the program", &[])
        }
        ("/preview", Access::Device(device)) => {
            crate::preview::run(stream, &req, shared, peer, client_label(&device.name))
        }
        ("/preview" | "/camera", Access::Pairing) => http::respond(&mut stream, 401, "text/plain", b"Pair first", &[]),
        ("/camera", Access::Device(device)) => {
            let source = crate::camera::source_of(&req);
            // only the phone paired as this source's camera
            if device.scope != Scope::Camera || source.is_none() || device.source != source {
                let body = json!({"error": "This phone isn't paired as this layer's camera. Scan its code again."})
                    .to_string();
                return http::respond(&mut stream, 403, "application/json", body.as_bytes(), &[]);
            }
            let source = source.unwrap_or_default();
            crate::camera::run(stream, &req, shared, &secure.cameras, peer, *device, source)
        }
        (_, session) => run(stream, &req, secure, shared, peer, session),
    }
}

enum Access {
    /// Only `session.hello` and `pair.claim`.
    Pairing,
    Device(Box<Device>),
}

fn run(
    stream: Tls,
    req: &Request,
    secure: &Arc<Secure>,
    shared: &Arc<Shared>,
    peer: SocketAddr,
    access: Access,
) -> io::Result<()> {
    stream.sock.set_read_timeout(Some(crate::ws::READ_TIMEOUT))?;
    let ws = crate::ws::accept(stream, req, None)?;
    let label = match &access {
        Access::Device(d) => format!("{} ({})", d.name, peer),
        Access::Pairing => format!("pairing ({peer})"),
    };
    let joined = shared.join("Lightair phone", label);
    let session = Session { secure: secure.clone(), shared: shared.clone(), peer: peer.ip(), access, events: None };
    crate::ws::run(ws, joined, shared, session)
}

struct Session {
    secure: Arc<Secure>,
    shared: Arc<Shared>,
    peer: IpAddr,
    access: Access,
    events: Option<Vec<String>>,
}

impl Session {
    fn scope(&self) -> Option<Scope> {
        match &self.access {
            Access::Device(d) => Some(d.scope),
            Access::Pairing => None,
        }
    }

    fn one(&mut self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let reply_id = id.clone().unwrap_or(Value::Null);
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return Some(error(&reply_id, INVALID_REQUEST, "a request needs a method"));
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let desktop = &self.secure.config.desktop;
        let result = match (method, &self.access) {
            ("session.hello", _) => Ok(json!({
                "desktop": {"id": desktop.id, "name": desktop.name, "version": desktop.version},
                "protocol": PROTOCOL,
                "features": self.secure.config.features,
                "scope": self.scope().map(Scope::as_str),
            })),
            ("pair.claim", Access::Pairing) => {
                self.secure.pairing.claim(&params, self.peer, desktop).map_err(|r| (UNAUTHORIZED, r.0))
            }
            (_, Access::Pairing) => Err((UNAUTHORIZED, "Pair first (pair.claim).".to_string())),
            ("pair.forget", Access::Device(d)) => {
                self.secure.pairing.remove(&d.id);
                Ok(json!({"forgotten": true}))
            }
            (m, Access::Device(d)) if !d.scope.allows(m) => {
                Err((UNAUTHORIZED, format!("{} can only watch, not change the show.", d.name)))
            }
            ("events.subscribe", Access::Device(_)) => {
                self.events = params
                    .get("events")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect());
                Ok(json!({"events": self.events}))
            }
            (m, Access::Device(d)) => {
                let client = client_label(&d.name);
                self.shared.call(client, m, params).map_err(|e| (rpc::code(e.kind), e.message))
            }
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((c, m)) => error(&id, c, &m),
        })
    }
}

impl crate::ws::Protocol for Session {
    fn message(&mut self, text: &str) -> Result<Vec<String>, (u16, String)> {
        let reply = match serde_json::from_str::<Value>(text) {
            Err(e) => Some(error(&Value::Null, PARSE, &e.to_string())),
            Ok(Value::Array(batch)) if batch.is_empty() => Some(error(&Value::Null, INVALID_REQUEST, "empty batch")),
            Ok(Value::Array(batch)) => {
                let out: Vec<Value> = batch.into_iter().filter_map(|m| self.one(m)).collect();
                (!out.is_empty()).then_some(Value::Array(out))
            }
            Ok(m) => self.one(m),
        };
        Ok(reply.map(|r| r.to_string()).into_iter().collect())
    }

    fn event(&mut self, event: &Event) -> Option<String> {
        if !matches!(self.access, Access::Device(_)) {
            return None;
        }
        let named = self.events.as_ref().is_some_and(|e| e.contains(&event.name));
        // meters only to those who asked for them by name (docs/05 §4.2, Audio)
        let wanted = if event.name == "audio.levels" { named } else { self.events.is_none() || named };
        wanted.then(|| {
            json!({"jsonrpc": "2.0", "method": "event", "params": {"name": event.name, "data": event.data}}).to_string()
        })
    }
}
