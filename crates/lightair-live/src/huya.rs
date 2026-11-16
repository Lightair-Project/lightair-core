//! Huya (虎牙) chat, read without logging in: the room's ids from Huya's profile API, then
//! Huya's danmaku socket, which speaks TARS (Tencent's binary struct encoding). Lightair joins the
//! room's groups (`live:<uid>`, `chat:<uid>`) and reads pushes: chat (1400), gifts (6501),
//! noble subscriptions (1001) and the end of the stream (8001). As PureLive, rust-srec and biliup
//! do it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;

// ---- TARS ----

/// A decoded TARS value.
#[derive(Debug, Clone, PartialEq)]
pub enum Tars {
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Tars>),
    Map(Vec<(Tars, Tars)>),
    Struct(BTreeMap<u8, Tars>),
}

impl Tars {
    pub fn int(&self) -> i64 {
        match self {
            Tars::Int(n) => *n,
            _ => 0,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Tars::Str(s) => s.clone(),
            Tars::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        }
    }

    /// Field `tag` of a struct (or Null-ish 0 / "" through `int`/`text` when absent).
    pub fn get(&self, tag: u8) -> &Tars {
        static NONE: Tars = Tars::Int(0);
        match self {
            Tars::Struct(f) => f.get(&tag).unwrap_or(&NONE),
            _ => &NONE,
        }
    }

    /// Bytes holding a struct (a `vector<byte>` field) → that struct.
    pub fn inner(&self) -> Tars {
        match self {
            Tars::Bytes(b) => decode(b).unwrap_or(Tars::Struct(BTreeMap::new())),
            other => other.clone(),
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }

    fn head(&mut self) -> Option<(u8, u8)> {
        let h = *self.take(1)?.first()?;
        let (mut tag, ty) = (h >> 4, h & 0x0f);
        if tag == 15 {
            tag = *self.take(1)?.first()?;
        }
        Some((tag, ty))
    }

    fn value(&mut self, ty: u8) -> Option<Tars> {
        Some(match ty {
            0 => Tars::Int(self.take(1)?[0] as i8 as i64),
            1 => Tars::Int(i16::from_be_bytes(self.take(2)?.try_into().ok()?) as i64),
            2 => Tars::Int(i32::from_be_bytes(self.take(4)?.try_into().ok()?) as i64),
            3 => Tars::Int(i64::from_be_bytes(self.take(8)?.try_into().ok()?)),
            4 => Tars::Float(f32::from_be_bytes(self.take(4)?.try_into().ok()?) as f64),
            5 => Tars::Float(f64::from_be_bytes(self.take(8)?.try_into().ok()?)),
            6 => {
                let n = self.take(1)?[0] as usize;
                Tars::Str(String::from_utf8_lossy(self.take(n)?).into_owned())
            }
            7 => {
                let n = u32::from_be_bytes(self.take(4)?.try_into().ok()?) as usize;
                Tars::Str(String::from_utf8_lossy(self.take(n)?).into_owned())
            }
            8 => {
                let n = self.count()?;
                let mut out = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let (_, kt) = self.head()?;
                    let k = self.value(kt)?;
                    let (_, vt) = self.head()?;
                    out.push((k, self.value(vt)?));
                }
                Tars::Map(out)
            }
            9 => {
                let n = self.count()?;
                let mut out = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let (_, t) = self.head()?;
                    out.push(self.value(t)?);
                }
                Tars::List(out)
            }
            10 => Tars::Struct(self.fields(true)?),
            12 => Tars::Int(0),
            13 => {
                self.head()?;
                let n = self.count()?;
                Tars::Bytes(self.take(n)?.to_vec())
            }
            _ => return None,
        })
    }

    /// A length or count: a TARS integer at tag 0.
    fn count(&mut self) -> Option<usize> {
        let (_, t) = self.head()?;
        usize::try_from(self.value(t)?.int()).ok()
    }

    /// Fields until a struct end (`nested`) or the end of the bytes.
    fn fields(&mut self, nested: bool) -> Option<BTreeMap<u8, Tars>> {
        let mut out = BTreeMap::new();
        while self.at < self.b.len() {
            let (tag, ty) = self.head()?;
            if ty == 11 {
                return Some(out);
            }
            out.insert(tag, self.value(ty)?);
        }
        (!nested).then_some(out)
    }
}

/// Bytes holding a struct's fields → that struct.
pub fn decode(b: &[u8]) -> Option<Tars> {
    Reader { b, at: 0 }.fields(false).map(Tars::Struct)
}

