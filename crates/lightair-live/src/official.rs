//! The platforms' official APIs, used where the person signed in officially (oauth.rs): stream
//! title, description and category; sending chat; moderation; and, for YouTube, the signed-in
//! channel's own broadcast (Private and Unlisted ones too).
//!
//! - YouTube Data API v3 (`liveBroadcasts`, `videos`, `liveChat/messages`, `liveChat/bans`).
//! - Twitch Helix (`channels`, `search/categories`, `chat/messages`, `moderation/*`).
//! - Kick's public API (`api.kick.com/public/v1`: `channels`, `categories`, `chat`,
//!   `moderation/bans`).
//! - X's Livestream API (`api.x.com/2`: `sources`, `broadcasts`, `broadcasts/:id/chat`; needs
//!   X's approval for the app). X takes a title only when a broadcast starts: Lightair goes live
//!   on X itself once X sees the video, with the title set for it, and ends it when the stream
//!   stops. X sends chat to a public webhook, so reading it waits for the relay.

use serde_json::{Value, json};

use crate::net;
use crate::oauth;
use crate::stream_info::StreamInfo;

fn provider(platform: &str) -> &str {
    if platform == "youtube-shorts" { "youtube" } else { platform }
}

/// Whether `platform` is signed in officially (its API is used).
pub fn signed_in(platform: &str) -> bool {
    let p = provider(platform);
    oauth::available(p) && oauth::account(p).is_some()
}

/// A request to `url` with the official token: the answer's JSON (Null for no content).
fn call(platform: &str, method: &str, url: &str, body: Option<Value>) -> Result<Value, String> {
    let p = provider(platform);
    let token = oauth::token(p).ok_or_else(|| format!("sign in to {p} again (Settings › Connections)"))?;
    let agent = net::agent();
    let auth = format!("Bearer {token}");
    let client = oauth::client_id(p).unwrap_or_default();
    let with = |r: ureq::RequestBuilder<ureq::typestate::WithBody>| {
        let r = r.header("Authorization", &auth).header("Content-Type", "application/json").header("Accept", "application/json");
        if p == "twitch" { r.header("Client-Id", &client) } else { r }
    };
    let without = |r: ureq::RequestBuilder<ureq::typestate::WithoutBody>| {
        let r = r.header("Authorization", &auth).header("Accept", "application/json");
        if p == "twitch" { r.header("Client-Id", &client) } else { r }
    };
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut res = match method {
        "GET" => without(agent.get(url)).call(),
        "DELETE" => without(agent.delete(url)).call(),
        "PATCH" => with(agent.patch(url)).send(body),
        "PUT" => with(agent.put(url)).send(body),
        _ => with(agent.post(url)).send(body),
    }
    .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status >= 300 {
        let why = v["error"]["message"].as_str().or(v["message"].as_str()).map(String::from).unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("{p} said no ({status}): {why}"));
    }
    Ok(v)
}

// ---- YouTube ----

const YT: &str = "https://www.googleapis.com/youtube/v3";

/// The signed-in channel's broadcast: live now, else the next upcoming one.
fn yt_broadcast() -> Result<Value, String> {
    for status in ["active", "upcoming"] {
        let v = call("youtube", "GET", &format!("{YT}/liveBroadcasts?part=snippet,status&broadcastStatus={status}&broadcastType=all&maxResults=1"), None)?;
        if let Some(b) = v["items"].as_array().and_then(|a| a.first()) {
            return Ok(b.clone());
        }
    }
    Err("the YouTube channel has no live or upcoming stream".into())
}

fn yt_info() -> Result<StreamInfo, String> {
    let b = yt_broadcast()?;
    let id = b["id"].as_str().unwrap_or("");
    let v = call("youtube", "GET", &format!("{YT}/videos?part=snippet,liveStreamingDetails&id={id}"), None)?;
    let video = &v["items"][0];
    let details = &video["liveStreamingDetails"];
    Ok(StreamInfo {
        live: b["status"]["lifeCycleStatus"].as_str() == Some("live"),
        title: video["snippet"]["title"].as_str().or(b["snippet"]["title"].as_str()).unwrap_or("").to_string(),
        category: yt_category_name(video["snippet"]["categoryId"].as_str().unwrap_or("")).unwrap_or_default(),
        viewers: details["concurrentViewers"].as_str().and_then(|n| n.parse().ok()),
        started_ms: details["actualStartTime"].as_str().and_then(crate::stream_info::iso_ms),
        description: video["snippet"]["description"].as_str().map(String::from),
    })
}

fn yt_categories() -> Result<Vec<(String, String)>, String> {
    let v = call("youtube", "GET", &format!("{YT}/videoCategories?part=snippet&regionCode=US"), None)?;
    Ok(v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["snippet"]["assignable"].as_bool() != Some(false))
        .filter_map(|c| Some((c["id"].as_str()?.to_string(), c["snippet"]["title"].as_str()?.to_string())))
        .collect())
}

