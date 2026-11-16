//! Official sign-in (OAuth) where a platform offers it, as OBS does: Lightair's own registered
//! app on each platform, the person signing in in their own browser. Where signed in this way,
//! Lightair uses the platform's official API (official.rs); elsewhere the signed-in pages.
//!
//! - **YouTube** (Google): authorization code with PKCE to a loopback address
//!   (`http://127.0.0.1:<port>`), Google's flow for desktop apps. Google's "Desktop app"
//!   clients come with a secret that isn't secret (it ships in every copy); it's sent too.
//! - **Twitch**: the device code flow (Twitch's flow for public clients): the browser opens
//!   twitch.tv/activate with the code filled in; no secret in the app.
//! - **Kick**: authorization code with PKCE to a fixed loopback address (Kick wants the exact
//!   redirect registered: `http://127.0.0.1:47801/callback` unless set otherwise).
//!
//! Each app's ids come from the build (`LIGHTAIR_YOUTUBE_CLIENT_ID`, … set when building) or
//! the environment at run time; docs/official-apis.md says how to register them. Tokens are kept
//! by the app (the Keychain): [`set_store`].

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use crate::net;

/// The platforms with an official sign-in Lightair supports.
pub const PROVIDERS: &[&str] = &["youtube", "twitch", "kick"];

struct App {
    client_id: String,
    secret: Option<String>,
}

fn app(provider: &str) -> Option<App> {
    let (id, secret) = match provider {
        "youtube" => (option_env!("LIGHTAIR_YOUTUBE_CLIENT_ID"), option_env!("LIGHTAIR_YOUTUBE_CLIENT_SECRET")),
        "twitch" => (option_env!("LIGHTAIR_TWITCH_CLIENT_ID"), None),
        "kick" => (option_env!("LIGHTAIR_KICK_CLIENT_ID"), option_env!("LIGHTAIR_KICK_CLIENT_SECRET")),
        _ => return None,
    };
    let var = |k: &str| std::env::var(format!("LIGHTAIR_{}_{k}", provider.to_uppercase())).ok().filter(|v| !v.is_empty());
    let client_id = var("CLIENT_ID").or(id.map(String::from)).filter(|v| !v.is_empty())?;
    Some(App { client_id, secret: var("CLIENT_SECRET").or(secret.map(String::from)).filter(|v| !v.is_empty()) })
}

/// Whether this copy of Lightair has `provider`'s app (an id to sign in with).
pub fn available(provider: &str) -> bool {
    app(provider).is_some()
}

type Get = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
type Set = Box<dyn Fn(&str, &str) + Send + Sync>;
static STORE: OnceLock<(Get, Set)> = OnceLock::new();

/// Where tokens are kept (the app's Keychain), by name ("oauth-youtube"; empty: forget it).
pub fn set_store(get: Get, set: Set) {
    let _ = STORE.set((get, set));
}

fn load(provider: &str) -> Option<Value> {
    let (get, _) = STORE.get()?;
    serde_json::from_str(&get(&format!("oauth-{provider}"))?).ok()
}

fn save(provider: &str, v: &Value) {
    if let Some((_, set)) = STORE.get() {
        set(&format!("oauth-{provider}"), &v.to_string());
    }
}

/// Signs out (forgets the token).
pub fn forget(provider: &str) {
    if let Some((_, set)) = STORE.get() {
        set(&format!("oauth-{provider}"), "");
    }
}

/// Who is signed in officially on `provider`: (id, name).
pub fn account(provider: &str) -> Option<(String, String)> {
    let v = load(provider)?;
    Some((v["id"].as_str()?.to_string(), v["name"].as_str().unwrap_or("").to_string()))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    let _ = getrandom::fill(&mut b);
    b
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn form(fields: &[(&str, &str)]) -> String {
    fields.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect::<Vec<_>>().join("&")
}

pub(crate) fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    let mut res = net::agent()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(form(fields))
        .map_err(|e| e.to_string())?;
    let text = res.body_mut().read_to_string().unwrap_or_default();
    serde_json::from_str(&text).map_err(|_| format!("{url}: {}", text.chars().take(200).collect::<String>()))
}

/// Opens `url` in the person's own browser (Google turns away embedded ones).
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

