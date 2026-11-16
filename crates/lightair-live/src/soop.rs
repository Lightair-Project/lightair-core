//! SOOP (숲, formerly AfreecaTV) chat, read without logging in: the broadcast's chat server
//! (from the live player's API), then SOOP's own framing over a WebSocket: `ESC TAB`, a 4-digit
//! service code, a 6-digit length, 2 flag digits, and form-feed-separated fields. Anonymous
//! entry is 0001 (login) then 0002 (join the chat number); 0000 keeps it alive. Chat (0005), star
//! balloons (0018, 0033 in a sub-broadcast), ad balloons (0087), video balloons (0105),
//! subscriptions (0091 new, 0093 renewed, 0108 gifted), Quick View and OGQ gifts (0045, 0118) and
//! mission gifts (0121). Field layouts from joyfuI/soop-chat and wei756/soop-chat-client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;

const FF: char = '\u{0c}';

/// "https://play.sooplive.co.kr/<id>/<broadcast>", "ch.sooplive.co.kr/<id>", "<id>" → "<id>".
pub fn streamer(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let path = c.split("://").nth(1).unwrap_or(c);
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or("");
    let id = if first.contains('.') { parts.next().unwrap_or("") } else { first };
    id.split('?').next().unwrap_or("").to_string()
}

