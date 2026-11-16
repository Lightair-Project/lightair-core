//! TikTok LIVE chat and gifts, read without logging in. TikTok's own live connection needs
//! signatures only its web page can make, so a hidden page ([`crate::pages`]) opens the live
//! room. A script run as the page starts listens in on the page's own connection and hands its
//! frames over; they're decoded here (TikTok's protobuf: comments, gifts with their combos,
//! follows, shares). As a fallback, the comments the page draws are read too (each row's
//! `chat-message` mark) until frames arrive. The room's video is kept paused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::event::{ChatEvent, Fragment, Kind, Paid, Status, User};
use crate::proto::Msg;
use crate::hub::{Sink, sleep_unless};
use crate::pages;

/// Runs as the page starts: the page's live connection's frames, handed over (base64).
const EARLY: &str = r#"(function () {
  if (window.__lightairWS) return;
  window.__lightairWS = true;
  var Native = window.WebSocket;
  function b64(buf) {
    var bytes = new Uint8Array(buf), out = '';
    for (var i = 0; i < bytes.length; i += 0x8000) out += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(out);
  }
  function Hooked(url, protocols) {
    var ws = protocols === undefined ? new Native(url) : new Native(url, protocols);
    if (/webcast/.test(String(url))) {
      ws.addEventListener('message', function (ev) {
        var d = ev.data;
        if (d instanceof ArrayBuffer) lightair('ws:' + b64(d));
        else if (d && d.arrayBuffer) d.arrayBuffer().then(function (a) { lightair('ws:' + b64(a)); });
      });
    }
    return ws;
  }
  Hooked.prototype = Native.prototype;
  ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function (k) { Hooked[k] = Native[k]; });
  window.WebSocket = Hooked;
})();"#;

/// Runs in the live page after each load.
const SCRIPT: &str = r#"(function () {
  if (window.__lightairChat) return;
  window.__lightairChat = true;
  var since = Date.now(), live = false;
  function frags(el) {
    var out = [];
    el.childNodes.forEach(function (n) {
      if (n.nodeType === 3) out.push({ t: n.textContent });
      else if (n.tagName === 'IMG') {
        var alt = n.getAttribute('alt') || '';
        out.push(/^https?:/.test(n.src) ? { e: alt, src: n.src } : { t: alt });
      } else if (n.nodeType === 1) out = out.concat(frags(n));
    });
    return out;
  }
  function scan() {
    document.querySelectorAll('video').forEach(function (v) { if (!v.paused) { v.muted = true; v.pause(); } });
    var c = document.querySelector('[data-e2e="live-chat-container"]');
    if (!c) {
      if (Date.now() - since > 30000) { lightair('{"status":"offline"}'); since = Date.now(); live = false; }
      return;
    }
    since = Date.now();
    if (!live) { live = true; lightair('{"status":"live"}'); }
    c.querySelectorAll('[data-e2e="chat-message"]').forEach(function (row) {
      var name = row.querySelector('[data-e2e="message-owner-name"]');
      var head = name && name.parentElement && name.parentElement.parentElement;
      var body = head && head.nextElementSibling;
      if (!body) return;
      var user = name.getAttribute('title') || name.textContent;
      var sig = user + '\u0000' + body.textContent;
      if (row.getAttribute('data-lightair') === sig) return;
      row.setAttribute('data-lightair', sig);
      lightair(JSON.stringify({ user: user, f: frags(body) }));
    });
  }
  setInterval(scan, 400);
})();"#;

