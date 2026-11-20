//! The TLS port end to end: a phone pairs, signs in, says hello, calls, gets events.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::Message;

use crate::pairing::{DesktopInfo, Pairing, Scope, sign_header};
use crate::{ApiError, Call, Config, Handler, Identity, SecureConfig, Server};

/// Trusts exactly one certificate, by its SHA-256, as the phones do.
#[derive(Debug)]
struct Pinned([u8; 32]);

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        cert: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if Sha256::digest(cert.as_ref())[..] == self.0[..] {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("This computer's identity changed. Pair again.".into()))
        }
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.3 only".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

type Phone = tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

fn tls(port: u16, pin: [u8; 32]) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned(pin)))
        .with_no_client_auth();
    let conn = rustls::ClientConnection::new(Arc::new(config), "lightair.invalid".try_into().unwrap()).unwrap();
    rustls::StreamOwned::new(conn, TcpStream::connect(("127.0.0.1", port)).unwrap())
}

/// Connects to /rpc with extra headers; `Err((status, headers))` when refused.
fn connect(port: u16, pin: [u8; 32], headers: &[(&'static str, String)]) -> Result<Phone, (u16, String)> {
    connect_to(port, pin, "/rpc", headers)
}

fn connect_to(
    port: u16,
    pin: [u8; 32],
    path: &str,
    headers: &[(&'static str, String)],
) -> Result<Phone, (u16, String)> {
    let mut req =
        tungstenite::client::IntoClientRequest::into_client_request(format!("ws://127.0.0.1:{port}{path}")).unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(*k, v.parse().unwrap());
    }
    match tungstenite::client(req, tls(port, pin)) {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => {
            let nonce =
                r.headers().get("lightair-challenge").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
            Err((r.status().as_u16(), nonce))
        }
        Err(e) => panic!("{e}"),
    }
}

fn call(ws: &mut Phone, id: u64, method: &str, params: Value) -> Value {
    ws.send(Message::text(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()))
        .unwrap();
    loop {
        let m = ws.read().unwrap();
        let v: Value = serde_json::from_str(m.to_text().unwrap()).unwrap();
        if v["id"] == id {
            return v;
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[test]
fn a_phone_pairs_signs_in_and_controls() {
    let handler: Handler = Arc::new(|c: Call| match c.method.as_str() {
        "state.get" => Ok(json!({"program": {"id": "s1", "name": "Main"}, "client": c.client})),
        "scene.switch" => Ok(json!({})),
        _ => Err(ApiError::new(crate::ErrorKind::Unknown, "no such method")),
    });
    let server = Server::start(Config { port: 0, lan: false, password: Some("pw".into()) }, handler).unwrap();
    let identity = Identity::generate("7f3c").unwrap();
    let pin = identity.fingerprint();
    let desktop = DesktopInfo { id: "7f3c".into(), name: "Studio Mac".into(), version: "0.4.0".into() };
    let pairing = Pairing::new(vec![], |r| r.name == "Sam's iPhone", |_| {});
    let config = SecureConfig { port: 0, identity, desktop, features: vec!["audio.levels".into()] };
    let secure = server.start_secure(config, pairing.clone()).unwrap();
    let port = secure.addr().port();
    let key = SigningKey::from_bytes(&[3; 32]);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // a wrong pin fails closed
    assert!(std::panic::catch_unwind(|| connect(port, [0; 32], &[])).is_err());

    // without a key: a challenge, nothing else
    let (status, nonce) = connect(port, pin, &[]).unwrap_err();
    assert_eq!(status, 401);
    assert!(!nonce.is_empty());

    // pairing: hello works, other calls don't, the claim does
    let offer = pairing.offer(Scope::Control);
    let mut ws = connect(port, pin, &[("Lightair-Pairing", "1".into())]).unwrap();
    assert_eq!(call(&mut ws, 1, "session.hello", json!({}))["result"]["desktop"]["name"], "Studio Mac");
    assert_eq!(call(&mut ws, 2, "state.get", json!({}))["error"]["code"], -32001);
    let claim = json!({
        "secret": b64.encode(offer.secret),
        "device": {"id": "phone-1", "name": "Sam's iPhone", "model": "iPhone17,1", "platform": "iOS", "app_version": "0.1.0"},
        "public_key": b64.encode(key.verifying_key().to_bytes()),
    });
    let r = call(&mut ws, 3, "pair.claim", claim);
    assert_eq!(r["result"]["device_id"], "phone-1", "{r}");
    drop(ws);

    // signed in: hello says control; calls reach the app as the phone
    let (_, nonce) = connect(port, pin, &[]).unwrap_err();
    let auth = sign_header("phone-1", &nonce, now(), |m| key.sign(m).to_bytes());
    let mut ws = connect(port, pin, &[("Authorization", auth.clone())]).unwrap();
    let hello = call(&mut ws, 1, "session.hello", json!({"protocol": 2}));
    assert_eq!(hello["result"]["scope"], "control");
    assert_eq!(hello["result"]["protocol"], 2);
    assert_eq!(call(&mut ws, 2, "state.get", json!({}))["result"]["client"], "Sam's iPhone");
    // events: everything but meters, unless asked for by name
    server.emit("audio.levels", json!({"tracks": []}));
    server.emit("scene.program", json!({"id": "s2", "name": "BRB"}));
    let m: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(m["params"]["name"], "scene.program");

    // the same signature doesn't work twice
    assert_eq!(connect(port, pin, &[("Authorization", auth)]).unwrap_err().0, 401);

    // a watch device can read, not change
    let watcher = SigningKey::from_bytes(&[4; 32]);
    let offer = pairing.offer(Scope::Watch);
    let mut ws = connect(port, pin, &[("Lightair-Pairing", "1".into())]).unwrap();
    let claim = json!({
        "secret": b64.encode(offer.secret),
        "device": {"id": "phone-2", "name": "Sam's iPhone"},
        "public_key": b64.encode(watcher.verifying_key().to_bytes()),
    });
    call(&mut ws, 1, "pair.claim", claim);
    let (_, nonce) = connect(port, pin, &[]).unwrap_err();
    let mut ws =
        connect(port, pin, &[("Authorization", sign_header("phone-2", &nonce, now(), |m| watcher.sign(m).to_bytes()))])
            .unwrap();
    assert!(call(&mut ws, 1, "state.get", json!({}))["result"].is_object());
    assert_eq!(call(&mut ws, 2, "scene.switch", json!({"scene": "BRB"}))["error"]["code"], -32001);

    // the plain port is unchanged
    let mut plain = TcpStream::connect(("127.0.0.1", server.addr().port())).unwrap();
    write!(plain, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    plain.read_to_string(&mut out).unwrap();
    assert!(out.contains("JSON-RPC 2.0"));
}

#[test]
fn a_paired_phone_watches_the_preview() {
    let jpeg = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF, 0xD9]);
    let handler: Handler = Arc::new(move |c: Call| match c.method.as_str() {
        "preview.frame" => {
            assert_eq!(c.params["max_width"], 480);
            Ok(json!({"mime": "image/jpeg", "width": 480, "height": 270, "data": jpeg}))
        }
        _ => Err(ApiError::new(crate::ErrorKind::Unknown, "no such method")),
    });
    let server = Server::start(Config { port: 0, lan: false, password: None }, handler).unwrap();
    let identity = Identity::generate("d").unwrap();
    let pin = identity.fingerprint();
    let desktop = DesktopInfo { id: "d".into(), name: "Desk".into(), version: "1".into() };
    let pairing = Pairing::new(vec![], |_| true, |_| {});
    let secure =
        server.start_secure(SecureConfig { port: 0, identity, desktop, features: vec![] }, pairing.clone()).unwrap();
    let port = secure.addr().port();
    let key = SigningKey::from_bytes(&[5; 32]);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let offer = pairing.offer(Scope::Watch);
    let mut ws = connect(port, pin, &[("Lightair-Pairing", "1".into())]).unwrap();
    call(
        &mut ws,
        1,
        "pair.claim",
        json!({"secret": b64.encode(offer.secret), "device": {"id": "p"}, "public_key": b64.encode(key.verifying_key().to_bytes())}),
    );

    // unsigned: refused
    assert_eq!(connect_to(port, pin, "/preview", &[]).unwrap_err().0, 401);
    let (_, nonce) = connect(port, pin, &[]).unwrap_err();
    let auth = sign_header("p", &nonce, now(), |m| key.sign(m).to_bytes());
    let mut ws = connect_to(port, pin, "/preview", &[("Authorization", auth)]).unwrap();
    ws.send(Message::text(json!({"start": {"max_width": 480, "max_fps": 10}}).to_string())).unwrap();
    let frame = loop {
        if let Message::Binary(b) = ws.read().unwrap() {
            break b;
        }
    };
    assert_eq!(frame[0], 1, "JPEG");
    assert_eq!(u16::from_be_bytes([frame[2], frame[3]]), 480);
    assert_eq!(u16::from_be_bytes([frame[4], frame[5]]), 270);
    assert_eq!(&frame[16..], &[0xFF, 0xD8, 0xFF, 0xD9]);
    // acked: the next frame follows
    let pts = u64::from_be_bytes(frame[8..16].try_into().unwrap());
    ws.send(Message::text(json!({"ack": pts}).to_string())).unwrap();
    let next = loop {
        if let Message::Binary(b) = ws.read().unwrap() {
            break b;
        }
    };
    assert!(u64::from_be_bytes(next[8..16].try_into().unwrap()) > pts);
}

/// A Phone camera layer as the desktop would be: it records what reaches it.
#[derive(Default)]
struct Layer {
    channel: parking_lot::Mutex<Option<crate::CameraChannel>>,
    heard: parking_lot::Mutex<Vec<Value>>,
    gone: std::sync::atomic::AtomicUsize,
}

impl crate::CameraHost for Layer {
    fn connected(&self, source: &str, _: &crate::Device, channel: crate::CameraChannel) -> bool {
        if source != "cam-1" {
            return false;
        }
        *self.channel.lock() = Some(channel);
        true
    }
    fn message(&self, _: &str, message: &Value) {
        self.heard.lock().push(message.clone());
    }
    fn disconnected(&self, _: &str) {
        self.gone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    fn now_us(&self) -> u64 {
        5_000_000
    }
}

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !ok() {
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_phone_pairs_as_a_camera_and_talks_to_its_layer() {
    let handler: Handler = Arc::new(|_| Err(ApiError::new(crate::ErrorKind::Unknown, "no such method")));
    let server = Server::start(Config { port: 0, lan: false, password: None }, handler).unwrap();
    let identity = Identity::generate("d1").unwrap();
    let pin = identity.fingerprint();
    let fp = identity.fingerprint_b64();
    let desktop = DesktopInfo { id: "d1".into(), name: "Studio Mac".into(), version: "0.4.0".into() };
    let pairing = Pairing::new(vec![], |r| r.scope == Scope::Camera && r.source.as_deref() == Some("cam-1"), |_| {});
    let secure = server
        .start_secure(SecureConfig { port: 0, identity, desktop: desktop.clone(), features: vec![] }, pairing.clone())
        .unwrap();
    let port = secure.addr().port();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // the layer's code carries its source
    let offer = pairing.offer_camera("cam-1");
    assert!(pairing.link(&offer, &desktop, &["127.0.0.1".into()], port, &fp).ends_with("&c=cam-1"));
    let key = SigningKey::from_bytes(&[7; 32]);
    let mut ws = connect(port, pin, &[("Lightair-Pairing", "1".into())]).unwrap();
    let claim = json!({
        "secret": b64.encode(offer.secret),
        "device": {"id": "cam-phone", "name": "Old iPhone", "model": "iPhone12,1", "platform": "iOS", "app_version": "0.1.0"},
        "public_key": b64.encode(key.verifying_key().to_bytes()),
    });
    let r = call(&mut ws, 1, "pair.claim", claim);
    assert_eq!((r["result"]["scope"].as_str(), r["result"]["source"].as_str()), (Some("camera"), Some("cam-1")), "{r}");
    drop(ws);
    assert_eq!(pairing.devices()[0].source.as_deref(), Some("cam-1"));

    let signed = |path: &str| {
        let (_, nonce) = connect(port, pin, &[]).unwrap_err();
        let auth = sign_header("cam-phone", &nonce, now(), |m| key.sign(m).to_bytes());
        connect_to(port, pin, path, &[("Authorization", auth)])
    };
    // no host yet: nothing there
    assert_eq!(signed("/camera?source=cam-1").unwrap_err().0, 404);
    let layer = Arc::new(Layer::default());
    secure.cameras(layer.clone());
    // another layer's channel, or the remote's, isn't this phone's
    assert_eq!(signed("/camera?source=cam-2").unwrap_err().0, 403);
    assert_eq!(signed("/camera").unwrap_err().0, 403);

    let mut cam = signed("/camera?source=cam-1").unwrap();
    cam.send(Message::text(json!({"type": "hello", "device": {"model": "iPhone12,1"}}).to_string())).unwrap();
    wait_for("hello", || layer.heard.lock().iter().any(|m| m["type"] == "hello"));
    // the layer speaks: tally reaches the phone
    assert!(
        layer
            .channel
            .lock()
            .as_ref()
            .unwrap()
            .send(json!({"type": "tally", "program": true, "staged": false, "live": true}))
    );
    let m: Value = serde_json::from_str(cam.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!((m["type"].as_str(), m["live"].as_bool()), (Some("tally"), Some(true)));
    // time sync is answered on the layer's clock
    cam.send(Message::text(json!({"type": "time", "t1": 42}).to_string())).unwrap();
    let m: Value = serde_json::from_str(cam.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!((m["t1"].as_u64(), m["t2"].as_u64(), m["t3"].as_u64()), (Some(42), Some(5_000_000), Some(5_000_000)));

    // a camera device can't watch the program or control the show
    assert_eq!(signed("/preview").unwrap_err().0, 403);
    let mut rpc = signed("/rpc").unwrap();
    assert_eq!(call(&mut rpc, 1, "session.hello", json!({}))["result"]["scope"], "camera");
    assert_eq!(call(&mut rpc, 2, "scene.switch", json!({}))["error"]["code"], -32001);

    // a new connection for the source replaces the old one; leaving tells the layer
    let mut again = signed("/camera?source=cam-1").unwrap();
    again.send(Message::text(json!({"type": "health", "battery": 0.5}).to_string())).unwrap();
    wait_for("health", || layer.heard.lock().iter().any(|m| m["type"] == "health"));
    assert_eq!(
        layer.gone.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a replaced connection doesn't report its source gone"
    );
    drop(again);
    wait_for("disconnected", || layer.gone.load(std::sync::atomic::Ordering::SeqCst) == 1);
}

/// One raw HTTP request over the pinned TLS port, on a connection kept for the next.
fn http_req(
    s: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    s.flush().unwrap();
    // the head, then exactly Content-Length bytes (none for HEAD)
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    let text = String::from_utf8(head).unwrap();
    let mut lines = text.split("\r\n");
    let status: u16 = lines.next().unwrap()[9..12].parse().unwrap();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(": ")).map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())).collect();
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0u8; if method == "HEAD" { 0 } else { len }];
    s.read_exact(&mut body).unwrap();
    (status, headers, body)
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[test]
fn a_phone_fetches_and_sends_files_in_pieces() {
    let slot: Arc<std::sync::OnceLock<Arc<crate::Transfers>>> = Arc::default();
    let dir = std::env::temp_dir().join(format!("lightair-transfer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("take")).unwrap();
    // a file of 3 MB and a bit, its bytes counting up
    let data: Vec<u8> = (0..3_000_123u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("take/screen.mp4"), &data).unwrap();
    let (s2, d2) = (slot.clone(), dir.clone());
    let handler: Handler = Arc::new(move |c: Call| {
        let t = s2.get().unwrap();
        let device = c.device.clone().ok_or_else(|| ApiError::failed("no device"))?;
        match c.method.as_str() {
            "takes.fetch" => {
                let tr = t
                    .offer_download(
                        &device,
                        vec![("Take 1.lightair-take/screen.mp4".into(), d2.join("take/screen.mp4"))],
                    )
                    .unwrap();
                Ok(tr.to_json())
            }
            "transfer.upload" => {
                let files =
                    c.params["files"].as_array().unwrap().iter().filter_map(crate::FileSpec::from_json).collect();
                Ok(t.offer_upload(&device, files, d2.join("in"), json!({"kind": "export"})).unwrap().to_json())
            }
            _ => Err(ApiError::new(crate::ErrorKind::Unknown, "no such method")),
        }
    });
    let server = Server::start(Config { port: 0, lan: false, password: None }, handler).unwrap();
    slot.set(server.transfers()).ok();
    let identity = Identity::generate("7f3c").unwrap();
    let pin = identity.fingerprint();
    let desktop = DesktopInfo { id: "7f3c".into(), name: "Studio Mac".into(), version: "0.4.0".into() };
    let key = SigningKey::from_bytes(&[5; 32]);
    let device = crate::Device {
        id: "ipad-1".into(),
        name: "Sam's iPad".into(),
        model: String::new(),
        platform: "iOS".into(),
        app_version: String::new(),
        public_key: key.verifying_key().to_bytes(),
        scope: Scope::Control,
        paired_at: 0,
        last_seen: None,
        source: None,
    };
    let pairing = Pairing::new(vec![device], |_| false, |_| {});
    let config = SecureConfig { port: 0, identity, desktop, features: vec![] };
    let secure = server.start_secure(config, pairing.clone()).unwrap();
    let port = secure.addr().port();
    let (_, nonce) = connect(port, pin, &[]).unwrap_err();
    let mut ws =
        connect(port, pin, &[("Authorization", sign_header("ipad-1", &nonce, now(), |m| key.sign(m).to_bytes()))])
            .unwrap();

    // the take: offered with its SHA-256, the device's id reaching the app
    let r = call(&mut ws, 1, "takes.fetch", json!({"take": "t1"}));
    let token = r["result"]["token"].as_str().unwrap().to_owned();
    let m = &r["result"]["manifest"][0];
    assert_eq!(m["size"], data.len() as u64);
    assert_eq!(m["sha256"], Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect::<String>());
    let path = format!("/transfer/{token}/Take%201.lightair-take/screen.mp4");

    // in 1 MB pieces on one connection, resumed partway on another
    let mut got = Vec::new();
    let mut s = tls(port, pin);
    for first in (0..2_000_000).step_by(1_000_000) {
        let (st, h, body) =
            http_req(&mut s, "GET", &path, &[("Range", format!("bytes={first}-{}", first + 999_999))], &[]);
        assert_eq!(st, 206);
        assert_eq!(header(&h, "accept-ranges"), Some("bytes"));
        assert_eq!(
            header(&h, "content-range"),
            Some(format!("bytes {first}-{}/{}", first + 999_999, data.len()).as_str())
        );
        got.extend(body);
    }
    drop(s);
    let mut s = tls(port, pin);
    let (st, _, body) = http_req(&mut s, "GET", &path, &[("Range", format!("bytes={}-", got.len()))], &[]);
    assert_eq!(st, 206);
    got.extend(body);
    assert_eq!(got, data);
    let (st, h, _) = http_req(&mut s, "HEAD", &path, &[], &[]);
    assert_eq!((st, header(&h, "content-length")), (200, Some("3000123")));
    drop(s);

    // a wrong token, and a name that isn't in the transfer
    let mut s = tls(port, pin);
    assert_eq!(http_req(&mut s, "GET", "/transfer/nope/x.mp4", &[], &[]).0, 404);
    let mut s = tls(port, pin);
    assert_eq!(http_req(&mut s, "GET", &format!("/transfer/{token}/other.mp4"), &[], &[]).0, 404);

    // an upload in pieces: the wrong offset refused, a resume by HEAD, the SHA checked
    let export: Vec<u8> = (0..2_500_000u32).map(|i| (i % 241) as u8).collect();
    let sha: String = Sha256::digest(&export).iter().map(|b| format!("{b:02x}")).collect();
    let r = call(
        &mut ws,
        2,
        "transfer.upload",
        json!({"files": [{"name": "Export.mp4", "size": export.len(), "sha256": sha}]}),
    );
    let up = r["result"]["token"].as_str().unwrap().to_owned();
    let path = format!("/transfer/{up}/Export.mp4");
    let total = export.len();
    let piece =
        |a: usize, b: usize| (vec![("Content-Range", format!("bytes {a}-{}/{total}", b - 1))], export[a..b].to_vec());
    let mut s = tls(port, pin);
    let (h, b) = piece(0, 1_000_000);
    assert_eq!(http_req(&mut s, "PUT", &path, &h, &b).0, 202);
    let (h, b) = piece(2_000_000, total);
    let (st, hh, _) = http_req(&mut s, "PUT", &path, &h, &b);
    assert_eq!((st, header(&hh, "lightair-received")), (416, Some("1000000")));
    drop(s);
    let mut s = tls(port, pin);
    let (_, hh, _) = http_req(&mut s, "HEAD", &path, &[], &[]);
    assert_eq!(header(&hh, "lightair-received"), Some("1000000"));
    let (h, b) = piece(1_000_000, 2_000_000);
    assert_eq!(http_req(&mut s, "PUT", &path, &h, &b).0, 202);
    assert!(server.transfers().take_finished().is_empty());
    let (h, b) = piece(2_000_000, total);
    assert_eq!(http_req(&mut s, "PUT", &path, &h, &b).0, 201);
    assert_eq!(std::fs::read(dir.join("in/Export.mp4")).unwrap(), export);
    let done = server.transfers().take_finished();
    assert_eq!(done.len(), 1);
    assert_eq!((done[0].device.as_str(), done[0].meta["kind"].as_str()), ("ipad-1", Some("export")));

    // a file that doesn't match its SHA-256 is dropped
    let r = call(
        &mut ws,
        3,
        "transfer.upload",
        json!({"files": [{"name": "Bad.mp4", "size": 4, "sha256": "0".repeat(64)}]}),
    );
    let bad = r["result"]["token"].as_str().unwrap().to_owned();
    let mut s = tls(port, pin);
    assert_eq!(http_req(&mut s, "PUT", &format!("/transfer/{bad}/Bad.mp4"), &[], b"abcd").0, 422);
    assert!(!dir.join("in/Bad.mp4").exists() && !dir.join("in/Bad.mp4.partial").exists());

    // an empty file (an empty stem): an empty body, no Content-Range
    let empty: String = Sha256::digest(b"").iter().map(|b| format!("{b:02x}")).collect();
    let r =
        call(&mut ws, 4, "transfer.upload", json!({"files": [{"name": "Silence.wav", "size": 0, "sha256": empty}]}));
    let none = r["result"]["token"].as_str().unwrap().to_owned();
    let mut s = tls(port, pin);
    assert_eq!(http_req(&mut s, "PUT", &format!("/transfer/{none}/Silence.wav"), &[], b"").0, 201);
    assert_eq!(std::fs::metadata(dir.join("in/Silence.wav")).unwrap().len(), 0);
    // and a download with an open range, and none (the whole file)
    let mut s = tls(port, pin);
    let path0 = format!("/transfer/{token}/Take%201.lightair-take/screen.mp4");
    let (st, _, body) = http_req(&mut s, "GET", &path0, &[("Range", "bytes=3000000-".into())], &[]);
    assert_eq!((st, body.as_slice()), (206, &data[3_000_000..]));
    let (st, _, body) = http_req(&mut s, "GET", &path0, &[], &[]);
    assert_eq!((st, body.len()), (200, data.len()));

    // the device removed: its tokens stop working
    pairing.remove("ipad-1");
    let mut s = tls(port, pin);
    let path = format!("/transfer/{token}/Take%201.lightair-take/screen.mp4");
    assert_eq!(http_req(&mut s, "GET", &path, &[], &[]).0, 403);
    let _ = std::fs::remove_dir_all(&dir);
}
