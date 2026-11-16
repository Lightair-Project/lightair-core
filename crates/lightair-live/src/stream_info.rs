//! What a channel's stream looks like from outside (the Stream manager panel): live or not,
//! its title, category, viewers and since when, from the platforms' public pages and requests
//! (Kick's through a hidden page). And where each platform's own stream dashboard is.

use serde::Serialize;
use serde_json::Value;

use crate::net;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StreamInfo {
    pub live: bool,
    pub title: String,
    pub category: String,
    pub viewers: Option<u64>,
    /// When it went live (ms since the epoch).
    pub started_ms: Option<i64>,
}

/// The platform's own page for managing the stream (opened signed in, in a Lightair window).
pub fn dashboard(platform: &str, channel: &str) -> Option<String> {
    Some(match platform {
        "twitch" => format!("https://dashboard.twitch.tv/u/{}/stream-manager", crate::twitch::login(channel)),
        "youtube" | "youtube-shorts" => "https://studio.youtube.com/".into(),
        "kick" => "https://dashboard.kick.com/stream".into(),
        "tiktok" => "https://livecenter.tiktok.com/".into(),
        "bilibili" => "https://link.bilibili.com/p/center/index".into(),
        "chzzk" => "https://studio.chzzk.naver.com/".into(),
        "soop" => "https://studio.sooplive.co.kr/".into(),
        _ => return None,
    })
}

fn iso_ms(s: &str) -> Option<i64> {
    // "2026-11-17T08:30:00Z" (seconds precision is enough)
    let d: Vec<i64> = s.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).take(6).filter_map(|x| x.parse().ok()).collect();
    let [y, m, day, h, mi, sec] = d.as_slice().try_into().ok()?;
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + h * 3600 + mi * 60 + sec) * 1000)
}

fn between(s: &str, a: &str, b: &str) -> Option<String> {
    let i = s.find(a)? + a.len();
    let j = s[i..].find(b)?;
    Some(s[i..i + j].to_string())
}

/// Looks the stream up (a network request or a hidden page: not on the main thread).
pub fn info(platform: &str, channel: &str) -> Result<StreamInfo, String> {
    match platform {
        "twitch" => {
            let login = crate::twitch::login(channel);
            let q = format!("query{{user(login:\"{login}\"){{broadcastSettings{{title game{{displayName}}}} stream{{title viewersCount createdAt game{{displayName}}}}}}}}");
            let agent = net::agent();
            let mut res = agent
                .post("https://gql.twitch.tv/gql")
                .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
                .send(serde_json::json!({"query": q}).to_string())
                .map_err(|e| e.to_string())?;
            let v: Value = serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let u = &v["data"]["user"];
            let s = &u["stream"];
            let live = s.is_object();
            let text = |a: &Value, b: &Value| a.as_str().or(b.as_str()).unwrap_or("").to_string();
            Ok(StreamInfo {
                live,
                title: text(&s["title"], &u["broadcastSettings"]["title"]),
                category: text(&s["game"]["displayName"], &u["broadcastSettings"]["game"]["displayName"]),
                viewers: s["viewersCount"].as_u64(),
                started_ms: s["createdAt"].as_str().and_then(iso_ms),
            })
        }
        "youtube" | "youtube-shorts" => {
            let Some(video) = crate::youtube::video_of(channel)? else { return Ok(StreamInfo::default()) };
            let page = net::get(&net::agent(), &format!("https://www.youtube.com/watch?v={video}"), &[("Cookie", "SOCS=CAI")])?;
            Ok(StreamInfo {
                live: page.contains("\"isLiveNow\":true"),
                title: between(&page, "<meta name=\"title\" content=\"", "\"").unwrap_or_default(),
                category: between(&page, "\"category\":\"", "\"").unwrap_or_default(),
                viewers: between(&page, "\"originalViewCount\":\"", "\"").and_then(|n| n.parse().ok()),
                started_ms: between(&page, "\"startTimestamp\":\"", "\"").as_deref().and_then(iso_ms),
            })
        }
        "kick" => {
            let stop = std::sync::atomic::AtomicBool::new(false);
            let v = crate::pages::fetch_json(&format!("https://kick.com/api/v2/channels/{}", crate::kick::slug(channel)), std::time::Duration::from_secs(30), &stop)?;
            let l = &v["livestream"];
            Ok(StreamInfo {
                live: l.is_object() && l["is_live"].as_bool() != Some(false),
                title: l["session_title"].as_str().unwrap_or("").into(),
                category: l["categories"][0]["name"].as_str().unwrap_or("").into(),
                viewers: l["viewer_count"].as_u64(),
                started_ms: l["start_time"].as_str().or(l["created_at"].as_str()).and_then(iso_ms),
            })
        }
        other => Err(format!("{other}'s stream details aren't read yet")),
    }
}

fn twitch_gql(query: &str) -> Result<Value, String> {
    let agent = net::agent();
    let mut res = agent
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
        .send(serde_json::json!({"query": query}).to_string())
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Whether Lightair can change the stream's title and category itself on `platform`.
pub fn can_edit(platform: &str) -> bool {
    platform == "twitch"
}

/// Changes the stream's title and/or category (by name: the closest the platform finds), as
/// the person signed in to the platform in Lightair.
pub fn edit(platform: &str, channel: &str, title: Option<&str>, category: Option<&str>) -> Result<String, String> {
    if platform != "twitch" {
        return Err(format!("change it in {platform}'s dashboard"));
    }
    let token = crate::pages::cookie("https://www.twitch.tv", "auth-token").ok_or("sign in to Twitch first (Outputs › the destination › Sending)")?;
    let login = crate::twitch::login(channel);
    let id = twitch_gql(&format!("query{{user(login:\"{login}\"){{id}}}}"))?["data"]["user"]["id"].as_str().ok_or("no such Twitch channel")?.to_string();
    let mut body = serde_json::json!({});
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        body["title"] = serde_json::json!(t);
    }
    let mut found = String::new();
    if let Some(c) = category.map(str::trim).filter(|c| !c.is_empty()) {
        let q = format!("query{{searchCategories(query:{},first:1){{edges{{node{{id name}}}}}}}}", serde_json::to_string(c).unwrap_or_default());
        let v = twitch_gql(&q)?;
        let node = &v["data"]["searchCategories"]["edges"][0]["node"];
        let gid = node["id"].as_str().ok_or_else(|| format!("Twitch has no category like “{c}”"))?;
        found = node["name"].as_str().unwrap_or("").to_string();
        body["game_id"] = serde_json::json!(gid);
    }
    let agent = net::agent();
    let mut res = agent
        .patch(&format!("https://api.twitch.tv/helix/channels?broadcaster_id={id}"))
        .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    if status >= 300 {
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let why = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(String::from)).unwrap_or(text);
        return Err(format!("Twitch said no ({status}): {why}"));
    }
    Ok(if found.is_empty() { "Saved".into() } else { format!("Saved (category: {found})") })
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_times() {
        assert_eq!(super::iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(super::iso_ms("2026-11-16T13:00:00Z"), Some(1_794_834_000_000));
    }
}
