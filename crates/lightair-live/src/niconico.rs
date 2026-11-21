//! Niconico Live comments, read without logging in, from the comment server Niconico moved to
//! in 2024 ("NDGR": protobuf over streamed HTTP). The watch page's embedded data names the watch
//! socket; `startWatching` there answers with the view address; that streams `ChunkedEntry`s
//! (length-prefixed protobuf) naming segments, and each segment streams `ChunkedMessage`s:
//! comments, gifts, Nicoad and notices. Field numbers from n-air-app/nicolive-comment-protobuf;
//! the flow as tsukumijima/NDGRClient does it.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;
use crate::proto::Msg;

const ORIGIN: &str = "https://live.nicovideo.jp";

/// "https://live.nicovideo.jp/watch/lv123", "lv123", "ch123", "user/123", a channel or user
/// page → what follows /watch/.
pub fn program(channel: &str) -> String {
    let c = channel.trim();
    for prefix in ["lv", "ch"] {
        if let Some(at) = c.find(prefix) {
            let digits: String = c[at + 2..].chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                return format!("{prefix}{digits}");
            }
        }
    }
    if let Some(at) = c.find("user/") {
        let digits: String = c[at + 5..].chars().take_while(char::is_ascii_digit).collect();
        return format!("user/{digits}");
    }
    if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("user/{c}");
    }
    c.to_string()
}

/// The watch page's embedded data (`<script id="embedded-data" data-props="…">`).
pub fn embedded_data(html: &str) -> Option<Value> {
    let at = html.find("id=\"embedded-data\"")?;
    let rest = &html[at..];
    let start = rest.find("data-props=\"")? + "data-props=\"".len();
    let end = rest[start..].find('"')?;
    let raw = &rest[start..start + end];
    let text = raw
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&text).ok()
}

/// Streams last longer than `net::agent`'s 20 seconds (a view response up to ~32 s, a segment
/// ~24 s).
fn stream_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(net::tls_config())
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn open(agent: &ureq::Agent, url: &str) -> Result<ureq::BodyReader<'static>, String> {
    let res = agent
        .get(url)
        .header("User-Agent", net::USER_AGENT)
        .header("Origin", ORIGIN)
        .header("Referer", "https://live.nicovideo.jp/")
        .call()
        .map_err(|e| e.to_string())?;
    if res.status() != 200 {
        return Err(format!("{url}: {}", res.status()));
    }
    Ok(res.into_body().into_reader())
}

