//! SHOWROOM chat, read without logging in: the room's status (its broadcast key), then
//! SHOWROOM's message socket (`SUB\t<key>`; messages come as `MSG\t<key>\t<json>`): comments
//! (t 1) and gifts (t 2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;

/// "https://www.showroom-live.com/r/room_key", "room_key" → "room_key".
pub fn room(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let c = c.split('?').next().unwrap_or(c);
    c.rsplit('/').next().unwrap_or(c).to_string()
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let agent = net::agent();
    let status = net::get_json(
        &agent,
        &format!("https://www.showroom-live.com/api/room/status?room_url_key={}", room(&sink.channel().channel)),
        &[],
    )?;
    let (true, Some(key)) = (status["is_live"].as_bool() == Some(true), status["broadcast_key"].as_str()) else {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    };
    let host = status["broadcast_host"].as_str().unwrap_or("online.showroom-live.com");
    let mut socket = net::connect(&format!("wss://{host}/"), Duration::from_millis(500))?;
    socket.send(Message::text(format!("SUB\t{key}"))).map_err(|e| e.to_string())?;
    sink.status(Status::Connected);
    let channel = sink.channel().channel.clone();
    let mut ping = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if ping.elapsed() > Duration::from_secs(60) {
            socket.send(Message::text("PING\tshowroom")).map_err(|e| e.to_string())?;
            ping = Instant::now();
        }
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        let mut parts = t.as_str().splitn(3, '\t');
        if parts.next() != Some("MSG") {
            continue;
        }
        let Some(json) = parts.nth(1) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(json) else { continue };
        if v["t"].as_i64() == Some(101) {
            // the broadcast ended
            sink.status(Status::Offline);
            return Ok(());
        }
        if let Some(e) = parse(&v, &channel) {
            sink.emit(e);
        }
    }
    let _ = socket.close(None);
    Ok(())
}

pub fn parse(v: &Value, channel: &str) -> Option<ChatEvent> {
    let user = || {
        let name = v["ac"].as_str().unwrap_or("").to_string();
        User {
            id: v["u"].as_u64().map(|u| u.to_string()).unwrap_or_default(),
            login: name.clone(),
            display_name: name,
            ..Default::default()
        }
    };
    // t is a number or a string
    let t = v["t"].as_i64().or_else(|| v["t"].as_str().and_then(|s| s.parse().ok()))?;
    match t {
        1 => {
            let text = v["cm"].as_str().unwrap_or("");
            // counting comments ("1", "2", … "50") are SHOWROOM's way of saying hello: kept
            let mut e = ChatEvent::new("showroom", channel, Kind::Message);
            e.user = Some(user());
            e.ts_ms = v["created_at"].as_i64().map_or(e.ts_ms, |s| s * 1000);
            Some(e.with_fragments(text_fragments(text)))
        }
        2 => {
            let n = v["n"].as_u64().unwrap_or(1);
            let gift = v["g"].as_u64().unwrap_or(0);
            // gift type 2: paid (bought with points); the rest are free stars and seeds
            let paid = v["gt"].as_i64() == Some(2);
            let mut e = ChatEvent::new("showroom", channel, if paid { Kind::Paid } else { Kind::Notice });
            let u = user();
            e.headline = Some(format!("{} sent a gift ×{n}", u.display_name));
            e.user = Some(u);
            e.paid = Some(Paid {
                display: format!("gift ×{n}"),
                amount: n as f64,
                currency: format!("SHOWROOM_GIFT_{gift}"),
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
    fn comments_and_gifts() {
        let c: Value =
            serde_json::from_str(r#"{"t":1,"u":95,"ac":"ファン","cm":"キラキラ✨","created_at":1794842601}"#).unwrap();
        let e = parse(&c, "x").unwrap();
        assert_eq!((e.text.as_str(), e.name()), ("キラキラ✨", "ファン"));
        let g: Value = serde_json::from_str(r#"{"t":"2","u":95,"ac":"A","g":3000421,"gt":2,"n":100}"#).unwrap();
        assert_eq!(parse(&g, "x").unwrap().kind, Kind::Paid);
        assert_eq!(room("https://www.showroom-live.com/r/kogachan"), "kogachan");
    }
}

#[cfg(test)]
mod live_check {
    /// With LIGHTAIR_SHOWROOM_ROOM set to a live room: its socket answers.
    #[test]
    fn socket() {
        let Ok(room) = std::env::var("LIGHTAIR_SHOWROOM_ROOM") else { return };
        let agent = crate::net::agent();
        let status = crate::net::get_json(
            &agent,
            &format!("https://www.showroom-live.com/api/room/status?room_url_key={room}"),
            &[],
        )
        .unwrap();
        let key = status["broadcast_key"].as_str().unwrap().to_string();
        eprintln!("key {key:?} host {:?}", status["broadcast_host"]);
        let mut socket =
            crate::net::connect("wss://online.showroom-live.com/", std::time::Duration::from_millis(500)).unwrap();
        socket.send(tungstenite::Message::text(format!("SUB\t{key}"))).unwrap();
        for _ in 0..40 {
            if let Some(m) = crate::net::read(&mut socket).unwrap() {
                eprintln!("got {}", m.to_string().chars().take(120).collect::<String>());
            }
        }
    }
}
