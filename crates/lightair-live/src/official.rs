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
//!   stops. X's API sends chat only to a public webhook; Lightair needs no server, so it reads X
//!   chat from X's own broadcast page instead (the signed-in pages).

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
        let r = r
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");
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
        let why = v["error"]["message"]
            .as_str()
            .or(v["message"].as_str())
            .map(String::from)
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("{p} said no ({status}): {why}"));
    }
    Ok(v)
}

// ---- YouTube ----

const YT: &str = "https://www.googleapis.com/youtube/v3";

/// The signed-in channel's broadcast: live now, else the next upcoming one.
fn yt_broadcast() -> Result<Value, String> {
    for status in ["active", "upcoming"] {
        let v = call(
            "youtube",
            "GET",
            &format!("{YT}/liveBroadcasts?part=snippet,status&broadcastStatus={status}&broadcastType=all&maxResults=1"),
            None,
        )?;
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
            .ok_or_else(|| {
                format!(
                    "YouTube's categories are: {}",
                    all.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
                )
            })?;
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
    yt_broadcast()?["snippet"]["liveChatId"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "the stream has no live chat".into())
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
        if let Some(s) =
            v["data"].as_array().into_iter().flatten().find(|s| s["is_stream_active"].as_bool() == Some(true))
        {
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
    let v = call(
        "x",
        "GET",
        &format!("{XAPI}/broadcasts/{id}?broadcast.fields=title,state,total_watching,start_ms"),
        None,
    )?;
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
pub fn edit(
    platform: &str,
    channel: &str,
    title: Option<&str>,
    description: Option<&str>,
    category: Option<&str>,
) -> Result<String, String> {
    match provider(platform) {
        "youtube" => yt_edit(title, description, category),
        "twitch" => twitch_edit(channel, title, category),
        "kick" => kick_edit(channel, title, category),
        "x" => {
            let Some(t) = title else { return Ok("Nothing to change (X has no category)".into()) };
            oauth::keep("x-next-title", t);
            Ok(if X_LIVE.lock().is_some() {
                "Kept for the next time you go live (X can't rename a live broadcast)".into()
            } else {
                "Kept: X uses it when you go live".into()
            })
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
                Some(false) => Err(format!(
                    "Twitch held it back: {}",
                    v["data"][0]["drop_reason"]["message"].as_str().unwrap_or("")
                )),
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
        ("youtube", Action::Delete { .. }) => {
            Err("YouTube: delete it in YouTube's chat (its API numbers messages differently)".into())
        }
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

// ---- Twitch events (EventSub over WebSocket) ----

/// Channel-points redemptions, read while signed in to Twitch officially, into `hub` as
/// notices with the reward (kind "redeem" for automations, `{{ reward }}`). Runs until `stop`;
/// waits while not signed in, reconnects after a failure.
pub fn watch_twitch_events(hub: crate::Hub, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    while !stop.load(Ordering::Acquire) {
        if signed_in("twitch")
            && let Err(e) = twitch_events(&hub, &stop)
        {
            tracing::debug!(%e, "Twitch events");
        }
        crate::hub::sleep_unless(&stop, std::time::Duration::from_secs(15));
    }
}

fn twitch_events(hub: &crate::Hub, stop: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    use tungstenite::Message;
    let (me, login) = oauth::account("twitch").ok_or("not signed in")?;
    let mut url = "wss://eventsub.wss.twitch.tv/ws".to_string();
    'connect: loop {
        let mut socket = net::connect(&url, std::time::Duration::from_millis(500))?;
        let mut alive = std::time::Instant::now();
        while !stop.load(Ordering::Acquire) {
            // Twitch sends a keepalive at least every 10 s: silence means the link is gone
            if alive.elapsed() > std::time::Duration::from_secs(30) {
                return Err("Twitch went quiet".into());
            }
            let Some(m) = net::read(&mut socket)? else { continue };
            let Message::Text(t) = m else { continue };
            alive = std::time::Instant::now();
            let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
            match v["metadata"]["message_type"].as_str() {
                Some("session_welcome") if url.ends_with("/ws") => {
                    let session = v["payload"]["session"]["id"].as_str().unwrap_or("").to_string();
                    call(
                        "twitch",
                        "POST",
                        &format!("{HELIX}/eventsub/subscriptions"),
                        Some(json!({
                            "type": "channel.channel_points_custom_reward_redemption.add",
                            "version": "1",
                            "condition": {"broadcaster_user_id": me},
                            "transport": {"method": "websocket", "session_id": session},
                        })),
                    )
                    .map_err(|e| {
                        format!(
                            "couldn't listen for redemptions ({e}): sign in to Twitch again for the newer permissions"
                        )
                    })?;
                }
                Some("session_reconnect") => {
                    url = v["payload"]["session"]["reconnect_url"]
                        .as_str()
                        .unwrap_or("wss://eventsub.wss.twitch.tv/ws")
                        .to_string();
                    continue 'connect;
                }
                Some("notification") => {
                    if let Some(e) = redemption(&v["payload"]["event"], &login) {
                        hub.inject(crate::Channel { platform: "twitch".into(), channel: login.clone() }, e);
                    }
                }
                Some("revocation") => return Err("Twitch stopped sending redemptions".into()),
                _ => {}
            }
        }
        let _ = socket.close(None);
        return Ok(());
    }
}

/// A redemption → a notice with the reward as its "payment" in channel points.
fn redemption(ev: &Value, channel: &str) -> Option<crate::ChatEvent> {
    let reward = ev["reward"]["title"].as_str()?.to_string();
    let cost = ev["reward"]["cost"].as_u64().unwrap_or(0);
    let mut e = crate::ChatEvent::new("twitch", channel, crate::Kind::Notice);
    let name = ev["user_name"].as_str().unwrap_or("").to_string();
    e.user = Some(crate::User {
        id: ev["user_id"].as_str().unwrap_or("").into(),
        login: ev["user_login"].as_str().unwrap_or("").into(),
        display_name: name.clone(),
        ..Default::default()
    });
    e.headline = Some(format!("{name} redeemed {reward}"));
    e.paid = Some(crate::Paid { display: reward, amount: cost as f64, currency: "CHANNEL_POINTS".into() });
    e.native_id = ev["id"].as_str().map(String::from);
    let input = ev["user_input"].as_str().unwrap_or("");
    Some(if input.is_empty() { e } else { e.with_fragments(crate::event::text_fragments(input)) })
}

// ---- actions for automations and the remote API ----

/// The platform actions automation steps (and remote clients) can take through the official
/// APIs: `stream.edit` {platform, title, category, description}, and on Twitch `twitch.ad`
/// {seconds}, `twitch.poll` {title, choices, seconds}, `twitch.prediction` {title, outcomes,
/// seconds}, `twitch.raid` {channel}, `twitch.shoutout` {channel}, `twitch.clip`,
/// `twitch.marker` {description}.
pub fn is_action(method: &str) -> bool {
    matches!(
        method,
        "stream.edit"
            | "twitch.ad"
            | "twitch.poll"
            | "twitch.prediction"
            | "twitch.raid"
            | "twitch.shoutout"
            | "twitch.clip"
            | "twitch.marker"
    )
}

fn list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => {
            a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).collect()
        }
        Value::String(s) => s.split([',', '\n']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => vec![],
    }
}

fn number(v: &Value, default: u64) -> u64 {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f as u64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(default)
}

pub fn action(method: &str, p: &Value) -> Result<Value, String> {
    let text = |k: &str| p[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if method == "stream.edit" {
        let platform = text("platform").unwrap_or_else(|| "twitch".into());
        if !signed_in(&platform) {
            return Err(format!("sign in to {platform} officially first (Settings › Connections)"));
        }
        let channel = oauth::account(provider(&platform)).map(|(_, name)| name).unwrap_or_default();
        let said = edit(
            &platform,
            &channel,
            text("title").as_deref(),
            text("description").as_deref(),
            text("category").as_deref(),
        )?;
        return Ok(json!({"result": said}));
    }
    if !signed_in("twitch") {
        return Err("sign in to Twitch officially first (Settings › Connections)".into());
    }
    let me = twitch_me()?;
    let other = |k: &str| -> Result<String, String> { twitch_room(&text(k).ok_or_else(|| format!("{k} is needed"))?) };
    match method {
        "twitch.ad" => {
            let length = number(&p["seconds"], 60).clamp(30, 180);
            call(
                "twitch",
                "POST",
                &format!("{HELIX}/channels/commercial"),
                Some(json!({"broadcaster_id": me, "length": length})),
            )
        }
        "twitch.poll" => {
            let choices: Vec<Value> = list(&p["choices"]).into_iter().take(5).map(|c| json!({"title": c})).collect();
            if choices.len() < 2 {
                return Err("a poll needs 2 to 5 choices".into());
            }
            let body = json!({"broadcaster_id": me, "title": text("title").unwrap_or_else(|| "Poll".into()), "choices": choices, "duration": number(&p["seconds"], 60).clamp(15, 1800)});
            call("twitch", "POST", &format!("{HELIX}/polls"), Some(body))
        }
        "twitch.prediction" => {
            let outcomes: Vec<Value> = list(&p["outcomes"]).into_iter().take(10).map(|c| json!({"title": c})).collect();
            if outcomes.len() < 2 {
                return Err("a prediction needs 2 to 10 outcomes".into());
            }
            let body = json!({"broadcaster_id": me, "title": text("title").unwrap_or_else(|| "Prediction".into()), "outcomes": outcomes, "prediction_window": number(&p["seconds"], 120).clamp(30, 1800)});
            call("twitch", "POST", &format!("{HELIX}/predictions"), Some(body))
        }
        "twitch.raid" => call(
            "twitch",
            "POST",
            &format!("{HELIX}/raids?from_broadcaster_id={me}&to_broadcaster_id={}", other("channel")?),
            None,
        ),
        "twitch.shoutout" => call(
            "twitch",
            "POST",
            &format!(
                "{HELIX}/chat/shoutouts?from_broadcaster_id={me}&to_broadcaster_id={}&moderator_id={me}",
                other("channel")?
            ),
            None,
        ),
        "twitch.clip" => {
            let v = call("twitch", "POST", &format!("{HELIX}/clips?broadcaster_id={me}"), None)?;
            let id = v["data"][0]["id"].as_str().unwrap_or("");
            Ok(json!({"id": id, "url": format!("https://clips.twitch.tv/{id}"), "edit_url": v["data"][0]["edit_url"]}))
        }
        "twitch.marker" => {
            let mut body = json!({"user_id": me});
            if let Some(d) = text("description") {
                body["description"] = json!(d.chars().take(140).collect::<String>());
            }
            call("twitch", "POST", &format!("{HELIX}/streams/markers"), Some(body))
        }
        other => Err(format!("no action {other}")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn choices_come_from_lists_or_text() {
        assert_eq!(super::list(&serde_json::json!("Yes, No ,\nMaybe")), vec!["Yes", "No", "Maybe"]);
        assert_eq!(super::list(&serde_json::json!(["a", " ", "b"])), vec!["a", "b"]);
    }

    #[test]
    fn nothing_happens_without_the_official_sign_in() {
        assert!(super::action("twitch.ad", &serde_json::json!({})).is_err());
    }

    #[test]
    fn a_redemption_is_a_notice_with_its_reward() {
        let ev = serde_json::json!({"id": "r1", "user_id": "9", "user_login": "fan", "user_name": "Fan", "user_input": "play lofi",
            "reward": {"title": "Song request", "cost": 500}});
        let e = super::redemption(&ev, "me").unwrap();
        assert_eq!(e.alert_kind(), Some("redeem"));
        assert_eq!(
            (e.headline.as_deref(), e.text.as_str(), e.paid.unwrap().amount),
            (Some("Fan redeemed Song request"), "play lofi", 500.0)
        );
    }
}

// ── The stream key and ingest through sign-in (the desktop's outputs-upgrades §14, Lightair
// Live's studio-plan F4) ──────────────────────────────────────────────────────────────────────

/// Where a stream goes when its key comes through sign-in, and the YouTube broadcast made
/// for it (to end when the stream does). The key is never stored: kept only while streaming.
#[derive(Debug, Clone, PartialEq)]
pub struct Ingest {
    pub url: String,
    pub key: String,
    pub broadcast: Option<String>,
}

/// The signed-in account's stream key: Twitch (Helix `streams/key`, scope
/// `channel:read:stream_key`; an older sign-in without it is asked to sign in again).
pub fn stream_key(platform: &str) -> Result<String, String> {
    match platform {
        "twitch" => {
            let me = twitch_me()?;
            let v = call("twitch", "GET", &format!("{HELIX}/streams/key?broadcaster_id={me}"), None).map_err(|e| {
                if e.contains("401") || e.to_lowercase().contains("scope") {
                    "Twitch needs you to sign in again to give Lightair your stream key.".to_string()
                } else {
                    e
                }
            })?;
            twitch_key_of(&v)
        }
        other => Err(format!("{other} doesn't give its stream key through sign-in")),
    }
}

fn twitch_key_of(v: &Value) -> Result<String, String> {
    v["data"][0]["stream_key"]
        .as_str()
        .filter(|k| !k.is_empty())
        .map(String::from)
        .ok_or_else(|| "Twitch didn't give the stream key".into())
}

/// The channel's reusable stream (made once if there's none): its id and where it ingests.
pub fn youtube_ingest() -> Result<(String, Ingest), String> {
    let v = call(
        "youtube",
        "GET",
        &format!("{YT}/liveStreams?part=id,snippet,cdn,contentDetails&mine=true&maxResults=50"),
        None,
    )?;
    if let Some(found) = youtube_reusable(&v) {
        return Ok(found);
    }
    let made = call(
        "youtube",
        "POST",
        &format!("{YT}/liveStreams?part=id,snippet,cdn,contentDetails"),
        Some(youtube_stream_body()),
    )?;
    youtube_stream_of(&made).ok_or_else(|| "YouTube didn't make a stream key".into())
}

fn youtube_stream_body() -> Value {
    json!({
        "snippet": {"title": "Lightair"},
        "cdn": {"frameRate": "variable", "ingestionType": "rtmp", "resolution": "variable"},
        "contentDetails": {"isReusable": true}
    })
}

/// A liveStream's id and ingest (RTMPS where given).
fn youtube_stream_of(s: &Value) -> Option<(String, Ingest)> {
    let info = &s["cdn"]["ingestionInfo"];
    let url = info["rtmpsIngestionAddress"].as_str().filter(|u| !u.is_empty()).or(info["ingestionAddress"].as_str())?;
    let key = info["streamName"].as_str().filter(|k| !k.is_empty())?;
    Some((s["id"].as_str()?.to_string(), Ingest { url: url.to_string(), key: key.to_string(), broadcast: None }))
}

/// The first reusable RTMP stream in a `liveStreams.list` answer.
fn youtube_reusable(v: &Value) -> Option<(String, Ingest)> {
    v["items"]
        .as_array()?
        .iter()
        .filter(|s| s["contentDetails"]["isReusable"].as_bool() == Some(true))
        .filter(|s| s["cdn"]["ingestionType"].as_str().is_none_or(|t| t == "rtmp"))
        .find_map(youtube_stream_of)
}

/// A broadcast made now and bound to the channel's reusable stream: it goes live by itself
/// when the stream arrives and ends when it stops (`enableAutoStart`, `enableAutoStop`).
/// `privacy`: "public", "unlisted" or "private". About 150 quota units.
pub fn youtube_create_broadcast(title: &str, privacy: &str) -> Result<Ingest, String> {
    let (stream, mut ingest) = youtube_ingest()?;
    let title = if title.trim().is_empty() { "Live" } else { title.trim() };
    let b = call(
        "youtube",
        "POST",
        &format!("{YT}/liveBroadcasts?part=id,snippet,status,contentDetails"),
        Some(youtube_broadcast_body(title, privacy, &rfc3339_now())),
    )?;
    let id = b["id"].as_str().ok_or("YouTube didn't make the broadcast")?.to_string();
    call(
        "youtube",
        "POST",
        &format!("{YT}/liveBroadcasts/bind?id={id}&part=id,contentDetails&streamId={stream}"),
        None,
    )?;
    ingest.broadcast = Some(id);
    Ok(ingest)
}

fn youtube_broadcast_body(title: &str, privacy: &str, start: &str) -> Value {
    let privacy = if matches!(privacy, "public" | "unlisted" | "private") { privacy } else { "public" };
    json!({
        "snippet": {"title": title.chars().take(100).collect::<String>(), "scheduledStartTime": start},
        "status": {"privacyStatus": privacy, "selfDeclaredMadeForKids": false},
        "contentDetails": {"enableAutoStart": true, "enableAutoStop": true, "latencyPreference": "normal"}
    })
}

/// Ends a broadcast made by `youtube_create_broadcast` (when the stream ends).
pub fn youtube_end_broadcast(id: &str) -> Result<(), String> {
    call(
        "youtube",
        "POST",
        &format!("{YT}/liveBroadcasts/transition?broadcastStatus=complete&id={id}&part=status"),
        None,
    )
    .map(|_| ())
}

/// Now, as `2026-11-22T10:04:05Z`.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    rfc3339(secs)
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil from days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

#[cfg(test)]
mod ingest_tests {
    use super::*;

    #[test]
    fn keys_and_ingests_are_read_from_the_answers() {
        assert_eq!(twitch_key_of(&json!({"data": [{"stream_key": "live_1_abc"}]})).unwrap(), "live_1_abc");
        assert!(twitch_key_of(&json!({"data": []})).is_err());
        let list = json!({"items": [
            {"id": "once", "contentDetails": {"isReusable": false},
             "cdn": {"ingestionType": "rtmp", "ingestionInfo": {"ingestionAddress": "rtmp://a.rtmp.youtube.com/live2", "streamName": "k0"}}},
            {"id": "s1", "contentDetails": {"isReusable": true},
             "cdn": {"ingestionType": "rtmp", "ingestionInfo": {"ingestionAddress": "rtmp://a.rtmp.youtube.com/live2", "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2", "streamName": "k1"}}}
        ]});
        let (id, ingest) = youtube_reusable(&list).unwrap();
        assert_eq!(
            (id.as_str(), ingest.url.as_str(), ingest.key.as_str()),
            ("s1", "rtmps://a.rtmps.youtube.com/live2", "k1")
        );
        assert!(youtube_reusable(&json!({"items": []})).is_none());
        assert_eq!(youtube_stream_body()["contentDetails"]["isReusable"], true);
        let b = youtube_broadcast_body("My show", "secret", "2026-11-22T10:00:00Z");
        assert_eq!(b["status"]["privacyStatus"], "public");
        assert_eq!(b["contentDetails"]["enableAutoStart"], true);
        assert_eq!(b["snippet"]["scheduledStartTime"], "2026-11-22T10:00:00Z");
    }

    #[test]
    fn times_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_349_045), "2026-11-22T12:04:05Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
