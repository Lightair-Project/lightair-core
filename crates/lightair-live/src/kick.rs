//! Kick chat, read without logging in. Kick's API sits behind Cloudflare, which turns away
//! anything but a browser, so the channel (and its chatroom number) comes through a hidden page
//! ([`crate::pages`]); the chat itself is Kick's Pusher socket, which anyone can join: the
//! `chatrooms.<id>.v2` channel carries messages, subscriptions, gifts, hosts and deletions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tungstenite::Message;

use crate::event::{Badge, ChatEvent, Fragment, Kind, ReplyTo, Role, Status, User, plain_text};
use crate::hub::Sink;
use crate::{net, pages};

/// Kick's Pusher app when its realtime endpoint can't be asked (it rotates: October 2026's).
const FALLBACK: (&str, &str) = ("34bf7a0ff419a2a775b9", "us2");

/// What the hidden page runs on the channel's API URL: the channel, and the realtime endpoint's
/// current Pusher credentials (asked from inside the page; plain clients get no answer).
const SCRIPT: &str = r#"(async function () {
  var channel;
  try { channel = JSON.parse(document.body.innerText); } catch (e) { console.log('kick: not JSON yet: ' + document.title); return; }
  var realtime = null;
  try {
    var r = await fetch('https://web.kick.com/api/v1/realtime/connection', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify({ client: { id: crypto.randomUUID(), type: 'web' }, capabilities: { accepted_providers: [{ provider: 'pusher' }] } }),
      signal: AbortSignal.timeout(10000)
    });
    realtime = await r.json();
  } catch (e) {}
  lightair(JSON.stringify({ channel: channel, realtime: realtime }));
})();"#;

/// A channel's chatroom and the Pusher app (key, cluster) to read it with.
#[derive(Clone)]
struct Room {
    id: u64,
    /// The channel's own number (its `channel_<id>` events: Kicks gifts, gifted subs).
    channel: u64,
    key: String,
    cluster: String,
}

/// Rooms already looked up (a reconnect doesn't need the browser again).
static ROOMS: Mutex<Option<HashMap<String, Room>>> = Mutex::new(None);

/// "https://kick.com/xqc", "kick.com/xqc/videos", "xqc" → "xqc".
pub fn slug(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let path = c.split("://").nth(1).unwrap_or(c);
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or("");
    let s = if first.contains('.') { parts.next().unwrap_or("") } else { first };
    s.split('?').next().unwrap_or("").to_lowercase()
}

/// The realtime endpoint's Pusher app, if it answered with one.
fn pusher_app(realtime: &Value) -> Option<(String, String)> {
    realtime["data"]["connections"].as_array()?.iter().find(|c| c["provider"] == "pusher").and_then(|c| {
        let cr = &c["credentials"];
        Some((cr["app_key"].as_str().filter(|k| !k.is_empty())?.to_string(), cr["cluster"].as_str().filter(|k| !k.is_empty())?.to_string()))
    })
}