/// One packet: header and fields.
pub fn pack(service: u16, payload: &str) -> Vec<u8> {
    let body = payload.as_bytes();
    let mut out = format!("\u{1b}\t{service:04}{:06}00", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Complete packets from the front of `buf` (taken out of it): (service, fields).
pub fn unpack(buf: &mut Vec<u8>) -> Vec<(u16, Vec<String>)> {
    let mut out = Vec::new();
    loop {
        // resync on the marker
        let Some(start) = buf.windows(2).position(|w| w == [0x1b, b'\t']) else {
            buf.clear();
            break;
        };
        buf.drain(..start);
        if buf.len() < 14 {
            break;
        }
        let num = |a: usize, b: usize| std::str::from_utf8(&buf[a..b]).ok().and_then(|s| s.parse::<usize>().ok());
        let (Some(service), Some(len)) = (num(2, 6), num(6, 12)) else {
            buf.drain(..2);
            continue;
        };
        if buf.len() < 14 + len {
            break;
        }
        let payload = String::from_utf8_lossy(&buf[14..14 + len]).into_owned();
        buf.drain(..14 + len);
        out.push((service as u16, payload.split(FF).map(String::from).collect()));
    }
    out
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let id = streamer(&sink.channel().channel);
    let agent = net::agent();
    let form = format!("bid={id}&type=live&player_type=html5&stream_type=common&quality=HD&mode=landing&from_api=0&pwd=&is_revive=false");
    let mut res = agent
        .post(&format!("https://live.sooplive.com/afreeca/player_live_api.php?bjid={id}"))
        .header("User-Agent", net::USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form)
        .map_err(|e| e.to_string())?;
    let info: serde_json::Value =
        serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let ch = &info["CHANNEL"];
    if ch["RESULT"].as_i64() != Some(1) {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    }
    let as_text = |v: &serde_json::Value| v.as_str().map(String::from).or_else(|| v.as_u64().map(|n| n.to_string())).unwrap_or_default();
    let (domain, port, chat_no) = (as_text(&ch["CHDOMAIN"]).to_lowercase(), as_text(&ch["CHPT"]), as_text(&ch["CHATNO"]));
    let port: u32 = port.parse().map_err(|_| "no chat port")?;
    let url = format!("wss://{domain}:{}/Websocket/{id}", port + 1);
    let mut socket = net::connect_with(&url, &[("Sec-WebSocket-Protocol", "chat")], Duration::from_millis(500))?;
    socket.send(Message::binary(pack(1, &format!("{FF}{FF}{FF}16{FF}")))).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut joined = false;
    let mut alive = Instant::now();
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) {
        if joined && alive.elapsed() > Duration::from_secs(55) {
            socket.send(Message::binary(pack(0, &FF.to_string()))).map_err(|e| e.to_string())?;
            alive = Instant::now();
        }
        let Some(m) = net::read(&mut socket)? else { continue };
        let bytes = match m {
            Message::Binary(b) => b.to_vec(),
            Message::Text(t) => t.as_bytes().to_vec(),
            _ => continue,
        };
        buf.extend_from_slice(&bytes);
        for (service, fields) in unpack(&mut buf) {
            match service {
                1 if !joined => {
                    socket
                        .send(Message::binary(pack(2, &format!("{FF}{chat_no}{FF}{FF}{FF}{FF}{FF}"))))
                        .map_err(|e| e.to_string())?;
                }
                2 => {
                    joined = true;
                    sink.status(Status::Connected);
                }
                // the broadcast ended
                88 => {
                    sink.status(Status::Offline);
                    return Ok(());
                }
                _ => {
                    if let Some(e) = parse(service, &fields, &channel) {
                        sink.emit(e);
                    }
                }
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// A chat message, balloons, a subscription or a gift. `f` keeps the empty field before the
/// first separator, so indices are one more than in the libraries cited above.
pub fn parse(service: u16, f: &[String], channel: &str) -> Option<ChatEvent> {
    let at = |i: usize| f.get(i).map(String::as_str).unwrap_or("");
    // a user id with "(2)" and the like is the same person on another device
    let user = |id: usize, nick: usize| {
        let id = at(id).split('(').next().unwrap_or("").to_string();
        User { id: id.clone(), login: id, display_name: at(nick).into(), ..Default::default() }
    };
    let paid = |u: User, count: &str, what: &str, currency: &str| -> Option<ChatEvent> {
        let count: f64 = count.trim().parse().ok()?;
        let mut e = ChatEvent::new("soop", channel, Kind::Paid);
        e.headline = Some(format!("{} sent {what} ×{count}", u.display_name));
        e.user = Some(u);
        e.paid = Some(Paid { display: format!("{what} {count}개"), amount: count, currency: currency.into() });
        Some(e)
    };
    let membership = |u: User, headline: String| {
        let mut e = ChatEvent::new("soop", channel, Kind::Membership);
        e.headline = Some(headline);
        e.user = Some(u);
        e
    };
    match service {
        5 => {
            let mut e = ChatEvent::new("soop", channel, Kind::Message);
            e.user = Some(user(2, 6));
            Some(e.with_fragments(text_fragments(at(1))))
        }
        18 => paid(user(2, 3), at(4), "별풍선", "SOOP_BALLOON"),
        33 => paid(user(4, 5), at(6), "별풍선", "SOOP_BALLOON"),
        87 => paid(user(3, 4), at(10), "애드벌룬", "SOOP_ADBALLOON"),
        105 => paid(user(3, 4), at(5), "영상풍선", "SOOP_VIDEOBALLOON"),
        91 => {
            let u = user(3, 4);
            let plus = !matches!(at(8), "" | "1");
            Some(membership(u.clone(), format!("{} subscribed{}", u.display_name, if plus { " (Plus)" } else { "" })))
        }
        93 => {
            let u = user(2, 3);
            let months = at(4).parse::<u32>().unwrap_or(0);
            Some(membership(u.clone(), format!("{} subscribed for {months} months in a row", u.display_name)))
        }
        108 | 144 => {
            let u = user(2, 3);
            let to = at(5);
            Some(membership(u.clone(), format!("{} gave {to} a subscription", u.display_name)))
        }
        45 => {
            let u = user(2, 3);
            Some(membership(u.clone(), format!("{} gave {} Quick View", u.display_name, at(5))))
        }
        118 => {
            let u = user(2, 3);
            let mut e = ChatEvent::new("soop", channel, Kind::Notice);
            e.headline = Some(format!("{} gave {} an emoticon: {}", u.display_name, at(5), at(6)));
            e.user = Some(u);
            Some(e)
        }
        121 => {
            let v: serde_json::Value = serde_json::from_str(at(1)).ok()?;
            if !matches!(v["type"].as_str(), Some("GIFT" | "CHALLENGE_GIFT")) {
                return None;
            }
            let id = v["user_id"].as_str().unwrap_or("").to_string();
            let u = User { id: id.clone(), login: id, display_name: v["user_nick"].as_str().unwrap_or("").into(), ..Default::default() };
            let count = v["gift_count"].as_u64().map(|n| n.to_string()).or_else(|| v["gift_count"].as_str().map(String::from)).unwrap_or_default();
            paid(u, &count, "미션 별풍선", "SOOP_BALLOON")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip_across_frames() {
        let mut stream = pack(5, &format!("{FF}안녕하세요{FF}viewer(2){FF}0{FF}1{FF}0{FF}시청자{FF}"));
        stream.extend(pack(18, &format!("{FF}bj{FF}fan{FF}팬{FF}100{FF}")));
        // split across two frames
        let mut buf = stream[..10].to_vec();
        assert!(unpack(&mut buf).is_empty());
        buf.extend_from_slice(&stream[10..]);
        let packets = unpack(&mut buf);
        assert_eq!(packets.len(), 2);
        let chat = parse(packets[0].0, &packets[0].1, "x").unwrap();
        assert_eq!((chat.text.as_str(), chat.user.as_ref().unwrap().display_name.as_str()), ("안녕하세요", "시청자"));
        assert_eq!(chat.user.unwrap().id, "viewer");
        let balloon = parse(packets[1].0, &packets[1].1, "x").unwrap();
        assert_eq!(balloon.paid.unwrap().amount, 100.0);
    }

    #[test]
    fn subscriptions_and_other_gifts() {
        let f = |p: &str| p.split(FF).map(String::from).collect::<Vec<_>>();
        let sub = parse(91, &f(&format!("{FF}12{FF}bj{FF}fan{FF}팬{FF}SUB{FF}{FF}{FF}2")), "x").unwrap();
        assert_eq!(sub.kind, Kind::Membership);
        assert_eq!(sub.headline.as_deref(), Some("팬 subscribed (Plus)"));
        let renew = parse(93, &f(&format!("{FF}bj{FF}fan{FF}팬{FF}7{FF}12")), "x").unwrap();
        assert_eq!(renew.headline.as_deref(), Some("팬 subscribed for 7 months in a row"));
        let ad = parse(87, &f(&format!("{FF}12{FF}bj{FF}fan(2){FF}팬{FF}hi{FF}{FF}title{FF}{FF}{FF}500")), "x").unwrap();
        assert_eq!((ad.paid.as_ref().unwrap().amount, ad.user.unwrap().id.as_str()), (500.0, "fan"));
        let sub_balloon = parse(33, &f(&format!("{FF}x{FF}bj{FF}y{FF}fan{FF}팬{FF}30")), "x").unwrap();
        assert_eq!(sub_balloon.paid.unwrap().amount, 30.0);
        let mission = parse(121, &f(&format!("{FF}{{\"type\":\"GIFT\",\"user_id\":\"fan\",\"user_nick\":\"팬\",\"gift_count\":40}}")), "x").unwrap();
        assert_eq!(mission.paid.unwrap().amount, 40.0);
        assert!(parse(121, &f(&format!("{FF}{{\"type\":\"NOTICE\"}}")), "x").is_none());
    }

    #[test]
    fn a_streamer_however_typed() {
        for c in ["https://play.sooplive.co.kr/abc123/290000000", "ch.sooplive.co.kr/abc123", "abc123"] {
            assert_eq!(streamer(c), "abc123");
        }
    }
}
