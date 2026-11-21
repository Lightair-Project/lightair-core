//! Twitch chat, read anonymously (no login, no app registration): Twitch's chat over a
//! WebSocket as a guest (`justinfan…`), with tags (emotes, badges, colours, bits, replies) and
//! commands (subs, raids, deletions, timeouts). Sending needs an account (EventSub and Helix with
//! the user's token: docs/plans/live-integrations.md, 2.1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::Message;

use crate::event::{Badge, ChatEvent, Fragment, Kind, Paid, ReplyTo, Role, User, text_fragments};
use crate::hub::Sink;
use crate::net;

const SERVER: &str = "wss://irc-ws.chat.twitch.tv:443";

/// "https://www.twitch.tv/Ninja", "#ninja", "Ninja" → "ninja".
pub fn login(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let c = c.rsplit('/').next().unwrap_or(c);
    c.trim_start_matches('#').trim_start_matches('@').to_lowercase()
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let login = login(&sink.channel().channel);
    if login.is_empty() {
        return Err("no channel".into());
    }
    let mut socket = net::connect(SERVER, Duration::from_millis(500))?;
    let nick = format!("justinfan{}", 10_000 + (crate::event::now_ms() % 80_000));
    for line in [
        "CAP REQ :twitch.tv/tags twitch.tv/commands".to_string(),
        "PASS SCHMOOPIIE".to_string(),
        format!("NICK {nick}"),
        format!("JOIN #{login}"),
    ] {
        socket.send(Message::text(line)).map_err(|e| e.to_string())?;
    }
    let mut heard = Instant::now();
    // follows: Twitch's chat doesn't carry them; its site's public count does (checked now and
    // then on a thread of its own, so a slow answer never holds up chat)
    let _watching = {
        let (sink, login, stop_flag) = (sink.clone(), login.clone(), std::sync::Arc::new(AtomicBool::new(false)));
        let watch = stop_flag.clone();
        let _ = std::thread::Builder::new()
            .name("lightair-twitch-follows".into())
            .spawn(move || follows(&sink, &login, &watch));
        StopOnDrop(stop_flag)
    };
    // the channel's 7TV, FFZ and BTTV emotes, loaded once Twitch says which room this is
    let emotes: std::sync::Arc<parking_lot::Mutex<crate::emotes::Emotes>> = Default::default();
    while !stop.load(Ordering::Acquire) {
        let Some(message) = net::read(&mut socket)? else {
            // Twitch pings every ~5 minutes; nothing at all for 7 means the connection is gone
            if heard.elapsed() > Duration::from_secs(420) {
                return Err("no word from Twitch".into());
            }
            continue;
        };
        heard = Instant::now();
        let Message::Text(text) = message else { continue };
        for line in text.split("\r\n").filter(|l| !l.is_empty()) {
            if let Some(rest) = line.strip_prefix("PING") {
                socket.send(Message::text(format!("PONG{rest}"))).map_err(|e| e.to_string())?;
                continue;
            }
            match parse(line, &sink.channel().channel) {
                Parsed::Event(mut e) => {
                    let third = emotes.lock();
                    if !third.is_empty() {
                        e.fragments = third.apply(std::mem::take(&mut e.fragments));
                    }
                    drop(third);
                    sink.emit(*e)
                }
                Parsed::Joined(room) => {
                    sink.status(crate::event::Status::Connected);
                    if let Some(room) = room
                        && emotes.lock().is_empty()
                    {
                        let emotes = emotes.clone();
                        let _ = std::thread::Builder::new()
                            .name("lightair-emotes".into())
                            .spawn(move || *emotes.lock() = crate::emotes::Emotes::load(&room));
                    }
                }
                Parsed::Reconnect => return Ok(()),
                Parsed::Nothing => {}
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// Stops the follow watcher however its reader ends.
struct StopOnDrop(std::sync::Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Twitch's own site's client: public questions (counts) need nothing else.
const GQL_CLIENT: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

fn gql(query: &str, token: Option<&str>) -> Result<serde_json::Value, String> {
    let agent = net::agent();
    let mut req = agent
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", GQL_CLIENT)
        .header("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.header("Authorization", &format!("OAuth {t}"));
    }
    let mut res = req.send(serde_json::json!({"query": query}).to_string()).map_err(|e| e.to_string())?;
    serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// New followers: the public count, checked every 20 s; names when the channel's own session
/// (signed in to Twitch in Lightair) may see them, else "Someone".
fn follows(sink: &Sink, login: &str, stop: &AtomicBool) {
    let count_q = format!("query{{user(login:\"{login}\"){{followers(first:1){{totalCount}}}}}}");
    let names_q =
        format!("query{{user(login:\"{login}\"){{followers(first:20){{edges{{node{{id login displayName}}}}}}}}}}");
    let count = |v: &serde_json::Value| v["data"]["user"]["followers"]["totalCount"].as_u64();
    let mut last = gql(&count_q, None).ok().and_then(|v| count(&v));
    let token = || crate::pages::cookie("https://www.twitch.tv", "auth-token");
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(t) = token()
        && let Ok(v) = gql(&names_q, Some(&t))
    {
        known.extend(
            v["data"]["user"]["followers"]["edges"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["node"]["id"].as_str().map(String::from)),
        );
    }
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) && crate::hub::sleep_unless(stop, Duration::from_secs(20)) {
        let Some(now) = gql(&count_q, None).ok().and_then(|v| count(&v)) else { continue };
        let Some(before) = last.replace(now) else { continue };
        if now <= before {
            continue;
        }
        let mut named = Vec::new();
        if let Some(t) = token()
            && let Ok(v) = gql(&names_q, Some(&t))
        {
            for e in v["data"]["user"]["followers"]["edges"].as_array().into_iter().flatten() {
                let n = &e["node"];
                if let Some(id) = n["id"].as_str()
                    && known.insert(id.to_string())
                {
                    named.push(User {
                        id: id.into(),
                        login: n["login"].as_str().unwrap_or("").into(),
                        display_name: n["displayName"].as_str().unwrap_or("").into(),
                        ..Default::default()
                    });
                }
            }
        }
        let new = (now - before) as usize;
        let people: Vec<User> = if named.is_empty() {
            // nameless, and not a flood of them (a bot wave, a recount)
            (0..new.min(5)).map(|_| User { display_name: "Someone".into(), ..Default::default() }).collect()
        } else {
            named.into_iter().rev().collect()
        };
        for u in people {
            let mut e = ChatEvent::new("twitch", &channel, crate::event::Kind::Follow);
            e.user = Some(u);
            sink.emit(e);
        }
    }
}

pub enum Parsed {
    Event(Box<ChatEvent>),
    /// In the channel; its Twitch id, when given.
    Joined(Option<String>),
    Reconnect,
    Nothing,
}

/// One IRC line: `@tags :prefix COMMAND #channel :trailing`.
pub fn parse(line: &str, channel: &str) -> Parsed {
    let (tags, rest) = match line.strip_prefix('@') {
        Some(l) => l.split_once(' ').map_or((HashMap::new(), ""), |(t, r)| (parse_tags(t), r)),
        None => (HashMap::new(), line),
    };
    let rest = rest.strip_prefix(':').map_or(rest, |r| r.split_once(' ').map_or("", |(_, r)| r));
    let (head, trailing) = rest.split_once(" :").unwrap_or((rest, ""));
    let mut words = head.split(' ');
    let command = words.next().unwrap_or("");
    let tag = |k: &str| tags.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let event = |kind: Kind| ChatEvent::new("twitch", channel, kind);
    match command {
        "ROOMSTATE" => Parsed::Joined(tag("room-id").map(String::from)),
        "RECONNECT" => Parsed::Reconnect,
        "PRIVMSG" => {
            let bits = tag("bits").and_then(|b| b.parse::<f64>().ok());
            let mut e = event(if bits.is_some() { Kind::Paid } else { Kind::Message });
            // "/me" messages come wrapped; emote positions count within the unwrapped text
            let inner = trailing.strip_prefix("\u{1}ACTION ").and_then(|t| t.strip_suffix('\u{1}')).unwrap_or(trailing);
            e.native_id = tag("id").map(String::from);
            e.ts_ms = tag("tmi-sent-ts").and_then(|t| t.parse().ok()).unwrap_or(e.ts_ms);
            e.user = Some(user(&tags));
            e.paid = bits.map(|b| Paid { display: format!("{b} bits"), amount: b, currency: "BITS".into() });
            e.reply_to = tag("reply-parent-msg-id").map(|id| ReplyTo {
                native_id: Some(id.to_string()),
                author: tag("reply-parent-display-name").unwrap_or("").to_string(),
                excerpt: tag("reply-parent-msg-body").unwrap_or("").to_string(),
            });
            // a reply starts with "@name " for older clients: the reply line shows it already
            let text = match &e.reply_to {
                Some(r) if !r.author.is_empty() => {
                    let lower = inner.to_lowercase();
                    let prefix = format!("@{} ", r.author.to_lowercase());
                    if lower.starts_with(&prefix) { inner.get(prefix.len()..).unwrap_or(inner) } else { inner }
                }
                _ => inner,
            };
            let offset = inner.chars().count() - text.chars().count();
            Parsed::Event(Box::new(e.with_fragments(fragments(text, tag("emotes"), offset))))
        }
        "USERNOTICE" => {
            let kind = match tag("msg-id").unwrap_or("") {
                "sub"
                | "resub"
                | "subgift"
                | "submysterygift"
                | "giftpaidupgrade"
                | "primepaidupgrade"
                | "anongiftpaidupgrade"
                | "communitypayforward" => Kind::Membership,
                "raid" => {
                    Kind::Raid { viewers: tag("msg-param-viewerCount").and_then(|v| v.parse().ok()).unwrap_or(0) }
                }
                _ => Kind::Notice,
            };
            let mut e = event(kind);
            e.native_id = tag("id").map(String::from);
            e.ts_ms = tag("tmi-sent-ts").and_then(|t| t.parse().ok()).unwrap_or(e.ts_ms);
            e.user = Some(user(&tags));
            e.headline = tag("system-msg").map(String::from);
            Parsed::Event(Box::new(e.with_fragments(fragments(trailing, tag("emotes"), 0))))
        }
        "CLEARMSG" => Parsed::Event(Box::new(event(Kind::Deletion {
            native_id: tag("target-msg-id").map(String::from),
            user_id: None,
        }))),
        "CLEARCHAT" if !trailing.is_empty() => Parsed::Event(Box::new(event(Kind::Deletion {
            native_id: None,
            user_id: tag("target-user-id").map(String::from),
        }))),
        _ => Parsed::Nothing,
    }
}

/// `a=1;b=x\sy` → {a: "1", b: "x y"}.
fn parse_tags(tags: &str) -> HashMap<String, String> {
    tags.split(';')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            let mut out = String::with_capacity(v.len());
            let mut chars = v.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some('s') => out.push(' '),
                        Some(':') => out.push(';'),
                        Some('\\') => out.push('\\'),
                        Some('r') => out.push('\r'),
                        Some('n') => out.push('\n'),
                        Some(o) => out.push(o),
                        None => {}
                    }
                } else {
                    out.push(c);
                }
            }
            (k.to_string(), out)
        })
        .collect()
}

