//! CHZZK (치지직) chat, read without logging in: the live broadcast's chat channel (from the
//! channel's live status), a read-only token, then Naver's chat socket. Sending needs a Naver
//! login (the hidden page: docs/plans/live-integrations.md, 2.10).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::Message;

use crate::event::{Badge, ChatEvent, Fragment, Kind, Paid, Role, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;

/// "https://chzzk.naver.com/live/<id>", "chzzk.naver.com/<id>", "<id>" → the 32-character id.
pub fn channel_id(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    c.rsplit('/').next().unwrap_or(c).split('?').next().unwrap_or("").to_string()
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let agent = net::agent();
    let id = channel_id(&sink.channel().channel);
    let status = net::get_json(&agent, &format!("https://api.chzzk.naver.com/polling/v2/channels/{id}/live-status"), &[])?;
    let content = &status["content"];
    let Some(chat) = content["chatChannelId"].as_str().filter(|_| content["status"].as_str() == Some("OPEN")) else {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    };
    let token = net::get_json(
        &agent,
        &format!("https://comm-api.game.naver.com/nng_main/v1/chats/access-token?channelId={chat}&chatType=STREAMING"),
        &[],
    )?;
    let token = token["content"]["accessToken"].as_str().ok_or("no chat token")?.to_string();
    let server = chat.bytes().map(u32::from).sum::<u32>() % 9 + 1;
    let mut socket = net::connect(&format!("wss://kr-ss{server}.chat.naver.com/chat"), Duration::from_millis(500))?;
    let hello = json!({
        "ver": "3", "cmd": 100, "svcid": "game", "cid": chat, "tid": 1,
        "bdy": {"uid": null, "devType": 2001, "accTkn": token, "auth": "READ"}
    });
    socket.send(Message::text(hello.to_string())).map_err(|e| e.to_string())?;
    let mut pinged = Instant::now();
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) {
        if pinged.elapsed() > Duration::from_secs(20) {
            socket.send(Message::text(json!({"ver": "3", "cmd": 0}).to_string())).map_err(|e| e.to_string())?;
            pinged = Instant::now();
        }
        let Some(Message::Text(text)) = net::read(&mut socket)? else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        match v["cmd"].as_i64() {
            // ping: pong
            Some(0) => socket.send(Message::text(json!({"ver": "3", "cmd": 10000}).to_string())).map_err(|e| e.to_string())?,
            Some(10100) => sink.status(Status::Connected),
            Some(93101) | Some(93102) => {
                for m in v["bdy"].as_array().into_iter().flatten() {
                    if let Some(e) = parse(m, &channel) {
                        sink.emit(e);
                    }
                }
            }
            _ => {}
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// One chat or donation message (93101, 93102).
pub fn parse(m: &Value, channel: &str) -> Option<ChatEvent> {
    // `profile` and `extras` arrive as JSON strings
    let nested = |k: &str| m[k].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).unwrap_or(Value::Null);
    let profile = nested("profile");
    let extras = nested("extras");
    let pay = extras["payAmount"].as_f64().filter(|a| *a > 0.0);
    let mut e = ChatEvent::new("chzzk", channel, if pay.is_some() { Kind::Paid } else { Kind::Message });
    e.ts_ms = m["msgTime"].as_i64().unwrap_or(e.ts_ms);
    e.native_id = Some(format!("{}:{}", m["uid"].as_str().unwrap_or(""), e.ts_ms));
    let mut roles = Vec::new();
    match profile["userRoleCode"].as_str() {
        Some("streamer") => roles.push(Role::Broadcaster),
        Some("streaming_channel_manager") | Some("streaming_chat_manager") => roles.push(Role::Moderator),
        _ => {}
    }
    if !profile["streamingProperty"]["subscription"].is_null() {
        roles.push(Role::Subscriber);
    }
    let badges = profile["activityBadges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| Some(Badge { set: b["badgeId"].as_str()?.into(), version: String::new(), image: b["imageUrl"].as_str().map(String::from) }))
        .collect();
    let name = profile["nickname"].as_str().unwrap_or("익명").to_string();
    e.user = Some(User {
        id: m["uid"].as_str().or(profile["userIdHash"].as_str()).unwrap_or("").into(),
        login: name.clone(),
        display_name: name,
        color: None,
        roles,
        badges,
    });
    e.paid = pay.map(|a| Paid { display: format!("{} 치즈", a as i64), amount: a, currency: "CHEESE".into() });
    let text = m["msg"].as_str().or(m["content"].as_str()).unwrap_or("");
    Some(e.with_fragments(emojis(text, &extras["emojis"])))
}

/// `{:name:}` in the text, drawn from the message's own emoji list.
fn emojis(text: &str, list: &Value) -> Vec<Fragment> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("{:") {
        let Some(j) = rest[i + 2..].find(":}") else { break };
        let name = &rest[i + 2..i + 2 + j];
        match list[name].as_str() {
            Some(url) => {
                out.extend(text_fragments(&rest[..i]));
                out.push(Fragment::Emote { provider: "chzzk".into(), id: name.into(), code: format!(":{name}:"), image: url.into() });
            }
            None => out.extend(text_fragments(&rest[..i + 4 + j])),
        }
        rest = &rest[i + 4 + j..];
    }
    out.extend(text_fragments(rest));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_and_a_donation() {
        let chat = serde_json::json!({
            "uid": "u1", "msgTime": 1700000000000i64, "msg": "안녕 {:d_1:} 반가워",
            "profile": "{\"nickname\":\"시청자\",\"userRoleCode\":\"common_user\",\"streamingProperty\":{\"subscription\":{\"accumulativeMonth\":3}}}",
            "extras": "{\"emojis\":{\"d_1\":\"https://e/d_1.png\"}}"
        });
        let e = parse(&chat, "c1").unwrap();
        assert_eq!(e.kind, Kind::Message);
        assert_eq!(e.text, "안녕 :d_1: 반가워");
        assert!(matches!(&e.fragments[1], Fragment::Emote { image, .. } if image == "https://e/d_1.png"));
        let u = e.user.unwrap();
        assert_eq!((u.display_name.as_str(), u.roles.as_slice()), ("시청자", &[Role::Subscriber][..]));
        let donation = serde_json::json!({
            "uid": "u2", "msgTime": 1700000000500i64, "msg": "응원해요",
            "profile": "{\"nickname\":\"팬\",\"userRoleCode\":\"common_user\"}",
            "extras": "{\"payAmount\":1000}"
        });
        let d = parse(&donation, "c1").unwrap();
        assert_eq!(d.kind, Kind::Paid);
        assert_eq!(d.paid.unwrap().display, "1000 치즈");
    }

    #[test]
    fn a_channel_however_it_was_typed() {
        let id = "c42cd75ec4855a9edf204a407c3c1dd2";
        for c in [id.to_string(), format!("https://chzzk.naver.com/live/{id}"), format!("chzzk.naver.com/{id}/")] {
            assert_eq!(channel_id(&c), id);
        }
    }
}
