//! DouYu (斗鱼) chat, read without logging in: its danmaku server over a WebSocket, in its own
//! framing (a little-endian length twice, a type, then `key@=value/` text), joining the room's
//! everyone group. Chat messages and gifts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;

/// "https://www.douyu.com/9999?…", "9999" → "9999" (a vanity name is resolved to the room id).
pub fn room(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let c = c.rsplit('/').next().unwrap_or(c);
    c.split('?').next().unwrap_or("").to_string()
}

/// One client packet: `key@=value/` text in DouYu's framing.
pub fn pack(body: &str) -> Vec<u8> {
    let mut data = body.as_bytes().to_vec();
    data.push(0);
    let len = (data.len() + 8) as u32;
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&689u16.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&data);
    out
}

/// The text bodies in a frame (one frame can hold several packets).
pub fn unpack(frame: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 12 <= frame.len() {
        let len = u32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]) as usize;
        let end = at + 4 + len;
        if len < 9 || end > frame.len() {
            break;
        }
        let body = &frame[at + 12..end];
        let body = body.strip_suffix(&[0]).unwrap_or(body);
        out.push(String::from_utf8_lossy(body).into_owned());
        at = end;
    }
    out
}

/// `type@=chatmsg/nn@=a@Sb/` → [("type", "chatmsg"), ("nn", "a/b")].
pub fn fields(body: &str) -> Vec<(String, String)> {
    let unescape = |s: &str| s.replace("@S", "/").replace("@A", "@");
    body.split('/').filter_map(|kv| kv.split_once("@=")).map(|(k, v)| (unescape(k), unescape(v))).collect()
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let agent = net::agent();
    let given = room(&sink.channel().channel);
    let info = net::get_json(&agent, &format!("https://www.douyu.com/betard/{given}"), &[])?;
    let rid = info["room"]["room_id"].as_u64().map(|r| r.to_string()).unwrap_or(given);
    if info["room"]["show_status"].as_u64() != Some(1) {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    }
    let mut socket = net::connect("wss://danmuproxy.douyu.com:8506/", Duration::from_millis(500))?;
    for body in [format!("type@=loginreq/roomid@={rid}/"), format!("type@=joingroup/rid@={rid}/gid@=-9999/")] {
        socket.send(Message::binary(pack(&body))).map_err(|e| e.to_string())?;
    }
    sink.status(Status::Connected);
    let mut beat = Instant::now();
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) {
        if beat.elapsed() > Duration::from_secs(40) {
            socket.send(Message::binary(pack("type@=mrkl/"))).map_err(|e| e.to_string())?;
            beat = Instant::now();
        }
        let Some(Message::Binary(frame)) = net::read(&mut socket)? else { continue };
        for body in unpack(&frame) {
            if let Some(e) = parse(&body, &channel) {
                sink.emit(e);
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// A chat message (`chatmsg`) or a gift (`dgb`).
pub fn parse(body: &str, channel: &str) -> Option<ChatEvent> {
    let f = fields(body);
    let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let user = || User {
        id: get("uid").unwrap_or("").into(),
        login: get("nn").unwrap_or("").into(),
        display_name: get("nn").unwrap_or("").into(),
        ..Default::default()
    };
    match get("type")? {
        "chatmsg" => {
            let mut e = ChatEvent::new("douyu", channel, Kind::Message);
            e.native_id = get("cid").map(String::from);
            e.user = Some(user());
            Some(e.with_fragments(text_fragments(get("txt").unwrap_or(""))))
        }
        "dgb" => {
            let count: f64 = get("gfcnt").and_then(|c| c.parse().ok()).unwrap_or(1.0);
            // gifts are mostly free and frequent: a notice, not a highlighted paid message
            let mut e = ChatEvent::new("douyu", channel, Kind::Notice);
            let u = user();
            e.headline = Some(format!("{} sent a gift ×{count}", u.display_name));
            e.user = Some(u);
            e.paid = Some(Paid {
                display: format!("gift ×{count}"),
                amount: count,
                currency: format!("DOUYU_GIFT_{}", get("gfid").unwrap_or("")),
            });
            Some(e)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip_and_messages_parse() {
        let p = pack("type@=chatmsg/nn@=a@Sb/txt@=你好 @@A/cid@=c1/uid@=7/");
        let mut frame = p.clone();
        frame.extend(pack("type@=dgb/nn@=fan/gfid@=824/gfcnt@=3/"));
        let bodies = unpack(&frame);
        assert_eq!(bodies.len(), 2);
        let chat = parse(&bodies[0], "9999").unwrap();
        assert_eq!(chat.user.unwrap().display_name, "a/b");
        assert_eq!(chat.text, "你好 @@");
        assert_eq!(chat.native_id.as_deref(), Some("c1"));
        let gift = parse(&bodies[1], "9999").unwrap();
        assert_eq!((gift.kind, gift.headline.as_deref()), (Kind::Notice, Some("fan sent a gift ×3")));
        assert_eq!(room("https://www.douyu.com/9999?dyshid=1"), "9999");
    }
}
