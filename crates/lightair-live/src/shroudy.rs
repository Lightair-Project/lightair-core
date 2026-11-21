//! shroudy: the owner's OAuth broker (a Cloudflare Worker, `shroudy/README.md`), which holds the
//! client secret of a platform that wants one (Kick), so no copy of Lightair carries it.
//!
//! The app still makes the PKCE pair and the `state`, and checks the state when the browser
//! comes back; shroudy only adds the client id and secret. Its flow:
//!
//! 1. The browser opens `<client>/authorize?state=<return>.<nonce>&code_challenge=…`, which
//!    sends it on to the platform's sign-in with shroudy's callback as the redirect.
//! 2. The platform comes back to shroudy's callback, which sends the browser on to the return
//!    address the state names (`desktop` or `phone`, configured in shroudy's `src/apps.ts`).
//! 3. The app exchanges the code at `<client>/token` with its verifier, and refreshes at
//!    `<client>/refresh`: no client id, no secret.

use serde_json::Value;

use crate::oauth::{encode, post_form};

/// Lightair's Kick client at the owner's shroudy.
pub const KICK: &str = "https://shroudy.bregas-sat1.workers.dev/v1/lightair/oauth/kick";

/// The return addresses shroudy's Kick client knows (its `src/apps.ts`), by name: where the
/// browser is sent after signing in.
pub const KICK_RETURNS: &[(&str, &str)] =
    &[("desktop", "http://127.0.0.1:47801/callback"), ("phone", "app.lightair.live:/oauth/kick")];

/// One OAuth client at shroudy (`…/v1/<app>/oauth/<client>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broker {
    base: String,
}

impl Broker {
    /// The client at `base` (`https://…/v1/<app>/oauth/<client>`).
    pub fn at(base: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_string() }
    }

    /// Lightair's Kick client: [`KICK`], or `LIGHTAIR_SHROUDY_KICK` from the environment (a
    /// shroudy under `wrangler dev`).
    pub fn kick() -> Self {
        let given = std::env::var("LIGHTAIR_SHROUDY_KICK").ok().filter(|v| !v.trim().is_empty());
        Self::at(given.as_deref().unwrap_or(KICK))
    }

    /// The sign-in page for the browser: shroudy sends it on to the platform's, with its own
    /// callback and the app's client id.
    pub fn authorize_url(&self, state: &str, challenge: &str) -> String {
        format!(
            "{}/authorize?state={}&code_challenge={}&code_challenge_method=S256",
            self.base,
            encode(state),
            encode(challenge)
        )
    }

    /// Exchanges the code the browser brought back, with the PKCE verifier: the platform's token
    /// answer (with an `access_token`), or why not.
    pub fn exchange(&self, code: &str, verifier: &str) -> Result<Value, String> {
        let fields = [("grant_type", "authorization_code"), ("code", code), ("code_verifier", verifier)];
        token(post_form(&format!("{}/token", self.base), &fields))
    }

    /// A fresh token for `refresh_token`: the platform's answer (with an `access_token`), or why
    /// not.
    pub fn refresh(&self, refresh_token: &str) -> Result<Value, String> {
        token(post_form(&format!("{}/refresh", self.base), &[("refresh_token", refresh_token)]))
    }
}

/// shroudy's answer: the platform's token JSON, or its error in shroudy's (or the platform's)
/// own words.
fn token(answer: Result<Value, String>) -> Result<Value, String> {
    let v = answer.map_err(|e| format!("the sign-in service didn't answer properly: {e}"))?;
    if v["access_token"].is_string() {
        return Ok(v);
    }
    Err(format!(
        "no token from the sign-in service: {}",
        v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no reason given")
    ))
}

/// A one-request fake shroudy for tests: answers with `status` and `body`, and hands back the
/// request it got.
#[cfg(test)]
pub(crate) mod fake {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::mpsc;

    #[derive(Debug)]
    pub struct Request {
        pub method: String,
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Request {
        /// The form fields of the body, decoded.
        pub fn form(&self) -> Vec<(String, String)> {
            self.body
                .split('&')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    (crate::oauth::decode(k), crate::oauth::decode(v))
                })
                .collect()
        }

        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    /// Starts the fake: its base URL (`http://127.0.0.1:<port>/v1/lightair/oauth/kick`) and
    /// where its one request arrives.
    pub fn shroudy(status: u16, body: &'static str) -> (String, mpsc::Receiver<Request>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let length = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            let mut got = vec![0u8; length];
            reader.read_exact(&mut got).unwrap();
            let kind = if body.starts_with('{') { "application/json" } else { "text/html" };
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tx.send(Request { method, path, headers, body: String::from_utf8_lossy(&got).into_owned() });
        });
        (format!("http://127.0.0.1:{port}/v1/lightair/oauth/kick"), rx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Broker, fake};

    #[test]
    fn a_refresh_sends_only_the_refresh_token_to_shroudy() {
        let (base, got) = fake::shroudy(200, r#"{"access_token":"new","refresh_token":"r2","expires_in":3600}"#);
        let token = Broker::at(&base).refresh("r1").unwrap();
        assert_eq!(token["access_token"], "new");
        let req = got.recv().unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/v1/lightair/oauth/kick/refresh"));
        assert_eq!(req.form(), [("refresh_token".to_string(), "r1".to_string())], "nothing else, above all no secret");
    }

    #[test]
    fn shroudys_refusal_comes_back_in_its_own_words() {
        let (base, _got) =
            fake::shroudy(400, r#"{"error":"invalid_grant","error_description":"The code has expired."}"#);
        let err = Broker::at(&base).exchange("c0de", "v".repeat(43).as_str()).unwrap_err();
        assert!(err.contains("The code has expired."), "{err}");
    }

    #[test]
    fn an_answer_without_a_token_is_an_error() {
        let (base, _got) = fake::shroudy(502, "<html>Bad gateway</html>");
        assert!(Broker::at(&base).refresh("r1").is_err());
        let (base, _got) = fake::shroudy(200, r#"{"token_type":"Bearer"}"#);
        assert!(Broker::at(&base).refresh("r1").is_err(), "a 200 without an access_token is no token");
    }
}