fn room(slug: &str, stop: &AtomicBool) -> Result<Room, String> {
    if let Some(r) = ROOMS.lock().as_ref().and_then(|m| m.get(slug).cloned()) {
        return Ok(r);
    }
    let text = pages::run(&format!("https://kick.com/api/v2/channels/{slug}"), SCRIPT, Duration::from_secs(45), stop)?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let id = v["channel"]["chatroom"]["id"].as_u64().ok_or_else(|| format!("no Kick channel \"{slug}\""))?;
    let (key, cluster) = pusher_app(&v["realtime"]).unwrap_or_else(|| {
        tracing::info!("Kick's realtime endpoint didn't answer; using the known Pusher app");
        (FALLBACK.0.into(), FALLBACK.1.into())
    });
    let r = Room { id, channel: v["channel"]["id"].as_u64().unwrap_or(0), key, cluster };
    ROOMS.lock().get_or_insert_with(HashMap::new).insert(slug.to_string(), r.clone());
    Ok(r)
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let slug = slug(&sink.channel().channel);
    let room = room(&slug, stop)?;
    let url = format!("wss://ws-{}.pusher.com/app/{}?protocol=7&client=js&version=8.4.0&flash=false", room.cluster, room.key);
    let mut socket = net::connect(&url, Duration::from_millis(500))?;
    let mut channels = vec![format!("chatrooms.{}.v2", room.id)];
    if room.channel > 0 {
        channels.push(format!("channel_{}", room.channel));
    }
    for c in channels {
        let subscribe = json!({ "event": "pusher:subscribe", "data": { "auth": "", "channel": c } });
        socket.send(Message::text(subscribe.to_string())).map_err(|e| e.to_string())?;
    }
    let mut ping = Instant::now();
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) {
        if ping.elapsed() > Duration::from_secs(60) {
            socket.send(Message::text(r#"{"event":"pusher:ping","data":{}}"#)).map_err(|e| e.to_string())?;
            ping = Instant::now();
        }
        let Some(Message::Text(text)) = net::read(&mut socket)? else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        match v["event"].as_str().unwrap_or("") {
            "pusher_internal:subscription_succeeded" => sink.status(Status::Connected),
            "pusher:ping" => {
                socket.send(Message::text(r#"{"event":"pusher:pong","data":{}}"#)).map_err(|e| e.to_string())?;
            }
            "pusher:error" => {
                // a rotated app (4001): look it up again next time
                ROOMS.lock().as_mut().map(|m| m.remove(&slug));
                return Err(format!("Kick chat: {}", v["data"]["message"].as_str().unwrap_or("refused")));
            }
            event => {
                // the payload is JSON in a string
                let data = v["data"].as_str().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Value::Null);
                if let Some(e) = parse(event, &data, &channel) {
                    sink.emit(e);
                }
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// "hi [emote:37226:KEKW]" → text and Kick emotes.
pub fn fragments(content: &str) -> Vec<Fragment> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("[emote:") {
        let Some(len) = rest[start..].find(']') else { break };
        let inner = &rest[start + 7..start + len];
        let Some((id, name)) = inner.split_once(':') else { break };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        if start > 0 {
            out.push(Fragment::Text { text: rest[..start].into() });
        }
        out.push(Fragment::Emote {
            provider: "kick".into(),
            id: id.into(),
            code: name.into(),
            image: format!("https://files.kick.com/emotes/{id}/fullsize"),
        });
        rest = &rest[start + len + 1..];
    }
    if !rest.is_empty() {
        out.push(Fragment::Text { text: rest.into() });
    }
    out
}

fn user(sender: &Value) -> User {
    let s = |k: &str| sender[k].as_str().unwrap_or("").to_string();
    let id = sender["id"].as_u64().map(|i| i.to_string()).unwrap_or_default();
    let identity = &sender["identity"];
    let mut roles = Vec::new();
    let mut badges = Vec::new();
    for b in identity["badges"].as_array().into_iter().flatten() {
        let kind = b["type"].as_str().unwrap_or("");
        let role = match kind {
            "broadcaster" => Some(Role::Broadcaster),
            "moderator" => Some(Role::Moderator),
            "vip" => Some(Role::Vip),
            "subscriber" | "founder" => Some(Role::Subscriber),
            "verified" => Some(Role::Verified),
            _ => None,
        };
        if let Some(r) = role.filter(|r| !roles.contains(r)) {
            roles.push(r);
        }
        badges.push(Badge { set: kind.into(), version: b["count"].as_u64().map(|c| c.to_string()).unwrap_or_default(), image: None });
    }
    User {
        id,
        login: if s("slug").is_empty() { s("username").to_lowercase() } else { s("slug") },
        display_name: s("username"),
        color: identity["color"].as_str().filter(|c| !c.is_empty()).map(String::from),
        roles,
        badges,
    }
}

fn lightair_live_paid(amount: f64, gift: &str) -> crate::event::Paid {
    crate::event::Paid { display: format!("{gift} · {amount} Kicks"), amount, currency: "KICKS".into() }
}

/// One of the chatroom's events.
pub fn parse(event: &str, d: &Value, channel: &str) -> Option<ChatEvent> {
    let name = event.rsplit('\\').next().unwrap_or(event);
    match name {
        "ChatMessageEvent" => {
            let mut e = ChatEvent::new("kick", channel, Kind::Message);
            e.native_id = d["id"].as_str().map(String::from);
            e.user = Some(user(&d["sender"]));
            if d["type"].as_str() == Some("reply") {
                let m = &d["metadata"];
                e.reply_to = Some(ReplyTo {
                    native_id: m["original_message"]["id"].as_str().map(String::from),
                    author: m["original_sender"]["username"].as_str().unwrap_or("").into(),
                    excerpt: plain_text(&fragments(m["original_message"]["content"].as_str().unwrap_or(""))),
                });
            }
            Some(e.with_fragments(fragments(d["content"].as_str().unwrap_or(""))))
        }
        "SubscriptionEvent" => {
            let mut e = ChatEvent::new("kick", channel, Kind::Membership);
            let who = d["username"].as_str().unwrap_or("").to_string();
            let months = d["months"].as_u64().unwrap_or(1);
            e.headline = Some(if months > 1 { format!("{who} subscribed for {months} months") } else { format!("{who} subscribed") });
            e.user = Some(User { login: who.to_lowercase(), display_name: who, ..Default::default() });
            Some(e)
        }
        "GiftedSubscriptionsEvent" => {
            let mut e = ChatEvent::new("kick", channel, Kind::Membership);
            let who = d["gifter_username"].as_str().unwrap_or("Someone").to_string();
            let n = d["gifted_usernames"].as_array().map_or(1, Vec::len);
            e.headline = Some(format!("{who} gifted {n} sub{}", if n == 1 { "" } else { "s" }));
            e.user = Some(User { login: who.to_lowercase(), display_name: who, ..Default::default() });
            Some(e)
        }
        "StreamHostEvent" => {
            let mut e = ChatEvent::new("kick", channel, Kind::Raid { viewers: d["number_viewers"].as_u64().unwrap_or(0) });
            let who = d["host_username"].as_str().unwrap_or("").to_string();
            e.user = Some(User { login: who.to_lowercase(), display_name: who, ..Default::default() });
            Some(e.with_fragments(fragments(d["optional_message"].as_str().unwrap_or(""))))
        }
        // Kick's paid gifts ("Kicks"): who, which gift, how many Kicks
        "KicksGifted" => {
            let mut e = ChatEvent::new("kick", channel, Kind::Paid);
            let sender = if d["sender"].is_object() { &d["sender"] } else { &d["user"] };
            let who = sender["username"].as_str().or(d["username"].as_str()).unwrap_or("Someone").to_string();
            e.user = Some(User { id: sender["id"].as_u64().map(|i| i.to_string()).unwrap_or_default(), login: who.to_lowercase(), display_name: who, ..Default::default() });
            let gift = &d["gift"];
            let amount = gift["amount"].as_f64().or(d["amount"].as_f64()).unwrap_or(0.0);
            let name = gift["name"].as_str().or(d["gift_name"].as_str()).unwrap_or("Kicks");
            e.paid = Some(lightair_live_paid(amount, name));
            let message = gift["message"].as_str().or(d["message"].as_str()).unwrap_or("");
            Some(e.with_fragments(fragments(message)))
        }
        "MessageDeletedEvent" => Some(ChatEvent::new(
            "kick",
            channel,
            Kind::Deletion { native_id: d["message"]["id"].as_str().map(String::from), user_id: None },
        )),
        "UserBannedEvent" => Some(ChatEvent::new(
            "kick",
            channel,
            Kind::Deletion { native_id: None, user_id: d["user"]["id"].as_u64().map(|i| i.to_string()) },
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_with_emotes_and_replies() {
        let d: Value = serde_json::from_str(
            r##"{"id":"m1","content":"hi [emote:37226:KEKW] there","type":"reply",
            "sender":{"id":7,"username":"Viewer","slug":"viewer","identity":{"color":"#E9113C","badges":[{"type":"moderator","text":"Moderator"},{"type":"subscriber","text":"Subscriber","count":3}]}},
            "metadata":{"original_sender":{"id":1,"username":"Host"},"original_message":{"id":"m0","content":"gm [emote:1:wave]"}}}"##,
        )
        .unwrap();
        let e = parse("App\\Events\\ChatMessageEvent", &d, "viewer").unwrap();
        assert_eq!(e.text, "hi KEKW there");
        assert!(matches!(&e.fragments[1], Fragment::Emote { id, image, .. } if id == "37226" && image.ends_with("/37226/fullsize")));
        let u = e.user.unwrap();
        assert_eq!((u.display_name.as_str(), u.roles.as_slice()), ("Viewer", [Role::Moderator, Role::Subscriber].as_slice()));
        assert_eq!(e.reply_to.unwrap().excerpt, "gm wave");
    }

    #[test]
    fn subs_gifts_hosts_and_deletions() {
        let gift: Value = serde_json::from_str(r#"{"gifted_usernames":["a","b"],"gifter_username":"fan"}"#).unwrap();
        assert_eq!(parse("App\\Events\\GiftedSubscriptionsEvent", &gift, "x").unwrap().headline.as_deref(), Some("fan gifted 2 subs"));
        let kicks: Value = serde_json::from_str(r#"{"sender":{"id":5,"username":"Fan"},"gift":{"name":"Rage Quit","amount":100,"message":"gg"}}"#).unwrap();
        let k = parse("KicksGifted", &kicks, "x").unwrap();
        assert_eq!((k.kind, k.paid.unwrap().amount, k.text.as_str()), (Kind::Paid, 100.0, "gg"));
        let host: Value = serde_json::from_str(r#"{"number_viewers":12,"host_username":"pal","optional_message":""}"#).unwrap();
        assert_eq!(parse("App\\Events\\StreamHostEvent", &host, "x").unwrap().kind, Kind::Raid { viewers: 12 });
        let del: Value = serde_json::from_str(r#"{"id":"d","message":{"id":"m1"}}"#).unwrap();
        assert_eq!(parse("App\\Events\\MessageDeletedEvent", &del, "x").unwrap().kind, Kind::Deletion { native_id: Some("m1".into()), user_id: None });
        assert_eq!(fragments("[emote:x:y] plain"), vec![Fragment::Text { text: "[emote:x:y] plain".into() }]);
        let rt: Value = serde_json::from_str(r#"{"data":{"connections":[{"provider":"pusher","credentials":{"app_key":"k1","cluster":"us2"}}]}}"#).unwrap();
        assert_eq!(pusher_app(&rt), Some(("k1".into(), "us2".into())));
        assert_eq!(pusher_app(&Value::Null), None);
        for c in ["https://kick.com/XQC", "kick.com/xqc/videos", "xqc"] {
            assert_eq!(slug(c), "xqc");
        }
    }
}