/// "https://www.tiktok.com/@name/live", "@name", "name" → "name".
pub fn handle(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let c = c.split('?').next().unwrap_or(c);
    let name = match c.find('@') {
        Some(at) => &c[at + 1..],
        None => c.rsplit('/').next().unwrap_or(c),
    };
    name.split('/').next().unwrap_or("").to_string()
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let name = handle(&sink.channel().channel);
    let host = pages::host()?;
    let (tx, rx) = mpsc::channel::<String>();
    let tx = parking_lot::Mutex::new(tx);
    let script = format!("{}{SCRIPT}", pages::PRELUDE);
    let early = format!("{}{EARLY}", pages::PRELUDE);
    let page = host.open_early(&format!("https://www.tiktok.com/@{name}/live"), &early, &script, Arc::new(move |m| {
        let _ = tx.lock().send(m);
    }));
    let channel = sink.channel().channel.clone();
    // once the connection's frames come through, the drawn comments aren't needed
    let mut frames = false;
    while !stop.load(Ordering::Acquire) {
        let text = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(t) => t,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(b64) = text.strip_prefix("ws:") {
            use base64::Engine;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                let events = decode(&bytes, &channel);
                if !frames && events.iter().any(|e| e.kind == Kind::Message) {
                    frames = true;
                }
                for e in events {
                    sink.emit(e);
                }
            }
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        match v["status"].as_str() {
            Some("live") => sink.status(Status::Connected),
            Some("offline") => {
                // not live (or the room ended): look again in a minute, with the page closed
                sink.status(Status::Offline);
                drop(page);
                sleep_unless(stop, Duration::from_secs(60));
                return Ok(());
            }
            _ => {
                if !frames && let Some(e) = parse(&v, &channel) {
                    sink.emit(e);
                }
            }
        }
    }
    drop(page);
    Ok(())
}

fn person(m: &Msg) -> User {
    let nick = m.string(3);
    let unique = m.string(38);
    User { id: m.int(1).to_string(), login: if unique.is_empty() { nick.clone() } else { unique }, display_name: nick, ..Default::default() }
}