fn user(tags: &HashMap<String, String>) -> User {
    let get = |k: &str| tags.get(k).cloned().unwrap_or_default();
    let badges: Vec<Badge> = get("badges")
        .split(',')
        .filter_map(|b| b.split_once('/'))
        .map(|(set, version)| Badge { set: set.into(), version: version.into(), image: None })
        .collect();
    let mut roles = Vec::new();
    for b in &badges {
        let role = match b.set.as_str() {
            "broadcaster" => Some(Role::Broadcaster),
            "moderator" => Some(Role::Moderator),
            "vip" => Some(Role::Vip),
            "subscriber" | "founder" => Some(Role::Subscriber),
            "partner" => Some(Role::Verified),
            _ => None,
        };
        if let Some(r) = role.filter(|r| !roles.contains(r)) {
            roles.push(r);
        }
    }
    let login = get("login");
    let display = get("display-name");
    User {
        id: get("user-id"),
        display_name: if display.is_empty() { login.clone() } else { display },
        login,
        color: tags.get("color").filter(|c| !c.is_empty()).cloned(),
        roles,
        badges,
    }
}

/// The message with its emotes: `emotes=25:0-4,6-10/1902:12-16` (character positions in the
/// whole trailing text; `offset` characters of it come before `text`).
fn fragments(text: &str, emotes: Option<&str>, offset: usize) -> Vec<Fragment> {
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    for e in emotes.unwrap_or("").split('/').filter(|e| !e.is_empty()) {
        let Some((id, ranges)) = e.split_once(':') else { continue };
        for r in ranges.split(',') {
            if let Some((a, b)) = r.split_once('-')
                && let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>())
                && a >= offset
            {
                spans.push((a - offset, b - offset, id.to_string()));
            }
        }
    }
    spans.sort();
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    for (a, b, id) in spans {
        if a < at || b >= chars.len() {
            continue;
        }
        out.extend(text_fragments(&chars[at..a].iter().collect::<String>()));
        let code: String = chars[a..=b].iter().collect();
        out.push(Fragment::Emote {
            provider: "twitch".into(),
            image: format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0"),
            id,
            code,
        });
        at = b + 1;
    }
    out.extend(text_fragments(&chars[at.min(chars.len())..].iter().collect::<String>()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(line: &str) -> ChatEvent {
        match parse(line, "ninja") {
            Parsed::Event(e) => *e,
            _ => panic!("no event from {line}"),
        }
    }

    #[test]
    fn a_message_with_emotes_badges_and_a_colour() {
        let e = event(
            "@badge-info=subscriber/14;badges=moderator/1,subscriber/12;color=#1E90FF;display-name=Viewer;emotes=25:0-4,19-23;id=abc-1;login=viewer;tmi-sent-ts=1700000000000;user-id=42 :viewer!viewer@viewer.tmi.twitch.tv PRIVMSG #ninja :Kappa hello @ninja Kappa",
        );
        assert_eq!(e.kind, Kind::Message);
        assert_eq!(e.native_id.as_deref(), Some("abc-1"));
        assert_eq!(e.ts_ms, 1_700_000_000_000);
        let u = e.user.as_ref().unwrap();
        assert_eq!((u.display_name.as_str(), u.color.as_deref(), u.id.as_str()), ("Viewer", Some("#1E90FF"), "42"));
        assert_eq!(u.roles, [Role::Moderator, Role::Subscriber]);
        assert_eq!(e.text, "Kappa hello @ninja Kappa");
        assert!(matches!(&e.fragments[0], Fragment::Emote { id, code, .. } if id == "25" && code == "Kappa"));
        assert!(e.fragments.iter().any(|f| matches!(f, Fragment::Mention { name } if name == "@ninja")));
        assert!(matches!(e.fragments.last(), Some(Fragment::Emote { .. })));
    }

    #[test]
    fn bits_subs_raids_replies_and_deletions() {
        let cheer = event(
            "@bits=500;display-name=Big;id=b1;user-id=7 :big!big@big.tmi.twitch.tv PRIVMSG #ninja :Cheer500 nice",
        );
        assert_eq!(cheer.kind, Kind::Paid);
        assert_eq!(cheer.paid.unwrap().display, "500 bits");
        let sub = event(
            "@display-name=Sub;msg-id=resub;system-msg=Sub\\ssubscribed\\sfor\\s12\\smonths!;user-id=8 :tmi.twitch.tv USERNOTICE #ninja :still here",
        );
        assert_eq!(sub.kind, Kind::Membership);
        assert_eq!(sub.headline.as_deref(), Some("Sub subscribed for 12 months!"));
        assert_eq!(sub.text, "still here");
        let raid = event(
            "@display-name=Raider;msg-id=raid;msg-param-viewerCount=120;user-id=9 :tmi.twitch.tv USERNOTICE #ninja",
        );
        assert_eq!(raid.kind, Kind::Raid { viewers: 120 });
        let reply = event(
            "@display-name=B;id=r1;reply-parent-display-name=A;reply-parent-msg-body=hi\\sall;reply-parent-msg-id=p1;user-id=10 :b!b@b PRIVMSG #ninja :@A hello",
        );
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.reply_to.unwrap().native_id.as_deref(), Some("p1"));
        let del = event("@login=x;target-msg-id=abc-1 :tmi.twitch.tv CLEARMSG #ninja :bad");
        assert_eq!(del.kind, Kind::Deletion { native_id: Some("abc-1".into()), user_id: None });
        let ban = event("@target-user-id=42 :tmi.twitch.tv CLEARCHAT #ninja :viewer");
        assert_eq!(ban.kind, Kind::Deletion { native_id: None, user_id: Some("42".into()) });
    }

    #[test]
    fn a_channel_however_it_was_typed() {
        for c in ["Ninja", "#ninja", "https://www.twitch.tv/Ninja", "twitch.tv/ninja/", "@ninja"] {
            assert_eq!(login(c), "ninja");
        }
    }
}
