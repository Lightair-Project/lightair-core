//! TikTok LIVE chat, read without logging in. TikTok's own chat connection needs signatures
//! only its web page can make, so a hidden page ([`crate::pages`]) opens the live room and its
//! script reads the comments as the page draws them (each row's `chat-message` mark, the
//! name's `message-owner-name`), sending each new one. The room's video is kept paused: only
//! the chat is wanted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::event::{ChatEvent, Fragment, Kind, Status, User};
use crate::hub::{Sink, sleep_unless};
use crate::pages;

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
    let page = host.open(&format!("https://www.tiktok.com/@{name}/live"), &script, Arc::new(move |m| {
        let _ = tx.lock().send(m);
    }));
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) {
        let text = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(t) => t,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
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
                if let Some(e) = parse(&v, &channel) {
                    sink.emit(e);
                }
            }
        }
    }
    drop(page);
    Ok(())
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
