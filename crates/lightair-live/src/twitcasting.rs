//! TwitCasting (ツイキャス) chat, read without logging in: the user's current movie
//! (`streamserver.php`), then its event socket (`eventpubsuburl.php`), which sends JSON arrays
//! of comments and gifts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;
use tungstenite::Message;

use crate::event::{ChatEvent, Kind, Paid, Status, User, text_fragments};
use crate::hub::{Sink, sleep_unless};
use crate::net;

/// "https://twitcasting.tv/user_id", "twitcasting.tv/c:user", "user_id" → "user_id".
pub fn user(channel: &str) -> String {
    let c = channel.trim().trim_end_matches('/');
    let path = c.split("twitcasting.tv/").nth(1).unwrap_or(c);
    path.split(['/', '?']).next().unwrap_or("").to_string()
}

pub fn read(sink: &Sink, stop: &AtomicBool) -> Result<(), String> {
    let agent = net::agent();
    let u = user(&sink.channel().channel);
    let info = net::get_json(&agent, &format!("https://twitcasting.tv/streamserver.php?target={u}&mode=client"), &[])?;
    let (Some(movie), true) = (info["movie"]["id"].as_u64(), info["movie"]["live"].as_bool() == Some(true)) else {
        sink.status(Status::Offline);
        sleep_unless(stop, Duration::from_secs(60));
        return Ok(());
    };
    let mut res = agent
        .post("https://twitcasting.tv/eventpubsuburl.php")
        .header("User-Agent", net::USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(format!("movie_id={movie}"))
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let url = v["url"].as_str().ok_or("no event socket for that movie")?;
    let mut socket = net::connect(url, Duration::from_millis(500))?;
    sink.status(Status::Connected);
    let channel = sink.channel().channel.clone();
    while !stop.load(Ordering::Acquire) {
        let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
        let items = match v {
            Value::Array(a) => a,
            other => vec![other],
        };
        for item in &items {
            if let Some(e) = parse(item, &channel) {
                sink.emit(e);
            }
        }
    }
    let _ = socket.close(None);
    Ok(())
}

fn person(v: &Value) -> User {
    let name = v["name"].as_str().unwrap_or("").to_string();
    let screen = v["screenName"].as_str().or(v["screen_id"].as_str()).unwrap_or("").to_string();
    User {
        id: v["id"].as_str().map(String::from).unwrap_or_else(|| screen.clone()),
        login: screen,
        display_name: name,
        ..Default::default()
    }
}

/// A comment or a gift.
pub fn parse(v: &Value, channel: &str) -> Option<ChatEvent> {
    match v["type"].as_str()? {
        "comment" => {
            let mut e = ChatEvent::new("twitcasting", channel, Kind::Message);
            e.native_id = v["id"].as_u64().map(|i| i.to_string()).or(v["id"].as_str().map(String::from));
            e.ts_ms = v["createdAt"].as_i64().unwrap_or(e.ts_ms);
            e.user = Some(person(&v["author"]));
            Some(e.with_fragments(text_fragments(v["message"].as_str().unwrap_or(""))))
        }
        "gift" => {
            let mut e = ChatEvent::new("twitcasting", channel, Kind::Notice);
            let u = person(&v["sender"]);
            let item = v["item"]["name"].as_str().unwrap_or("a gift");
            e.headline = Some(format!("{} sent {item}", u.display_name));
            e.user = Some(u);
            e.paid = Some(Paid { display: item.to_string(), amount: 1.0, currency: "TWITCASTING_ITEM".into() });
            Some(e.with_fragments(text_fragments(v["message"].as_str().unwrap_or(""))))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_gifts() {
        let c: Value = serde_json::from_str(r#"{"type":"comment","id":42,"message":"こんにちは","createdAt":1700000000000,"author":{"id":"c:fan","name":"ファン","screenName":"c:fan"}}"#).unwrap();
        let e = parse(&c, "x").unwrap();
        assert_eq!((e.text.as_str(), e.user.unwrap().display_name.as_str()), ("こんにちは", "ファン"));
        let g: Value = serde_json::from_str(
            r#"{"type":"gift","message":"","item":{"name":"お茶"},"sender":{"name":"A","screenName":"a"}}"#,
        )
        .unwrap();
        assert_eq!(parse(&g, "x").unwrap().headline.as_deref(), Some("A sent お茶"));
        assert_eq!(user("https://twitcasting.tv/c:someone/movie/1"), "c:someone");
    }
}
