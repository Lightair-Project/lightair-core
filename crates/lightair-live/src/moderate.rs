//! Chat moderation from Lightair: delete a message, time someone out, ban them. Through the
//! signed-in session (Outputs › a destination › Sending › Sign in), no app approval:
//!
//! - Twitch: typed into its own chat box in a signed-in hidden page (`/timeout`, `/ban`,
//!   `/delete`), as a moderator does; Twitch's site makes the requests. (Its API, Helix, doesn't
//!   take the site's session: it answers 404.)
//! - Kick: its site's own requests, made from a signed-in hidden page.
//!
//! YouTube's (its chat's item menus) isn't built yet.

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::net;

const TWITCH_CLIENT: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

/// What can be done on `platform` from Lightair.
pub fn can(platform: &str) -> bool {
    matches!(platform, "twitch" | "kick") || crate::official::signed_in(platform)
}

pub enum Action<'a> {
    Delete {
        message_id: &'a str,
    },
    /// `seconds`: 1 to 1,209,600 (two weeks) on Twitch.
    Timeout {
        user_id: &'a str,
        login: &'a str,
        seconds: u64,
        reason: &'a str,
    },
    Ban {
        user_id: &'a str,
        login: &'a str,
        reason: &'a str,
    },
}

/// Does `action` in `channel`'s chat on `platform`, as the signed-in account (a moderator
/// there, or its owner).
pub fn act(platform: &str, channel: &str, action: Action) -> Result<(), String> {
    if crate::official::signed_in(platform) {
        return crate::official::moderate(platform, channel, action);
    }
    match platform {
        "twitch" => twitch(channel, action),
        "kick" => kick(channel, action),
        other => Err(format!("moderating {other} chat from Lightair isn't supported yet")),
    }
}

fn twitch_token() -> Result<String, String> {
    crate::pages::cookie("https://www.twitch.tv", "auth-token")
        .ok_or_else(|| "sign in to Twitch first (Outputs › the destination › Sending)".into())
}

/// The signed-in Twitch account: (id, login). Kept until the token changes.
pub fn twitch_me() -> Result<(String, String), String> {
    /// (the token it was for, (id, login))
    type Me = Option<(String, (String, String))>;
    static ME: OnceLock<Mutex<Me>> = OnceLock::new();
    let token = twitch_token()?;
    let cache = ME.get_or_init(|| Mutex::new(None));
    if let Some((t, me)) = cache.lock().as_ref()
        && *t == token
    {
        return Ok(me.clone());
    }
    let agent = net::agent();
    let mut res = agent
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", TWITCH_CLIENT)
        .header("Authorization", &format!("OAuth {token}"))
        .send(json!({"query": "query{currentUser{id login}}"}).to_string())
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let v: Value = serde_json::from_str(&res.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
    let u = &v["data"]["currentUser"];
    let me = (
        u["id"].as_str().ok_or_else(|| format!("Twitch didn't say who is signed in ({status})"))?.to_string(),
        u["login"].as_str().unwrap_or("").to_string(),
    );
    *cache.lock() = Some((token, me.clone()));
    Ok(me)
}

fn twitch(channel: &str, action: Action) -> Result<(), String> {
    twitch_token()?;
    let login = crate::twitch::login(channel);
    // logins and ids only: nothing else can ride along into the command
    let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let one_line = |s: &str| s.replace(['\n', '\r'], " ");
    let command = match action {
        Action::Delete { message_id } if safe(message_id) => format!("/delete {message_id}"),
        Action::Timeout { login: who, seconds, reason, .. } if safe(who) => {
            format!("/timeout {who} {} {}", seconds.clamp(1, 1_209_600), one_line(reason))
        }
        Action::Ban { login: who, reason, .. } if safe(who) => format!("/ban {who} {}", one_line(reason)),
        _ => return Err("that isn't a Twitch user or message".into()),
    };
    crate::send::page_send(
        "twitch",
        &format!("https://www.twitch.tv/popout/{login}/chat?popout="),
        &crate::rules::text("twitch.send_input", "[data-a-target=\"chat-input\"]"),
        &crate::rules::text("twitch.send_button", "[data-a-target=\"chat-send-button\"]"),
        command.trim(),
    )
}

/// Kick: its site's requests, from a signed-in page on the channel (the session's token and
/// Kick's anti-forgery cookie are the page's own).
fn kick(channel: &str, action: Action) -> Result<(), String> {
    if !crate::accounts::signed_in("kick") {
        return Err("sign in to Kick first (Outputs › the destination › Sending)".into());
    }
    let slug = crate::kick::slug(channel);
    let (method, path, body) = match action {
        Action::Delete { message_id } => {
            ("DELETE", format!("/api/v2/channels/{slug}/messages/{message_id}"), Value::Null)
        }
        Action::Timeout { login, seconds, reason, .. } => (
            "POST",
            format!("/api/v2/channels/{slug}/bans"),
            json!({"banned_username": login, "duration": (seconds / 60).max(1), "permanent": false, "reason": reason}),
        ),
        Action::Ban { login, reason, .. } => (
            "POST",
            format!("/api/v2/channels/{slug}/bans"),
            json!({"banned_username": login, "permanent": true, "reason": reason}),
        ),
    };
    let script = format!(
        r#"(async function () {{
  function cookie(n) {{ var m = document.cookie.match(new RegExp('(?:^|; )' + n + '=([^;]*)')); return m ? decodeURIComponent(m[1]) : ''; }}
  try {{
    var headers = {{ 'Accept': 'application/json', 'Content-Type': 'application/json', 'X-XSRF-TOKEN': cookie('XSRF-TOKEN') }};
    var session = cookie('session_token');
    if (session) headers['Authorization'] = 'Bearer ' + session;
    var r = await fetch({path:?}, {{ method: {method:?}, credentials: 'include', headers: headers, body: {body} }});
    var text = await r.text();
    lightair(r.ok ? 'ok' : 'no:' + r.status + ' ' + text.slice(0, 200));
  }} catch (e) {{ lightair('no:' + e); }}
}})();"#,
        body = if body.is_null() {
            "undefined".to_string()
        } else {
            serde_json::to_string(&body.to_string()).unwrap_or_default()
        },
    );
    let answer = crate::pages::run(
        &format!("https://kick.com/{slug}"),
        &script,
        Duration::from_secs(30),
        &AtomicBool::new(false),
    )?;
    match answer.strip_prefix("no:") {
        None => Ok(()),
        Some(why) => Err(format!("Kick said no: {why}")),
    }
}