fn yt_category_name(id: &str) -> Option<String> {
    yt_categories().ok()?.into_iter().find(|(i, _)| i == id).map(|(_, n)| n)
}

/// Title, description and category: the video's whole snippet is written back (YouTube
/// replaces what's left out).
fn yt_edit(title: Option<&str>, description: Option<&str>, category: Option<&str>) -> Result<String, String> {
    let b = yt_broadcast()?;
    let id = b["id"].as_str().unwrap_or("").to_string();
    let v = call("youtube", "GET", &format!("{YT}/videos?part=snippet&id={id}"), None)?;
    let mut snippet = v["items"][0]["snippet"].clone();
    if snippet.is_null() {
        return Err("YouTube didn't give the stream's details".into());
    }
    if let Some(t) = title {
        snippet["title"] = json!(t);
    }
    if let Some(d) = description {
        snippet["description"] = json!(d);
    }
    let mut found = String::new();
    if let Some(c) = category {
        let all = yt_categories()?;
        let want = c.to_lowercase();
        let (cid, name) = all
            .iter()
            .find(|(_, n)| n.to_lowercase() == want)
            .or_else(|| all.iter().find(|(_, n)| n.to_lowercase().contains(&want)))
            .ok_or_else(|| format!("YouTube's categories are: {}", all.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")))?;
        snippet["categoryId"] = json!(cid);
        found = name.clone();
    }
    // read-only parts YouTube refuses back
    for k in ["thumbnails", "localized", "channelId", "channelTitle", "publishedAt", "liveBroadcastContent"] {
        if let Some(o) = snippet.as_object_mut() {
            o.remove(k);
        }
    }
    call("youtube", "PUT", &format!("{YT}/videos?part=snippet"), Some(json!({"id": id, "snippet": snippet})))?;
    Ok(if found.is_empty() { "Saved".into() } else { format!("Saved (category: {found})") })
}

fn yt_chat_id() -> Result<String, String> {
    yt_broadcast()?["snippet"]["liveChatId"].as_str().map(String::from).ok_or_else(|| "the stream has no live chat".into())
}

// ---- Twitch ----

const HELIX: &str = "https://api.twitch.tv/helix";

fn twitch_room(channel: &str) -> Result<String, String> {
    let login = crate::twitch::login(channel);
    if let Some((id, name)) = oauth::account("twitch")
        && name.eq_ignore_ascii_case(&login)
    {
        return Ok(id);
    }
    let v = call("twitch", "GET", &format!("{HELIX}/users?login={}", oauth::encode(&login)), None)?;
    v["data"][0]["id"].as_str().map(String::from).ok_or_else(|| format!("no Twitch channel {login}"))
}

fn twitch_me() -> Result<String, String> {
    oauth::account("twitch").map(|(id, _)| id).ok_or_else(|| "sign in to Twitch (Settings › Connections)".into())
}

fn twitch_edit(channel: &str, title: Option<&str>, category: Option<&str>) -> Result<String, String> {
    let room = twitch_room(channel)?;
    let mut body = json!({});
    if let Some(t) = title {
        body["title"] = json!(t);
    }
    let mut found = String::new();
    if let Some(c) = category {
        let v = call("twitch", "GET", &format!("{HELIX}/search/categories?first=1&query={}", oauth::encode(c)), None)?;
        let g = &v["data"][0];
        body["game_id"] = json!(g["id"].as_str().ok_or_else(|| format!("Twitch has no category like “{c}”"))?);
        found = g["name"].as_str().unwrap_or("").to_string();
    }
    call("twitch", "PATCH", &format!("{HELIX}/channels?broadcaster_id={room}"), Some(body))?;
    Ok(if found.is_empty() { "Saved".into() } else { format!("Saved (category: {found})") })
}

// ---- Kick ----

const KICK: &str = "https://api.kick.com/public/v1";

fn kick_room(channel: &str) -> Result<u64, String> {
    let slug = crate::kick::slug(channel);
    let v = call("kick", "GET", &format!("{KICK}/channels?slug={}", oauth::encode(&slug)), None)?;
    v["data"][0]["broadcaster_user_id"].as_u64().ok_or_else(|| format!("no Kick channel {slug}"))
}

fn kick_edit(channel: &str, title: Option<&str>, category: Option<&str>) -> Result<String, String> {
    // the token's channel is the one changed; `channel` checked to be it
    let room = kick_room(channel)?;
    if oauth::account("kick").is_some_and(|(id, _)| id != room.to_string()) {
        return Err("Kick lets the signed-in account change only its own channel".into());
    }
    let mut body = json!({});
    if let Some(t) = title {
        body["stream_title"] = json!(t);
    }
    let mut found = String::new();
    if let Some(c) = category {
        let v = call("kick", "GET", &format!("{KICK}/categories?q={}", oauth::encode(c)), None)?;
        let g = &v["data"][0];
        body["category_id"] = json!(g["id"].as_u64().ok_or_else(|| format!("Kick has no category like “{c}”"))?);
        found = g["name"].as_str().unwrap_or("").to_string();
    }
    call("kick", "PATCH", &format!("{KICK}/channels"), Some(body))?;
    Ok(if found.is_empty() { "Saved".into() } else { format!("Saved (category: {found})") })
}

// ---- X ----

const XAPI: &str = "https://api.x.com/2";
/// The X broadcast Lightair started (its id), while it runs.
static X_LIVE: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

fn x_title() -> Option<String> {
    oauth::kept("x-next-title")
}

/// Goes live on X once its source sees the video (waits up to 90 s; nothing when Lightair isn't
/// streaming to X): the broadcast's address.
pub fn x_go_live() -> Result<Option<String>, String> {
    if !signed_in("x") || X_LIVE.lock().is_some() {
        return Ok(None);
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let source = loop {
        let v = call("x", "GET", &format!("{XAPI}/sources"), None)?;
        if let Some(s) = v["data"].as_array().into_iter().flatten().find(|s| s["is_stream_active"].as_bool() == Some(true)) {
            break s["id"].as_str().unwrap_or("").to_string();
        }
        if std::time::Instant::now() > until {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    };
    let mut body = json!({"source_id": source});
    if let Some(t) = x_title() {
        body["title"] = json!(t);
    }
    let v = call("x", "POST", &format!("{XAPI}/broadcasts"), Some(body))?;
    let d = &v["data"];
    let id = d["id"].as_str().or(d["broadcast_id"].as_str()).ok_or("X didn't start the broadcast")?.to_string();
    *X_LIVE.lock() = Some(id);
    Ok(d["share_url"].as_str().map(String::from))
}

/// Ends the X broadcast Lightair started.
pub fn x_end() -> Result<(), String> {
    let Some(id) = X_LIVE.lock().take() else { return Ok(()) };
    call("x", "PUT", &format!("{XAPI}/broadcasts/{id}/state"), Some(json!({"state": "END"}))).map(|_| ())
}

fn x_info() -> Result<StreamInfo, String> {
    let Some(id) = X_LIVE.lock().clone() else {
        return Ok(StreamInfo { title: x_title().unwrap_or_default(), ..Default::default() });
    };
    let v = call("x", "GET", &format!("{XAPI}/broadcasts/{id}?broadcast.fields=title,state,total_watching,start_ms"), None)?;
    let d = &v["data"];
    Ok(StreamInfo {
        live: d["state"].as_str() == Some("RUNNING"),
        title: d["title"].as_str().unwrap_or("").to_string(),
        category: String::new(),
        viewers: d["total_watching"].as_str().and_then(|n| n.parse().ok()).or(d["total_watching"].as_u64()),
        started_ms: d["start_ms"].as_str().and_then(|n| n.parse().ok()).or(d["start_ms"].as_i64()),
        description: None,
    })
}

// ---- what the rest of Lightair calls ----

/// The stream's details through the API (YouTube: the signed-in channel's own broadcast, so
/// Private and Unlisted streams are found too). None: not signed in officially, or not needed.
pub fn info(platform: &str) -> Option<Result<StreamInfo, String>> {
    if !signed_in(platform) {
        return None;
    }
    match provider(platform) {
        "youtube" => Some(yt_info()),
        "x" => Some(x_info()),
        _ => None,
    }
}

/// Changes what's set (title, description: YouTube only, category by name).
pub fn edit(platform: &str, channel: &str, title: Option<&str>, description: Option<&str>, category: Option<&str>) -> Result<String, String> {
    match provider(platform) {
        "youtube" => yt_edit(title, description, category),
        "twitch" => twitch_edit(channel, title, category),
        "kick" => kick_edit(channel, title, category),
        "x" => {
            let Some(t) = title else { return Ok("Nothing to change (X has no category)".into()) };
            oauth::keep("x-next-title", t);
            Ok(if X_LIVE.lock().is_some() { "Kept for the next time you go live (X can't rename a live broadcast)".into() } else { "Kept: X uses it when you go live".into() })
        }
        other => Err(format!("no official API for {other}")),
    }
}

/// Sends `text` to chat (replying to `reply_to` where the platform can).
pub fn send(platform: &str, channel: &str, text: &str, reply_to: Option<&str>) -> Result<(), String> {
    match provider(platform) {
        "youtube" => {
            let chat = yt_chat_id()?;
            call("youtube", "POST", &format!("{YT}/liveChat/messages?part=snippet"), Some(json!({"snippet": {"liveChatId": chat, "type": "textMessageEvent", "textMessageDetails": {"messageText": text}}})))
                .map(|_| ())
        }
        "twitch" => {
            let mut body = json!({"broadcaster_id": twitch_room(channel)?, "sender_id": twitch_me()?, "message": text});
            if let Some(r) = reply_to {
                body["reply_parent_message_id"] = json!(r);
            }
            let v = call("twitch", "POST", &format!("{HELIX}/chat/messages"), Some(body))?;
            match v["data"][0]["is_sent"].as_bool() {
                Some(false) => Err(format!("Twitch held it back: {}", v["data"][0]["drop_reason"]["message"].as_str().unwrap_or(""))),
                _ => Ok(()),
            }
        }
        "kick" => {
            let mut body = json!({"broadcaster_user_id": kick_room(channel)?, "content": text, "type": "user"});
            if let Some(r) = reply_to {
                body["reply_to_message_id"] = json!(r);
            }
            call("kick", "POST", &format!("{KICK}/chat"), Some(body)).map(|_| ())
        }
        "x" => {
            let id = X_LIVE.lock().clone().ok_or("Lightair isn't live on X")?;
            let text: String = text.chars().take(140).collect();
            let mut body = json!({"text": text});
            if let Some(r) = reply_to {
                body["reply_to"] = json!(r);
            }
            call("x", "POST", &format!("{XAPI}/broadcasts/{id}/chat"), Some(body)).map(|_| ())
        }
        other => Err(format!("no official API for {other}")),
    }
}

/// Moderation through the API.
pub fn moderate(platform: &str, channel: &str, action: crate::moderate::Action) -> Result<(), String> {
    use crate::moderate::Action;
    match (provider(platform), action) {
        ("youtube", Action::Delete { .. }) => Err("YouTube: delete it in YouTube's chat (its API numbers messages differently)".into()),
        ("youtube", Action::Timeout { user_id, seconds, .. }) => yt_ban(user_id, Some(seconds)),
        ("youtube", Action::Ban { user_id, .. }) => yt_ban(user_id, None),
        ("twitch", action) => {
            let (room, me) = (twitch_room(channel)?, twitch_me()?);
            match action {
                Action::Delete { message_id } => call("twitch", "DELETE", &format!("{HELIX}/moderation/chat?broadcaster_id={room}&moderator_id={me}&message_id={}", oauth::encode(message_id)), None),
                Action::Timeout { user_id, seconds, reason, .. } => call(
                    "twitch",
                    "POST",
                    &format!("{HELIX}/moderation/bans?broadcaster_id={room}&moderator_id={me}"),
                    Some(json!({"data": {"user_id": user_id, "duration": seconds.clamp(1, 1_209_600), "reason": reason}})),
                ),
                Action::Ban { user_id, reason, .. } => {
                    call("twitch", "POST", &format!("{HELIX}/moderation/bans?broadcaster_id={room}&moderator_id={me}"), Some(json!({"data": {"user_id": user_id, "reason": reason}})))
                }
            }
            .map(|_| ())
        }
        ("kick", action) => {
            let room = kick_room(channel)?;
            let user = |id: &str| id.parse::<u64>().map_err(|_| "that isn't a Kick user".to_string());
            match action {
                Action::Delete { message_id } => call("kick", "DELETE", &format!("{KICK}/chat/{}", oauth::encode(message_id)), None),
                Action::Timeout { user_id, seconds, reason, .. } => call(
                    "kick",
                    "POST",
                    &format!("{KICK}/moderation/bans"),
                    Some(json!({"broadcaster_user_id": room, "user_id": user(user_id)?, "duration": (seconds / 60).clamp(1, 10080), "reason": reason})),
                ),
                Action::Ban { user_id, reason, .. } => {
                    call("kick", "POST", &format!("{KICK}/moderation/bans"), Some(json!({"broadcaster_user_id": room, "user_id": user(user_id)?, "reason": reason})))
                }
            }
            .map(|_| ())
        }
        (other, _) => Err(format!("no official API for {other}")),
    }
}

fn yt_ban(channel_id: &str, seconds: Option<u64>) -> Result<(), String> {
    let chat = yt_chat_id()?;
    let mut snippet = json!({"liveChatId": chat, "type": "permanent", "bannedUserDetails": {"channelId": channel_id}});
    if let Some(s) = seconds {
        snippet["type"] = json!("temporary");
        snippet["banDurationSeconds"] = json!(s);
    }
    call("youtube", "POST", &format!("{YT}/liveChat/bans?part=snippet"), Some(json!({"snippet": snippet}))).map(|_| ())
}
