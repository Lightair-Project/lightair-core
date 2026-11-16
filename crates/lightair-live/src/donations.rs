//! Donations from tip services, with the streamer's own token from each service's dashboard
//! (no app approval): Streamlabs' socket (socket.io: donations) and StreamElements' realtime
//! socket (Astro: tips). Tokens are kept by the app (the Keychain) and handed over here; the
//! channel a reader is given only names the service.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::Sink;
use crate::net;

static TOKENS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// Sets (or, empty, removes) a service's token ("streamlabs", "streamelements").
pub fn set_token(service: &str, token: &str) {
    let mut t = TOKENS.get_or_init(Default::default).lock();
    if token.trim().is_empty() {
        t.remove(service);
    } else {
        t.insert(service.into(), token.trim().into());
    }
}

/// Ko-fi's settings as kept (`{}` when there are none).
pub fn kofi_settings() -> Value {
    TOKENS.get_or_init(Default::default).lock().get("kofi").and_then(|t| serde_json::from_str(t).ok()).unwrap_or_else(|| json!({}))
}

fn token(service: &str) -> Result<String, String> {
    TOKENS.get_or_init(Default::default).lock().get(service).cloned().ok_or_else(|| format!("no {service} token (Settings › Connections)"))
}

fn money(amount: f64, currency: &str) -> Paid {
    let symbol = match currency {
        "USD" | "CAD" | "AUD" | "NZD" | "SGD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "KRW" => "₩",
        "IDR" => "Rp",
        _ => "",
    };
    let display = if symbol.is_empty() { format!("{amount:.2} {currency}") } else { format!("{symbol}{amount:.2}") };
    Paid { display, amount, currency: currency.into() }
}

fn tip(service: &str, name: &str, amount: f64, currency: &str, message: &str) -> ChatEvent {
    let mut e = ChatEvent::new(service, "tips", Kind::Paid).with_fragments(text_fragments(message));
    e.user = Some(User { id: name.to_lowercase(), login: name.to_lowercase(), display_name: name.into(), ..Default::default() });
    e.paid = Some(money(amount, currency));
    e
}

fn num(v: &Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0)
}

