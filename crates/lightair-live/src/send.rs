//! Sending chat as the person signed in (accounts): Twitch through its chat server with the
//! session's token; YouTube, Kick and TikTok by typing into their own chat page, hidden and
//! signed in (kept open a couple of minutes after the last message, then closed).

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tungstenite::Message;

use crate::{accounts, net, pages};

/// A chat page kept for sending: closed after this long unused.
const KEEP: Duration = Duration::from_secs(120);

/// Sends `text` to `channel`'s chat on `platform` (replying to `reply_to` where the platform
/// can), as the person signed in there.
pub fn send(platform: &str, channel: &str, text: &str, reply_to: Option<&str>) -> Result<(), String> {
    let text = text.replace(['\r', '\n'], " ");
    if text.trim().is_empty() {
        return Err("nothing to send".into());
    }
    if !crate::rules::enabled(platform) {
        return Err(format!("Lightair turned {platform} off while it's fixed; it comes back on its own"));
    }
    let rule = crate::rules::text;
    match platform {
        "twitch" => twitch(channel, &text, reply_to),
        "youtube" | "youtube-shorts" => {
            let video = crate::youtube::video_of(channel)?.ok_or("the channel isn't live")?;
            page_send(platform, &format!("https://www.youtube.com/live_chat?is_popout=1&v={video}"), &rule("youtube.send_input", "#input[contenteditable]"), &rule("youtube.send_button", "#send-button button"), &text)
        }
        "kick" => page_send(platform, &format!("https://kick.com/{}", crate::kick::slug(channel)), &rule("kick.send_input", "[data-testid=\"chat-input\"]"), &rule("kick.send_button", "#send-message-button"), &text),
        "tiktok" => page_send(
            platform,
            &format!("https://www.tiktok.com/@{}/live", crate::tiktok::handle(channel)),
            &rule("tiktok.send_input", "[data-e2e=\"room-chat-input-field\"]"),
            &rule("tiktok.send_button", "[data-e2e=\"room-chat-send-btn\"]"),
            &text,
        ),
        other => Err(format!("sending to {other} chat isn't supported yet")),
    }
}

/// Signs in to Twitch's chat server as the signed-in person and joins `channel`, sending
/// nothing: what Twitch answered (a check).
pub fn check_twitch(channel: &str) -> Result<String, String> {
    let token = pages::cookie("https://www.twitch.tv", "auth-token").ok_or("no Twitch sign-in")?;
    let nick = pages::cookie("https://www.twitch.tv", "login").unwrap_or_else(|| "lightair".into());
    let mut socket = net::connect("wss://irc-ws.chat.twitch.tv:443", Duration::from_millis(500))?;
    for line in [format!("PASS oauth:{token}"), format!("NICK {nick}"), format!("JOIN #{}", crate::twitch::login(channel))] {
        socket.send(Message::text(line)).map_err(|e| e.to_string())?;
    }
    let mut heard = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(6) {
        if let Some(Message::Text(t)) = net::read(&mut socket)? {
            heard.extend(t.lines().map(|l| l.chars().take(120).collect::<String>()));
        }
    }
    let _ = socket.close(None);
    Ok(format!("as {nick}: {}", heard.join(" | ")))
}

fn twitch(channel: &str, text: &str, reply_to: Option<&str>) -> Result<(), String> {
    let token = pages::cookie("https://www.twitch.tv", "auth-token").ok_or("sign in to Twitch first (Outputs › the destination › Sign in)")?;
    let nick = pages::cookie("https://www.twitch.tv", "login").unwrap_or_else(|| "lightair".into());
    let mut socket = net::connect("wss://irc-ws.chat.twitch.tv:443", Duration::from_millis(500))?;
    for line in [format!("PASS oauth:{token}"), format!("NICK {nick}")] {
        socket.send(Message::text(line)).map_err(|e| e.to_string())?;
    }
    // wait for the welcome (or a refusal)
    let started = Instant::now();
    loop {
        if started.elapsed() > Duration::from_secs(8) {
            return Err("Twitch didn't answer".into());
        }
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        if t.contains(" 001 ") {
            break;
        }
        if t.contains("Login authentication failed") || t.contains("Improperly formatted auth") {
            return Err("Twitch refused the sign-in: sign in again".into());
        }
    }
    // Twitch drops messages to a channel not joined: join, and wait until it says so
    let room = crate::twitch::login(channel);
    socket.send(Message::text("CAP REQ :twitch.tv/commands twitch.tv/tags")).map_err(|e| e.to_string())?;
    socket.send(Message::text(format!("JOIN #{room}"))).map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        if started.elapsed() > Duration::from_secs(8) {
            return Err("Twitch didn't let Lightair into the chat".into());
        }
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        // the end of the names list, or the room's state: joined
        if t.contains(" 366 ") || t.contains(&format!("ROOMSTATE #{room}")) {
            break;
        }
    }
    let tags = reply_to.map(|id| format!("@reply-parent-msg-id={id} ")).unwrap_or_default();
    socket.send(Message::text(format!("{tags}PRIVMSG #{room} :{text}"))).map_err(|e| e.to_string())?;
    // a refusal comes back as a NOTICE (too fast, followers-only, slow mode, banned…)
    let started = Instant::now();
    let mut refused = None;
    while started.elapsed() < Duration::from_millis(1200) {
        if let Some(Message::Text(t)) = net::read(&mut socket)? {
            for line in t.lines().filter(|l| l.contains(" NOTICE #")) {
                if let Some(msg) = line.rsplit_once(" :").map(|(_, m)| m.to_string()) {
                    refused = Some(msg);
                }
            }
        }
        if refused.is_some() {
            break;
        }
    }
    let _ = socket.close(None);
    let _ = net::read(&mut socket);
    match refused {
        Some(why) => Err(format!("Twitch: {why}")),
        None => Ok(()),
    }
}

