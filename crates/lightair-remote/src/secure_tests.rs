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
    let mut req =
        tungstenite::client::IntoClientRequest::into_client_request(format!("ws://127.0.0.1:{port}/rpc")).unwrap();
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