/// Waits for the browser to come back to `listener` with `?code=…&state=…`: the code.
fn wait_for_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err("the sign-in wasn't finished in time".into());
        }
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = req.split_whitespace().nth(1).unwrap_or("");
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let param = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(decode);
        let (page, result) = match (param("code"), param("state"), param("error")) {
            (Some(code), Some(s), _) if s == state => ("You're signed in. You can close this tab and go back to Lightair.", Ok(code)),
            (_, _, Some(e)) => ("The sign-in was cancelled. You can close this tab.", Err(format!("the sign-in was cancelled ({e})"))),
            _ => ("Nothing to do here.", Err(String::new())),
        };
        let body = format!("<!doctype html><meta charset=utf-8><title>Lightair</title><body style=\"font:16px -apple-system,sans-serif;padding:40px\">{page}</body>");
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        match result {
            Err(e) if e.is_empty() => continue,
            r => return r,
        }
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00"), 16).unwrap_or(b'?'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What signing in asks the person to do now (shown while it waits): Twitch's code to check.
pub type Prompt = Box<dyn Fn(&str) + Send>;

/// Signs in to `provider` (opens the browser, waits up to five minutes): the account's name.
pub fn sign_in(provider: &str, prompt: Prompt) -> Result<String, String> {
    let app = app(provider).ok_or_else(|| format!("this copy of Lightair has no {provider} app to sign in with (docs/official-apis.md)"))?;
    let token = match provider {
        "twitch" => twitch_device(&app, &prompt)?,
        "youtube" | "kick" => code_flow(provider, &app, &prompt)?,
        other => return Err(format!("no official sign-in for {other}")),
    };
    let mut record = token;
    record["expires_at"] = json!(now() + record["expires_in"].as_i64().unwrap_or(3600));
    let (id, name) = whoami(provider, record["access_token"].as_str().unwrap_or(""))?;
    record["id"] = json!(id);
    record["name"] = json!(name);
    save(provider, &record);
    Ok(name)
}