struct ChatPage {
    page: Box<dyn pages::Page>,
    /// Messages from the page ("ready", "sent:<n>:<result>").
    rx: Arc<Mutex<mpsc::Receiver<String>>>,
    ready: bool,
    used: Instant,
}

static PAGES: OnceLock<Mutex<HashMap<String, ChatPage>>> = OnceLock::new();
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn reaper() {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(15));
            if let Some(p) = PAGES.get() {
                p.lock().retain(|_, c| c.used.elapsed() < KEEP);
            }
        }
    });
}

pub(crate) fn page_send(platform: &str, url: &str, input: &str, button: &str, text: &str) -> Result<(), String> {
    if !accounts::signed_in(platform) {
        return Err(format!("sign in to {platform} first (Outputs › the destination › Sign in)"));
    }
    let pages_map = PAGES.get_or_init(|| {
        reaper();
        Mutex::new(HashMap::new())
    });
    let script = format!(
        r#"(function () {{
  if (window.__lightairSend) return;
  var INPUT = {input:?}, BUTTON = {button:?};
  window.__lightairSend = async function (n, text) {{
    var el = document.querySelector(INPUT);
    if (!el) return lightair('sent:' + n + ':no chat box here (signed in? chat open?)');
    el.focus();
    if (el.isContentEditable) {{ document.execCommand('selectAll', false); document.execCommand('insertText', false, text); }}
    else {{ var set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value').set; set.call(el, text); el.dispatchEvent(new Event('input', {{ bubbles: true }})); }}
    await new Promise(function (r) {{ setTimeout(r, 250); }});
    var b = document.querySelector(BUTTON);
    if (b && !b.disabled) b.click();
    else el.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true }}));
    lightair('sent:' + n + ':ok');
  }};
  var t = setInterval(function () {{ if (document.querySelector(INPUT)) {{ clearInterval(t); lightair('ready'); }} }}, 500);
}})();"#
    );
    let mut map = pages_map.lock();
    let entry = match map.get_mut(url) {
        Some(e) => e,
        None => {
            let (tx, rx) = mpsc::channel::<String>();
            let tx = Mutex::new(tx);
            let page = pages::host()?.open(url, &format!("{}{script}", pages::PRELUDE), Arc::new(move |m| {
                let _ = tx.lock().send(m);
            }));
            map.insert(url.to_string(), ChatPage { page, rx: Arc::new(Mutex::new(rx)), ready: false, used: Instant::now() });
            map.get_mut(url).unwrap()
        }
    };
    entry.used = Instant::now();
    let rx = entry.rx.clone();
    let rx = rx.lock();
    if !entry.ready {
        let started = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(m) if m == "ready" => break,
                Ok(_) => {}
                Err(_) if started.elapsed() > Duration::from_secs(30) => {
                    map.remove(url);
                    return Err(format!("{platform}'s chat box didn't appear (signed in, and chat open?)"));
                }
                Err(_) => {}
            }
        }
        entry.ready = true;
    }
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    entry.page.eval(&format!("window.__lightairSend({n}, {})", serde_json::to_string(text).unwrap_or_default()));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if let Ok(m) = rx.recv_timeout(Duration::from_millis(250))
            && let Some(rest) = m.strip_prefix(&format!("sent:{n}:"))
        {
            return if rest == "ok" { Ok(()) } else { Err(rest.to_string()) };
        }
    }
    Err(format!("{platform}'s chat page didn't answer"))
}