/// The next length-prefixed message (a varint, then that many bytes); None at the end.
pub fn next_message(r: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut len: u64 = 0;
    let mut shift = 0;
    loop {
        let mut b = [0u8; 1];
        match r.read(&mut b) {
            Ok(0) if shift == 0 => return Ok(None),
            Ok(0) => return Err("cut off".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        }
        len |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err("bad length".into());
        }
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let id = program(&sink.channel().channel);
    let agent = net::agent();
    let html = net::get(&agent, &format!("https://live.nicovideo.jp/watch/{id}"), &[])?;
    let data = embedded_data(&html).ok_or("the watch page has no program data (is the address right?)")?;
    let socket_url = data["site"]["relive"]["webSocketUrl"].as_str().unwrap_or("");
    if data["program"]["status"].as_str() != Some("ON_AIR") || socket_url.is_empty() {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    }
    // the watch socket: just for the view address
    let mut socket = net::connect_with(socket_url, &[("Origin", ORIGIN)], Duration::from_millis(500))?;
    socket
        .send(Message::text(json!({"type": "startWatching", "data": {"reconnect": false}}).to_string()))
        .map_err(|e| e.to_string())?;
    let asked = Instant::now();
    let view = loop {
        if stop.load(Ordering::Acquire) || asked.elapsed() > Duration::from_secs(20) {
            let _ = socket.close(None);
            return Err("Niconico didn't say where comments are".into());
        }
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
        match v["type"].as_str() {
            Some("messageServer") => break v["data"]["viewUri"].as_str().unwrap_or("").to_string(),
            Some("ping") => socket.send(Message::text(r#"{"type":"pong"}"#)).map_err(|e| e.to_string())?,
            Some("error") | Some("disconnect") => {
                let _ = socket.close(None);
                return Err(format!("Niconico: {}", v["data"]));
            }
            _ => {}
        }
    };
    let _ = socket.close(None);
    if view.is_empty() {
        return Err("Niconico gave no comment address".into());
    }
    sink.status(Status::Connected);
    let channel = sink.channel().channel.clone();
    let done = Arc::new(AtomicBool::new(false));
    let seen_segments = Mutex::new(HashSet::<String>::new());
    let seen = Arc::new(Mutex::new(HashSet::<String>::new()));
    let stream = stream_agent();
    let mut at = "now".to_string();
    let result = loop {
        if stop.load(Ordering::Acquire) || done.load(Ordering::Acquire) {
            break Ok(());
        }
        let sep = if view.contains('?') { '&' } else { '?' };
        let mut body = match open(&stream, &format!("{view}{sep}at={at}")) {
            Ok(b) => b,
            Err(e) => break Err(e),
        };
        let mut next = None;
        loop {
            let entry = match next_message(&mut body) {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(%e, "niconico: view stream");
                    break;
                }
            };
            let m = Msg::new(&entry);
            if let Some(seg) = m.bytes(1) {
                let uri = Msg::new(seg).string(3);
                if !uri.is_empty() && seen_segments.lock().insert(uri.clone()) {
                    let (sink, seen, done, stream, channel) =
                        (sink.clone(), seen.clone(), done.clone(), stream.clone(), channel.clone());
                    let _ = std::thread::Builder::new().name("lightair-niconico-segment".into()).spawn(move || {
                        if let Err(e) = segment(&stream, &uri, &sink, &seen, &done, &channel) {
                            tracing::debug!(%e, "niconico: segment");
                        }
                    });
                }
            }
            if let Some(n) = m.bytes(4) {
                next = Some(Msg::new(n).int(1));
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
        }
        match next {
            Some(n) => at = n.to_string(),
            // no next: the program ended
            None => {
                sink.status(Status::Offline);
                break Ok(());
            }
        }
    };
    done.store(true, Ordering::Release);
    result
}

/// Reads one segment's messages as they come.
fn segment(
    agent: &ureq::Agent,
    uri: &str,
    sink: &Sink,
    seen: &Mutex<HashSet<String>>,
    done: &AtomicBool,
    channel: &str,
) -> Result<(), String> {
    let mut body = open(agent, uri)?;
    while let Some(bytes) = next_message(&mut body)? {
        if done.load(Ordering::Acquire) {
            break;
        }
        let m = Msg::new(&bytes);
        let id = m.msg(1).string(1);
        if !id.is_empty() && !seen.lock().insert(id) {
            continue;
        }
        // the program ended
        if m.msg(4).msg(9).int(1) == 1 {
            sink.status(Status::Offline);
            done.store(true, Ordering::Release);
            break;
        }
        if let Some(e) = parse(&bytes, channel) {
            sink.emit(e);
        }
    }
    Ok(())
}

/// A `ChunkedMessage` → an event, when it's one worth showing.
pub fn parse(bytes: &[u8], channel: &str) -> Option<ChatEvent> {
    let m = Msg::new(bytes);
    let meta = m.msg(1);
    let message = m.msg(2);
    let at = meta.msg(2);
    let ts = (at.int(1) as i64) * 1000 + (at.int(2) as i64) / 1_000_000;
    let mut e = if let Some(chat) =
        message.bytes(1).or(message.bytes(20)).or_else(|| message.bytes(22).and_then(|f| Msg::new(f).bytes(1)))
    {
        let c = Msg::new(chat);
        let raw = c.int(5);
        let id = if raw != 0 { raw.to_string() } else { c.string(6) };
        let name = c.string(2);
        let mut e = ChatEvent::new("niconico", channel, Kind::Message);
        e.user = Some(User {
            id: id.clone(),
            login: id.clone(),
            display_name: if name.is_empty() { id } else { name },
            ..Default::default()
        });
        e.with_fragments(text_fragments(&c.string(1)))
    } else if let Some(gift) = message.bytes(8) {
        let g = Msg::new(gift);
        let (name, point, item) = (g.string(3), g.int(4), g.string(6));
        let mut e = ChatEvent::new("niconico", channel, Kind::Paid);
        let id = g.int(2).to_string();
        e.headline = Some(format!("{name} gave {item}"));
        e.user = Some(User { id: id.clone(), login: id, display_name: name, ..Default::default() });
        e.paid = Some(Paid {
            display: format!("{item} ({point} pt)"),
            amount: point as f64,
            currency: "NICONICO_POINT".into(),
        });
        let text = g.string(5);
        if text.is_empty() { e } else { e.with_fragments(text_fragments(&text)) }
    } else if let Some(ad) = message.bytes(9) {
        let ad = Msg::new(ad);
        let mut e = ChatEvent::new("niconico", channel, Kind::Notice);
        if let Some(v1) = ad.bytes(2) {
            e.headline = Some(Msg::new(v1).string(2));
        } else {
            let latest = ad.msg(1).msg(1);
            e.headline = Some(format!("{} advertised the program ({} pt)", latest.string(1), latest.int(2)));
        }
        e
    } else if let Some(n) = message.bytes(7) {
        // ichiba, program extended, ranking in, supporter registered, level up
        let n = Msg::new(n);
        let text = [1, 5, 6, 9, 10].iter().map(|f| n.string(*f)).find(|t| !t.is_empty())?;
        let mut e = ChatEvent::new("niconico", channel, Kind::Notice);
        e.headline = Some(text);
        e
    } else {
        let n = Msg::new(message.bytes(23)?);
        let kind = match n.int(1) {
            9 => Kind::Follow,
            1 | 4 | 5 | 7 | 8 | 10 => Kind::Notice,
            _ => return None,
        };
        let mut e = ChatEvent::new("niconico", channel, kind);
        e.headline = Some(n.string(2));
        e
    };
    if ts > 0 {
        e.ts_ms = ts;
    }
    let id = meta.string(1);
    e.native_id = (!id.is_empty()).then_some(id);
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32, wire: u32) -> Vec<u8> {
        let k = (n << 3) | wire;
        if k < 0x80 { vec![k as u8] } else { vec![(k as u8 & 0x7f) | 0x80, (k >> 7) as u8] }
    }

    fn field(n: u32, body: &[u8]) -> Vec<u8> {
        let mut out = key(n, 2);
        out.push(body.len() as u8);
        out.extend_from_slice(body);
        out
    }

    fn varint(n: u32, v: u8) -> Vec<u8> {
        [key(n, 0), vec![v]].concat()
    }

    #[test]
    fn programs_however_typed() {
        assert_eq!(program("https://live.nicovideo.jp/watch/lv345678?ref=top"), "lv345678");
        assert_eq!(program("ch2646436"), "ch2646436");
        assert_eq!(program("https://www.nicovideo.jp/user/12345/live_programs"), "user/12345");
        assert_eq!(program("12345"), "user/12345");
    }

    #[test]
    fn the_watch_pages_data() {
        let html = r#"<script id="embedded-data" data-props="{&quot;program&quot;:{&quot;status&quot;:&quot;ON_AIR&quot;},&quot;site&quot;:{&quot;relive&quot;:{&quot;webSocketUrl&quot;:&quot;wss://a/b?x=1&amp;y=2&quot;}}}"></script>"#;
        let d = embedded_data(html).unwrap();
        assert_eq!(d["site"]["relive"]["webSocketUrl"], "wss://a/b?x=1&y=2");
    }

    #[test]
    fn length_prefixed_messages() {
        let mut stream: &[u8] = &[2, 8, 1, 0, 3, 9];
        assert_eq!(next_message(&mut stream).unwrap(), Some(vec![8, 1]));
        assert_eq!(next_message(&mut stream).unwrap(), Some(vec![]));
        assert!(next_message(&mut stream).is_err());
    }

    #[test]
    fn comments_gifts_and_follows() {
        // meta {id "m1", at {seconds 1}}, message {chat {content "こんにちは", name "太郎", raw_user_id 42}}
        let meta = [field(1, b"m1"), field(2, &varint(1, 1))].concat();
        let chat = [field(1, "こんにちは".as_bytes()), field(2, "太郎".as_bytes()), varint(5, 42)].concat();
        let msg = [field(1, &meta), field(2, &field(1, &chat))].concat();
        let e = parse(&msg, "lv1").unwrap();
        assert_eq!(
            (e.kind.clone(), e.text.as_str(), e.native_id.as_deref(), e.ts_ms),
            (Kind::Message, "こんにちは", Some("m1"), 1000)
        );
        assert_eq!(e.user.unwrap().display_name, "太郎");
        // an anonymous comment: the hashed id stands in for the name
        let anon = [field(1, b"hi"), field(6, b"a:xyz")].concat();
        let e = parse(&field(2, &field(20, &anon)), "lv1").unwrap();
        assert_eq!(e.user.unwrap().display_name, "a:xyz");
        let gift = [field(3, "花子".as_bytes()), varint(4, 100), field(6, "ギフト".as_bytes())].concat();
        let e = parse(&field(2, &field(8, &gift)), "lv1").unwrap();
        assert_eq!((e.kind.clone(), e.paid.unwrap().amount), (Kind::Paid, 100.0));
        let follow = [varint(1, 9), field(2, "フォローされました".as_bytes())].concat();
        assert_eq!(parse(&field(2, &field(23, &follow)), "lv1").unwrap().kind, Kind::Follow);
        // visits aren't shown
        assert!(parse(&field(2, &field(7, &field(7, b"10 visited"))), "lv1").is_none());
    }
}