/// Streamlabs: socket.io (Engine.IO 3) over a WebSocket; donations come as `42["event", …]`.
pub fn streamlabs(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let t = token("streamlabs")?;
    let mut socket = net::connect(&format!("wss://sockets.streamlabs.com/socket.io/?token={t}&EIO=3&transport=websocket"), Duration::from_millis(500))?;
    let mut ping = Duration::from_secs(25);
    let mut last = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if last.elapsed() > ping {
            socket.send(Message::text("2")).map_err(|e| e.to_string())?;
            last = Instant::now();
        }
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        let t = t.as_str();
        if let Some(open) = t.strip_prefix('0') {
            if let Ok(v) = serde_json::from_str::<Value>(open) {
                ping = Duration::from_millis(v["pingInterval"].as_u64().unwrap_or(25_000));
            }
        } else if t == "40" {
            sink.status(Status::Connected);
        } else if t.starts_with("44") {
            return Err("Streamlabs refused the token: paste it again (Settings › Connections)".into());
        } else if let Some(payload) = t.strip_prefix("42")
            && let Ok(v) = serde_json::from_str::<Value>(payload)
        {
            for e in parse_streamlabs(&v[1]) {
                sink.emit(e);
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

pub fn parse_streamlabs(v: &Value) -> Vec<ChatEvent> {
    if v["type"].as_str() != Some("donation") {
        return Vec::new();
    }
    let list = match &v["message"] {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    list.iter()
        .map(|m| tip("streamlabs", m["name"].as_str().or(m["from"].as_str()).unwrap_or("Someone"), num(&m["amount"]), m["currency"].as_str().unwrap_or("USD"), m["message"].as_str().unwrap_or("")))
        .collect()
}

/// The `channel` claim of StreamElements' JWT (its channel id).
fn jwt_channel(jwt: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["channel"].as_str().map(String::from)
}

/// StreamElements: the Astro socket, subscribed to the channel's tips.
pub fn streamelements(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let jwt = token("streamelements")?;
    let room = jwt_channel(&jwt).ok_or("that isn't a StreamElements JWT token")?;
    let mut socket = net::connect("wss://astro.streamelements.com", Duration::from_millis(500))?;
    let subscribe = json!({"type": "subscribe", "nonce": format!("lightair-{}", crate::event::now_ms()), "data": {"topic": "channel.tips", "room": room, "token": jwt, "token_type": "jwt"}});
    socket.send(Message::text(subscribe.to_string())).map_err(|e| e.to_string())?;
    while !stop.load(Ordering::Acquire) {
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
        match v["type"].as_str() {
            Some("response") if v["error"].is_string() => return Err(format!("StreamElements: {}", v["data"]["message"].as_str().or(v["error"].as_str()).unwrap_or("refused"))),
            Some("response") => sink.status(Status::Connected),
            Some("message") => {
                if let Some(e) = parse_streamelements(&v["data"]) {
                    sink.emit(e);
                }
            }
            _ => {}
        }
    }
    let _ = socket.close(None);
    Ok(())
}

pub fn parse_streamelements(d: &Value) -> Option<ChatEvent> {
    let don = if d["donation"].is_object() { &d["donation"] } else { d };
    let name = don["user"]["username"].as_str().or(don["username"].as_str()).or(d["displayName"].as_str())?;
    Some(tip("streamelements", name, num(&don["amount"]), don["currency"].as_str().unwrap_or("USD"), don["message"].as_str().unwrap_or("")))
}

/// Ko-fi, through Lightair's relay (relay/: Ko-fi only sends webhooks). The "kofi" token is
/// `{"relay": address, "key": the inbox's key, "verify": Ko-fi's verification token}`.
pub fn kofi(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let t: Value = serde_json::from_str(&token("kofi")?).map_err(|_| "Ko-fi isn't set up (Settings › Connections)")?;
    let (Some(relay), Some(key), Some(verify)) = (t["relay"].as_str(), t["key"].as_str(), t["verify"].as_str()) else {
        return Err("Ko-fi needs the relay's address and Ko-fi's verification token (Settings › Connections)".into());
    };
    let host = relay.trim().trim_end_matches('/').replace("https://", "wss://").replace("http://", "ws://");
    let mut socket = net::connect(&format!("{host}/ws/{key}?kofi={}", verify.trim()), Duration::from_millis(500))?;
    sink.status(Status::Connected);
    let mut last = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if last.elapsed() > Duration::from_secs(30) {
            socket.send(Message::text("ping")).map_err(|e| e.to_string())?;
            last = Instant::now();
        }
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        if let Ok(v) = serde_json::from_str::<Value>(t.as_str())
            && let Some(e) = parse_kofi(&v)
        {
            sink.emit(e);
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// A relayed Ko-fi event: a tip, a membership or a shop order.
pub fn parse_kofi(v: &Value) -> Option<ChatEvent> {
    let name = v["name"].as_str().unwrap_or("Someone");
    let mut e = tip("kofi", name, num(&v["amount"]), v["currency"].as_str().unwrap_or("USD"), v["message"].as_str().unwrap_or(""));
    match v["type"].as_str() {
        Some("Subscription") => {
            e.kind = Kind::Membership;
            let tier = v["tier"].as_str().unwrap_or("");
            e.headline = Some(if tier.is_empty() { format!("{name} became a member on Ko-fi") } else { format!("{name} joined {tier} on Ko-fi") });
        }
        Some("Shop Order") => e.headline = Some(format!("{name} bought from the shop")),
        _ => {}
    }
    e.native_id = v["id"].as_str().map(String::from);
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamlabs_and_streamelements_tips() {
        let v: Value = serde_json::from_str(r#"{"type":"donation","message":[{"name":"Fan","amount":"5.0000000000","currency":"USD","message":"hi"}]}"#).unwrap();
        let e = &parse_streamlabs(&v)[0];
        assert_eq!((e.name(), e.paid.as_ref().unwrap().amount, e.paid.as_ref().unwrap().display.as_str()), ("Fan", 5.0, "$5.00"));
        let d: Value = serde_json::from_str(r#"{"donation":{"user":{"username":"Pal"},"amount":10,"currency":"EUR","message":"gg"}}"#).unwrap();
        assert_eq!(parse_streamelements(&d).unwrap().paid.unwrap().display, "€10.00");
        // {"alg":"none"}.{"channel":"abc"}.
        assert_eq!(jwt_channel("eyJhbGciOiJub25lIn0.eyJjaGFubmVsIjoiYWJjIn0.x").as_deref(), Some("abc"));
        let k: Value = serde_json::from_str(r#"{"source":"kofi","type":"Donation","name":"Kim","amount":3,"currency":"GBP","message":"ty"}"#).unwrap();
        assert_eq!(parse_kofi(&k).unwrap().paid.unwrap().display, "£3.00");
    }
}