fn head(out: &mut Vec<u8>, tag: u8, ty: u8) {
    if tag < 15 {
        out.push((tag << 4) | ty);
    } else {
        out.extend([0xf0 | ty, tag]);
    }
}

fn put_int(out: &mut Vec<u8>, tag: u8, n: i64) {
    match n {
        0 => head(out, tag, 12),
        -128..=127 => {
            head(out, tag, 0);
            out.push(n as i8 as u8);
        }
        -32768..=32767 => {
            head(out, tag, 1);
            out.extend((n as i16).to_be_bytes());
        }
        -2147483648..=2147483647 => {
            head(out, tag, 2);
            out.extend((n as i32).to_be_bytes());
        }
        _ => {
            head(out, tag, 3);
            out.extend(n.to_be_bytes());
        }
    }
}

fn put_str(out: &mut Vec<u8>, tag: u8, s: &str) {
    if s.len() <= 255 {
        head(out, tag, 6);
        out.push(s.len() as u8);
    } else {
        head(out, tag, 7);
        out.extend((s.len() as u32).to_be_bytes());
    }
    out.extend(s.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, tag: u8, b: &[u8]) {
    head(out, tag, 13);
    head(out, 0, 0);
    put_int(out, 0, b.len() as i64);
    out.extend(b);
}

/// A `WebSocketCommand`: its type and body.
pub fn command(kind: i64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_int(&mut out, 0, kind);
    put_bytes(&mut out, 1, body);
    out
}

/// Joining the room's groups (`WSRegisterGroupReq`, command 16).
pub fn register(uid: &str) -> Vec<u8> {
    let mut req = Vec::new();
    head(&mut req, 0, 9);
    put_int(&mut req, 0, 2);
    put_str(&mut req, 0, &format!("live:{uid}"));
    put_str(&mut req, 0, &format!("chat:{uid}"));
    put_str(&mut req, 1, "");
    command(16, &req)
}

// ---- the reader ----

/// "https://www.huya.com/660000", "huya.com/name", "660000" → "660000" / "name".
pub fn room(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let c = c.split('?').next().unwrap_or(c);
    c.rsplit('/').next().unwrap_or(c).to_string()
}

/// The room's live state and presenter uid: (live, uid).
fn profile(agent: &ureq::Agent, room: &str) -> Result<(bool, String), String> {
    let headers = [("Origin", "https://www.huya.com"), ("Referer", "https://www.huya.com/")];
    let mut numeric = room.to_string();
    if !room.chars().all(|c| c.is_ascii_digit()) {
        // a letter alias: the page names the number
        let html = net::get(agent, &format!("https://www.huya.com/{room}"), &headers)?;
        let at = html.find("\"profileRoom\"").ok_or("Huya has no such room")?;
        numeric = html[at + 13..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    }
    let v: Value = net::get_json(agent, &format!("https://mp.huya.com/cache.php?m=Live&do=profileRoom&roomid={numeric}"), &headers)?;
    let data = &v["data"];
    let uid = match &data["profileInfo"]["uid"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return Err("Huya has no such room".into()),
    };
    Ok((data["liveStatus"].as_str() == Some("ON"), uid))
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let agent = net::agent();
    let (live, uid) = profile(&agent, &room(&sink.channel().channel))?;
    if !live {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    }
    let mut socket = net::connect("wss://cdnws.api.huya.com/", Duration::from_millis(500))?;
    socket.send(Message::binary(register(&uid))).map_err(|e| e.to_string())?;
    let channel = sink.channel().channel.clone();
    let mut beat = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if beat.elapsed() > Duration::from_secs(60) {
            socket.send(Message::binary(command(20, &[]))).map_err(|e| e.to_string())?;
            beat = Instant::now();
        }
        let Some(Message::Binary(b)) = net::read(&mut socket)? else { continue };
        let Some(cmd) = decode(&b) else { continue };
        let body = cmd.get(1).inner();
        let pushes: Vec<(i64, Tars)> = match cmd.get(0).int() {
            // registered
            17 => {
                sink.status(Status::Connected);
                continue;
            }
            7 => vec![(body.get(1).int(), body.get(2).inner())],
            22 => match body.get(1) {
                Tars::List(items) => items.iter().map(|i| (i.get(0).int(), i.get(1).inner())).collect(),
                _ => continue,
            },
            _ => continue,
        };
        for (uri, msg) in pushes {
            if uri == 8001 {
                sink.status(Status::Offline);
                let _ = socket.close(None);
                return Ok(());
            }
            if let Some(e) = parse(uri, &msg, &channel) {
                sink.emit(e);
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// A push → an event: chat (1400), a gift (6501), a noble subscription (1001).
pub fn parse(uri: i64, m: &Tars, channel: &str) -> Option<ChatEvent> {
    let user = |id: i64, name: String| User { id: id.to_string(), login: id.to_string(), display_name: name, ..Default::default() };
    match uri {
        1400 => {
            let sender = m.get(0);
            let mut e = ChatEvent::new("huya", channel, Kind::Message);
            e.user = Some(user(sender.get(0).int(), sender.get(2).text()));
            Some(e.with_fragments(text_fragments(&m.get(3).text())))
        }
        6501 => {
            let (item, count) = (m.get(0).int(), m.get(2).int().max(1));
            let name = m.get(20).text();
            let name = if name.is_empty() { format!("gift {item}") } else { name };
            // 虎粮 (4) is free: a notice, not money
            let mut e = ChatEvent::new("huya", channel, if item == 4 { Kind::Notice } else { Kind::Paid });
            let u = user(m.get(4).int(), m.get(6).text());
            e.headline = Some(format!("{} sent {name} ×{count}", u.display_name));
            e.user = Some(u);
            if item != 4 {
                e.paid = Some(Paid { display: format!("{name} ×{count}"), amount: count as f64, currency: format!("HUYA_GIFT_{item}") });
            }
            Some(e)
        }
        1001 => {
            let noble = m.get(0);
            let mut e = ChatEvent::new("huya", channel, Kind::Membership);
            let u = user(noble.get(0).int(), noble.get(1).text());
            let months = noble.get(13).int();
            e.headline = Some(format!("{} became {}{}", u.display_name, noble.get(3).text(), if months > 0 { format!(" ({months} months)") } else { String::new() }));
            e.user = Some(u);
            Some(e)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tars_struct(fields: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(fields);
        out
    }

    #[test]
    fn the_heartbeat_is_what_huya_expects() {
        assert_eq!(command(20, &[]), [0x00, 0x14, 0x1d, 0x00, 0x0c]);
    }

    #[test]
    fn registering_round_trips() {
        let cmd = decode(&register("1234567890")).unwrap();
        assert_eq!(cmd.get(0).int(), 16);
        let req = cmd.get(1).inner();
        assert_eq!(req.get(0), &Tars::List(vec![Tars::Str("live:1234567890".into()), Tars::Str("chat:1234567890".into())]));
    }

    #[test]
    fn chat_and_gifts_parse() {
        // MessageNotice { 0: SenderInfo { 0: uid, 2: nick }, 3: content }
        let mut sender = Vec::new();
        put_int(&mut sender, 0, 9_000_000_000);
        put_str(&mut sender, 2, "小虎");
        let mut msg = Vec::new();
        head(&mut msg, 0, 10);
        msg.extend(&sender);
        head(&mut msg, 0, 11);
        put_int(&mut msg, 1, 0);
        put_str(&mut msg, 3, "你好");
        let e = parse(1400, &decode(&tars_struct(&msg)).unwrap(), "660000").unwrap();
        let u = e.user.clone().unwrap();
        assert_eq!((e.text.as_str(), u.display_name.as_str(), u.id.as_str()), ("你好", "小虎", "9000000000"));
        // a push on command 7 wraps it in bytes
        let mut push = Vec::new();
        put_int(&mut push, 1, 1400);
        put_bytes(&mut push, 2, &msg);
        let cmd = decode(&command(7, &push)).unwrap();
        let body = cmd.get(1).inner();
        assert_eq!(parse(body.get(1).int(), &body.get(2).inner(), "x").unwrap().text, "你好");
        // a gift: 0 item, 2 count, 4 sender uid, 6 sender nick, 20 name
        let mut gift = Vec::new();
        put_int(&mut gift, 0, 20277);
        put_int(&mut gift, 2, 3);
        put_int(&mut gift, 4, 42);
        put_str(&mut gift, 6, "粉丝");
        put_str(&mut gift, 20, "超级火箭");
        let e = parse(6501, &decode(&gift).unwrap(), "x").unwrap();
        assert_eq!((e.kind.clone(), e.paid.unwrap().amount, e.headline.unwrap().as_str()), (Kind::Paid, 3.0, "粉丝 sent 超级火箭 ×3"));
    }

    #[test]
    fn rooms_however_typed() {
        assert_eq!(room("https://www.huya.com/660000"), "660000");
        assert_eq!(room("huya.com/lpl?from=x"), "lpl");
        assert_eq!(room("660000"), "660000");
    }
}