/// One frame of the page's live connection: its comments, gifts (a combo once, when it ends),
/// follows and shares.
pub fn decode(frame: &[u8], channel: &str) -> Vec<ChatEvent> {
    let f = Msg::new(frame);
    let Some(payload) = f.bytes(8) else { return Vec::new() };
    if !matches!(f.string(7).as_str(), "msg" | "") {
        return Vec::new();
    }
    let data = if payload.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        if std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(payload), &mut out).is_err() {
            return Vec::new();
        }
        out
    } else {
        payload.to_vec()
    };
    let response = Msg::new(&data);
    let mut out = Vec::new();
    for m in response.all_bytes(1) {
        let m = Msg::new(m);
        let Some(body) = m.bytes(2) else { continue };
        let b = Msg::new(body);
        let id = b.msg(1).int(2);
        let mut e = match m.string(1).as_str() {
            "WebcastChatMessage" => {
                let mut e = ChatEvent::new("tiktok", channel, Kind::Message);
                e.user = Some(person(&b.msg(2)));
                e.with_fragments(crate::event::text_fragments(&b.string(3)))
            }
            "WebcastGiftMessage" => {
                let details = b.msg(15);
                // a combo gift's running count: only its last
                if details.int(11) == 1 && b.int(9) == 0 {
                    continue;
                }
                let repeat = b.int(5).max(1);
                let gift = details.string(16);
                let gift = if gift.is_empty() { "a gift".to_string() } else { gift };
                let coins = details.int(12) * repeat;
                let mut e = ChatEvent::new("tiktok", channel, Kind::Paid);
                let u = person(&b.msg(7));
                e.headline = Some(format!("{} sent {gift} ×{repeat}", u.display_name));
                e.user = Some(u);
                e.paid = Some(Paid { display: format!("{gift} ×{repeat} ({coins} coin{})", if coins == 1 { "" } else { "s" }), amount: coins as f64, currency: "TIKTOK_COIN".into() });
                e
            }
            "WebcastSocialMessage" => {
                let kind = b.msg(1).msg(8).string(1);
                let u = person(&b.msg(2));
                if kind.contains("follow") {
                    let mut e = ChatEvent::new("tiktok", channel, Kind::Follow);
                    e.user = Some(u);
                    e
                } else if kind.contains("share") {
                    let mut e = ChatEvent::new("tiktok", channel, Kind::Notice);
                    e.headline = Some(format!("{} shared the LIVE", u.display_name));
                    e.user = Some(u);
                    e
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        if id > 0 {
            e.native_id = Some(id.to_string());
        }
        out.push(e);
    }
    out
}

/// A comment as the page's script sent it: `{"user", "f": [{"t"} | {"e", "src"}]}`.
pub fn parse(v: &Value, channel: &str) -> Option<ChatEvent> {
    let user = v["user"].as_str()?.to_string();
    let mut fragments: Vec<Fragment> = Vec::new();
    for f in v["f"].as_array()? {
        if let (Some(code), Some(src)) = (f["e"].as_str(), f["src"].as_str()) {
            fragments.push(Fragment::Emote { provider: "tiktok".into(), id: code.into(), code: code.into(), image: src.into() });
        } else if let Some(t) = f["t"].as_str() {
            match fragments.last_mut() {
                Some(Fragment::Text { text }) => text.push_str(t),
                _ => fragments.push(Fragment::Text { text: t.into() }),
            }
        }
    }
    if fragments.is_empty() {
        return None;
    }
    let mut e = ChatEvent::new("tiktok", channel, Kind::Message);
    e.user = Some(User { id: user.clone(), login: user.clone(), display_name: user, ..Default::default() });
    Some(e.with_fragments(fragments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leb(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// A protobuf field: (number, wire type 2, bytes).
    fn field(n: u32, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        leb(u64::from((n << 3) | 2), &mut out);
        leb(bytes.len() as u64, &mut out);
        out.extend_from_slice(bytes);
        out
    }

    fn varint(n: u32, v: u8) -> Vec<u8> {
        let mut out = Vec::new();
        leb(u64::from(n << 3), &mut out);
        leb(u64::from(v), &mut out);
        out
    }

    #[test]
    fn frames_with_gifts_and_comments() {
        let user = [field(3, b"Fan"), field(38, b"fan_1")].concat();
        let chat = [field(2, &user), field(3, b"hello")].concat();
        // a Rose ×5, combo ended
        let details = [field(16, b"Rose"), varint(12, 1), varint(11, 1)].concat();
        let gift = [varint(5, 5), field(7, &user), varint(9, 1), field(15, &details)].concat();
        let streaking = [varint(5, 3), field(7, &user), varint(9, 0), field(15, &details)].concat();
        let msg = |name: &[u8], body: &[u8]| field(1, &[field(1, name), field(2, body)].concat());
        let response = [msg(b"WebcastChatMessage", &chat), msg(b"WebcastGiftMessage", &gift), msg(b"WebcastGiftMessage", &streaking)].concat();
        let frame = [field(7, b"msg"), field(8, &response)].concat();
        let events = decode(&frame, "x");
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].text.as_str(), events[0].name()), ("hello", "Fan"));
        assert_eq!(events[1].paid.as_ref().unwrap().amount, 5.0);
        assert_eq!(events[1].headline.as_deref(), Some("Fan sent Rose ×5"));
    }

    #[test]
    fn comments_with_stickers() {
        let v: Value = serde_json::from_str(r#"{"user":"fan_1","f":[{"t":"hi "},{"t":"there "},{"e":"[wow]","src":"https://p16.tiktokcdn.com/wow.png"}]}"#).unwrap();
        let e = parse(&v, "creator").unwrap();
        assert_eq!(e.text, "hi there [wow]");
        assert_eq!(e.fragments.len(), 2);
        assert_eq!(e.user.unwrap().login, "fan_1");
        for c in ["https://www.tiktok.com/@creator/live?lang=en", "@creator", "creator", "tiktok.com/@creator"] {
            assert_eq!(handle(c), "creator");
        }
    }
}
