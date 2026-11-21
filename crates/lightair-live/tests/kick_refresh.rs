//! Staying signed in to Kick through shroudy (the owner's OAuth broker, which holds Kick's
//! secret): an expired token is refreshed there with nothing but the refresh token, the new one
//! is kept, and shroudy being down doesn't sign the person out.
//!
//! Its own test program: it points `LIGHTAIR_SHROUDY_KICK` at a stand-in shroudy and installs the
//! process's token store, both process-wide.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

/// A stand-in shroudy answering each request in turn with `answers` (status, body), and handing
/// back each request's path and body.
fn shroudy(answers: Vec<(u16, &'static str)>) -> (String, mpsc::Receiver<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, body) in answers {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut length = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut got = vec![0u8; length];
            reader.read_exact(&mut got).unwrap();
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tx.send((path, String::from_utf8_lossy(&got).into_owned()));
        }
    });
    (format!("http://127.0.0.1:{port}/v1/lightair/oauth/kick"), rx)
}

#[test]
fn a_kick_sign_in_stays_through_shroudy_refreshing_it() {
    // shroudy down once, then a fresh token with a new refresh token
    let (base, asked) = shroudy(vec![
        (502, r#"{"error":"upstream"}"#),
        (200, r#"{"access_token":"new","refresh_token":"r2","expires_in":3600}"#),
    ]);
    // SAFETY: the only test in this program, and nothing else here reads the environment
    unsafe { std::env::set_var("LIGHTAIR_SHROUDY_KICK", &base) };
    let kept: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let expired = json!({"access_token": "old", "refresh_token": "r1", "expires_at": 0, "id": "7", "name": "Mochi"});
    kept.lock().unwrap().insert("oauth-kick".into(), expired.to_string());
    let (get, set) = (kept.clone(), kept.clone());
    lightair_live::oauth::set_store(
        Box::new(move |k| get.lock().unwrap().get(k).cloned()),
        Box::new(move |k, v| {
            set.lock().unwrap().insert(k.into(), v.into());
        }),
    );
    let stored = || -> Value { serde_json::from_str(&kept.lock().unwrap()["oauth-kick"]).unwrap() };

    // shroudy down: no token now, but still signed in (the refresh token is kept to try again)
    assert_eq!(lightair_live::oauth::token("kick"), None);
    assert_eq!(stored()["refresh_token"], "r1", "not signed out by an outage");
    assert_eq!(lightair_live::oauth::account("kick"), Some(("7".into(), "Mochi".into())));
    asked.recv().unwrap();

    // shroudy back: refreshed with the refresh token alone (no client id or secret in the app)
    assert_eq!(lightair_live::oauth::token("kick").as_deref(), Some("new"));
    let (path, body) = asked.recv().unwrap();
    assert_eq!(path, "/v1/lightair/oauth/kick/refresh");
    assert_eq!(body, "refresh_token=r1");
    // kept: the new token and refresh token, good for an hour, without asking again
    let now = stored();
    assert_eq!((now["access_token"].as_str(), now["refresh_token"].as_str()), (Some("new"), Some("r2")));
    assert_eq!(lightair_live::oauth::token("kick").as_deref(), Some("new"));
    assert!(asked.try_recv().is_err(), "a token still good isn't refreshed");
}