fn code_flow(provider: &str, app: &App, prompt: &Prompt) -> Result<Value, String> {
    let port: u16 = if provider == "kick" { std::env::var("LIGHTAIR_KICK_REDIRECT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(47801) } else { 0 };
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("couldn't wait for the browser: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = if provider == "kick" { format!("http://127.0.0.1:{port}/callback") } else { format!("http://127.0.0.1:{port}") };
    let verifier = b64url(&random(48));
    let challenge = {
        use sha2::Digest;
        b64url(&sha2::Sha256::digest(verifier.as_bytes()))
    };
    let state = b64url(&random(16));
    let (auth, scope) = match provider {
        "youtube" => ("https://accounts.google.com/o/oauth2/v2/auth", "https://www.googleapis.com/auth/youtube"),
        _ => ("https://id.kick.com/oauth/authorize", "user:read channel:read channel:write chat:write moderation:ban"),
    };
    let mut url = format!(
        "{auth}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        encode(&app.client_id),
        encode(&redirect),
        encode(scope)
    );
    if provider == "youtube" {
        url.push_str("&access_type=offline&prompt=consent");
    }
    prompt("Finish signing in in your browser…");
    open_browser(&url);
    let code = wait_for_code(&listener, &state, Duration::from_secs(300))?;
    let token_url = if provider == "youtube" { "https://oauth2.googleapis.com/token" } else { "https://id.kick.com/oauth/token" };
    let mut fields = vec![
        ("grant_type", "authorization_code"),
        ("client_id", app.client_id.as_str()),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect.as_str()),
    ];
    if let Some(s) = &app.secret {
        fields.push(("client_secret", s.as_str()));
    }
    let v = post_form(token_url, &fields)?;
    if v["access_token"].as_str().is_none() {
        return Err(format!("{provider} didn't give a token: {}", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("")));
    }
    Ok(v)
}

fn twitch_device(app: &App, prompt: &Prompt) -> Result<Value, String> {
    let scopes = "channel:manage:broadcast user:write:chat user:read:chat moderator:manage:banned_users moderator:manage:chat_messages";
    let v = post_form("https://id.twitch.tv/oauth2/device", &[("client_id", &app.client_id), ("scopes", scopes)])?;
    let (Some(device), Some(code), Some(uri)) = (v["device_code"].as_str(), v["user_code"].as_str(), v["verification_uri"].as_str()) else {
        return Err(format!("Twitch didn't start the sign-in: {}", v["message"].as_str().unwrap_or("")));
    };
    prompt(&format!("Check that Twitch shows the code {code}, then Authorize"));
    open_browser(uri);
    let every = Duration::from_secs(v["interval"].as_u64().unwrap_or(5));
    let until = Instant::now() + Duration::from_secs(v["expires_in"].as_u64().unwrap_or(1800).min(1800));
    while Instant::now() < until {
        std::thread::sleep(every);
        let t = post_form(
            "https://id.twitch.tv/oauth2/token",
            &[("client_id", &app.client_id), ("scopes", scopes), ("device_code", device), ("grant_type", "urn:ietf:params:oauth:grant-type:device_code")],
        )?;
        if t["access_token"].is_string() {
            return Ok(t);
        }
        match t["message"].as_str().unwrap_or("") {
            "authorization_pending" => continue,
            m => return Err(format!("Twitch: {m}")),
        }
    }
    Err("the sign-in wasn't finished in time".into())
}

/// The signed-in account: (id, name).
fn whoami(provider: &str, token: &str) -> Result<(String, String), String> {
    let agent = net::agent();
    let auth = format!("Bearer {token}");
    let get = |url: &str, extra: Option<(&str, &str)>| -> Result<Value, String> {
        let mut req = agent.get(url).header("Authorization", &auth);
        if let Some((k, v)) = extra {
            req = req.header(k, v);
        }
        let mut res = req.call().map_err(|e| e.to_string())?;
        serde_json::from_str(&res.body_mut().read_to_string().unwrap_or_default()).map_err(|e| e.to_string())
    };
    match provider {
        "youtube" => {
            let v = get("https://www.googleapis.com/youtube/v3/channels?part=snippet&mine=true", None)?;
            let c = &v["items"][0];
            Ok((c["id"].as_str().ok_or("that Google account has no YouTube channel")?.to_string(), c["snippet"]["title"].as_str().unwrap_or("").to_string()))
        }
        "twitch" => {
            let client = app("twitch").map(|a| a.client_id).unwrap_or_default();
            let v = get("https://api.twitch.tv/helix/users", Some(("Client-Id", &client)))?;
            let u = &v["data"][0];
            Ok((u["id"].as_str().ok_or("Twitch didn't say who signed in")?.to_string(), u["login"].as_str().unwrap_or("").to_string()))
        }
        _ => {
            let v = get("https://api.kick.com/public/v1/users", None)?;
            let u = &v["data"][0];
            let id = u["user_id"].as_u64().map(|n| n.to_string()).or_else(|| u["user_id"].as_str().map(String::from)).ok_or("Kick didn't say who signed in")?;
            Ok((id, u["name"].as_str().unwrap_or("").to_string()))
        }
    }
}

/// A token for `provider`'s API, refreshed when it's about to run out; None when not signed in
/// officially.
pub fn token(provider: &str) -> Option<String> {
    let mut v = load(provider)?;
    if v["expires_at"].as_i64().unwrap_or(0) - 60 > now() {
        return v["access_token"].as_str().map(String::from);
    }
    let app = app(provider)?;
    let refresh = v["refresh_token"].as_str()?.to_string();
    let url = match provider {
        "youtube" => "https://oauth2.googleapis.com/token",
        "twitch" => "https://id.twitch.tv/oauth2/token",
        _ => "https://id.kick.com/oauth/token",
    };
    let mut fields = vec![("grant_type", "refresh_token"), ("client_id", app.client_id.as_str()), ("refresh_token", refresh.as_str())];
    if let Some(s) = &app.secret {
        fields.push(("client_secret", s.as_str()));
    }
    let t = post_form(url, &fields).ok()?;
    let access = t["access_token"].as_str()?.to_string();
    v["access_token"] = json!(access);
    if let Some(r) = t["refresh_token"].as_str() {
        v["refresh_token"] = json!(r);
    }
    v["expires_at"] = json!(now() + t["expires_in"].as_i64().unwrap_or(3600));
    save(provider, &v);
    Some(access)
}

/// The app's id on `provider` (Twitch's API wants it with every call).
pub fn client_id(provider: &str) -> Option<String> {
    app(provider).map(|a| a.client_id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_values_round_trip() {
        assert_eq!(super::encode("a b&c/é"), "a%20b%26c%2F%C3%A9");
        assert_eq!(super::decode("a%20b%26c%2F%C3%A9+x"), "a b&c/é x");
    }

    /// The browser coming back: the code is taken only with the right state.
    #[test]
    fn the_browser_brings_the_code_back() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for path in ["/favicon.ico", "/?code=wrong&state=nope", "/?code=4%2Fabc&state=s1"] {
                let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                let mut out = String::new();
                let _ = s.read_to_string(&mut out);
            }
        });
        assert_eq!(super::wait_for_code(&listener, "s1", std::time::Duration::from_secs(5)).unwrap(), "4/abc");
    }
}
