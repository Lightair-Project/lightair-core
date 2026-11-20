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
    /// Its description, where the platform has one per stream and it's known (YouTube, signed
    /// in officially).
    pub description: Option<String>,
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

pub(crate) fn iso_ms(s: &str) -> Option<i64> {
    // "2026-11-17T08:30:00Z" (seconds precision is enough)
    let d: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .take(6)
        .filter_map(|x| x.parse().ok())
        .collect();
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
    if let Some(r) = crate::official::info(platform) {
        return r;
    }
    match platform {
        "twitch" => {
            let login = crate::twitch::login(channel);
            let q = format!(
                "query{{user(login:\"{login}\"){{broadcastSettings{{title game{{displayName}}}} stream{{title viewersCount createdAt game{{displayName}}}}}}}}"
            );
            let agent = net::agent();
            let mut res = agent
                .post("https://gql.twitch.tv/gql")
                .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
                .send(serde_json::json!({"query": q}).to_string())
                .map_err(|e| e.to_string())?;
            let v: Value = serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
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
                description: None,
            })
        }
        "youtube" | "youtube-shorts" => {
            let Some(video) = crate::youtube::video_of(channel)? else { return Ok(StreamInfo::default()) };
            let page = net::get(
                &net::agent(),
                &format!("https://www.youtube.com/watch?v={video}"),
                &[("Cookie", "SOCS=CAI")],
            )?;
            Ok(StreamInfo {
                live: page.contains("\"isLiveNow\":true"),
                title: between(&page, "<meta name=\"title\" content=\"", "\"").unwrap_or_default(),
                category: between(&page, "\"category\":\"", "\"").unwrap_or_default(),
                viewers: between(&page, "\"originalViewCount\":\"", "\"").and_then(|n| n.parse().ok()),
                started_ms: between(&page, "\"startTimestamp\":\"", "\"").as_deref().and_then(iso_ms),
                description: None,
            })
        }
        "kick" => {
            let stop = std::sync::atomic::AtomicBool::new(false);
            let v = crate::pages::fetch_json(
                &format!("https://kick.com/api/v2/channels/{}", crate::kick::slug(channel)),
                std::time::Duration::from_secs(30),
                &stop,
            )?;
            let l = &v["livestream"];
            Ok(StreamInfo {
                live: l.is_object() && l["is_live"].as_bool() != Some(false),
                title: l["session_title"].as_str().unwrap_or("").into(),
                category: l["categories"][0]["name"].as_str().unwrap_or("").into(),
                viewers: l["viewer_count"].as_u64(),
                started_ms: l["start_time"].as_str().or(l["created_at"].as_str()).and_then(iso_ms),
                description: None,
            })
        }
        other => Err(format!("{other}'s stream details aren't read yet")),
    }
}

/// A category a stream can be put in, as `stream.categories` answers it.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Category {
    pub id: String,
    pub name: String,
    /// Its picture (box art), where the platform has one.
    pub image: Option<String>,
}

/// YouTube's categories (a fixed list; a live stream takes any of them).
const YOUTUBE_CATEGORIES: &[(&str, &str)] = &[
    ("20", "Gaming"),
    ("24", "Entertainment"),
    ("22", "People & Blogs"),
    ("10", "Music"),
    ("27", "Education"),
    ("28", "Science & Technology"),
    ("17", "Sports"),
    ("26", "Howto & Style"),
    ("23", "Comedy"),
    ("25", "News & Politics"),
    ("1", "Film & Animation"),
    ("2", "Autos & Vehicles"),
    ("15", "Pets & Animals"),
    ("19", "Travel & Events"),
    ("29", "Nonprofits & Activism"),
];

/// Categories on `platform` whose names match `query` (at most 20, best first), for choosing
/// one before `edit`. Twitch and Kick by their own search; YouTube from its fixed list.
pub fn categories(platform: &str, query: &str) -> Result<Vec<Category>, String> {
    let query = query.trim();
    match platform {
        "twitch" => {
            let v = twitch_gql_body(serde_json::json!({
                "query": "query($q:String!){searchCategories(query:$q,first:20){edges{node{id name displayName boxArtURL(width:144,height:192)}}}}",
                "variables": {"q": query},
            }))?;
            let edges = v["data"]["searchCategories"]["edges"].as_array().cloned().unwrap_or_default();
            Ok(edges
                .iter()
                .map(|e| &e["node"])
                .filter_map(|n| {
                    Some(Category {
                        id: n["id"].as_str()?.to_string(),
                        name: n["displayName"].as_str().or(n["name"].as_str())?.to_string(),
                        image: n["boxArtURL"].as_str().map(str::to_string),
                    })
                })
                .collect())
        }
        "kick" => {
            // from a kick.com page, as its own site asks (a request from outside meets its bot check)
            let script = format!(
                r#"(async () => {{
  try {{
    var r = await fetch('https://search.kick.com/collections/subcategory_index/documents/search?q=' + encodeURIComponent({q}) + '&query_by=name&per_page=20', {{ headers: {{ 'X-Typesense-Api-Key': '{KICK_SEARCH_KEY}' }} }});
    lightair('ok:' + await r.text());
  }} catch (e) {{ lightair('no:' + e); }}
}})();"#,
                q = serde_json::to_string(query).unwrap_or_default(),
            );
            let answer = crate::pages::run(
                "https://kick.com/",
                &script,
                std::time::Duration::from_secs(30),
                &std::sync::atomic::AtomicBool::new(false),
            )?;
            let text = answer
                .strip_prefix("ok:")
                .ok_or_else(|| format!("Kick's search didn't answer: {}", answer.trim_start_matches("no:")))?;
            let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            let hits = v["hits"].as_array().cloned().unwrap_or_default();
            Ok(hits
                .iter()
                .map(|h| &h["document"])
                .filter_map(|d| {
                    let id =
                        d["id"].as_str().map(str::to_string).or_else(|| d["id"].as_u64().map(|n| n.to_string()))?;
                    let image = ["src", "banner", "thumbnail"].iter().find_map(|k| d[*k].as_str()).map(str::to_string);
                    Some(Category { id, name: d["name"].as_str()?.to_string(), image })
                })
                .collect())
        }
        "youtube" | "youtube-shorts" => {
            let q = query.to_lowercase();
            Ok(YOUTUBE_CATEGORIES
                .iter()
                .filter(|(_, name)| q.is_empty() || name.to_lowercase().contains(&q))
                .map(|(id, name)| Category { id: id.to_string(), name: name.to_string(), image: None })
                .collect())
        }
        other => Err(format!("{other} has no categories to choose from in Lightair")),
    }
}

/// The key Kick's own site searches its categories with (public, in its pages).
const KICK_SEARCH_KEY: &str = "nXIMW0iEN6sMujFYjFuhdrSwVow3pDQu";

fn twitch_gql(query: &str) -> Result<Value, String> {
    twitch_gql_body(serde_json::json!({"query": query}))
}

/// An anonymous GraphQL request (the site's public client id).
fn twitch_gql_body(body: Value) -> Result<Value, String> {
    let agent = net::agent();
    let mut res = agent
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
        .send(body.to_string())
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// A GraphQL request as the signed-in account (`Authorization: OAuth <the site's token>`).
fn twitch_gql_as(token: &str, body: Value) -> Result<Value, String> {
    let agent = net::agent();
    let mut res = agent
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko")
        .header("Authorization", &format!("OAuth {token}"))
        .header("User-Agent", net::USER_AGENT)
        .send(body.to_string())
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Whether Lightair can change the stream's title and category itself on `platform`. (YouTube
/// Studio's own edit asks for Google's anti-bot attestation: its dashboard does it.)
pub fn can_edit(platform: &str) -> bool {
    matches!(platform, "twitch" | "kick") || crate::official::signed_in(platform)
}

/// Whether the stream's description can be changed from Lightair (YouTube, signed in
/// officially).
pub fn can_describe(platform: &str) -> bool {
    matches!(platform, "youtube" | "youtube-shorts") && crate::official::signed_in(platform)
}

/// Changes the stream's title and/or category (by name: the closest the platform finds), as
/// the person signed in to the platform in Lightair.
pub fn edit(
    platform: &str,
    channel: &str,
    title: Option<&str>,
    description: Option<&str>,
    category: Option<&str>,
) -> Result<String, String> {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    let category = category.map(str::trim).filter(|c| !c.is_empty());
    // the official API where signed in that way
    if crate::official::signed_in(platform) {
        return crate::official::edit(platform, channel, title, description, category);
    }
    match platform {
        "twitch" => edit_twitch(channel, title, category),
        "kick" => edit_kick(channel, title, category),
        other => Err(format!("change it in {other}'s dashboard")),
    }
}

/// Twitch: the dashboard's own GraphQL mutation (`updateBroadcastSettings`; the title is
/// `status`). Twitch's Helix API doesn't take the site's session.
fn edit_twitch(channel: &str, title: Option<&str>, category: Option<&str>) -> Result<String, String> {
    let token = crate::pages::cookie("https://www.twitch.tv", "auth-token")
        .ok_or("sign in to Twitch first (Outputs › the destination › Sending)")?;
    let login = crate::twitch::login(channel);
    let id = twitch_gql(&format!("query{{user(login:\"{login}\"){{id}}}}"))?["data"]["user"]["id"]
        .as_str()
        .ok_or("no such Twitch channel")?
        .to_string();
    let mut input = serde_json::json!({"userID": id});
    if let Some(t) = title {
        input["status"] = serde_json::json!(t);
    }
    let mut found = String::new();
    if let Some(c) = category {
        let v = twitch_gql_as(
            &token,
            serde_json::json!({
                "query": "query($q:String!){searchCategories(query:$q,first:1){edges{node{id name displayName}}}}",
                "variables": {"q": c},
            }),
        )?;
        let node = &v["data"]["searchCategories"]["edges"][0]["node"];
        let gid = node["id"].as_str().ok_or_else(|| format!("Twitch has no category like “{c}”"))?;
        found = node["displayName"].as_str().or(node["name"].as_str()).unwrap_or("").to_string();
        input["categoryID"] = serde_json::json!(gid);
        input["game"] = serde_json::json!(node["name"].as_str().unwrap_or(&found));
    }
    let v = twitch_gql_as(
        &token,
        serde_json::json!({
            "operationName": "EditBroadcastContext_BroadcastSettingsMutation",
            "query": "mutation EditBroadcastContext_BroadcastSettingsMutation($input: UpdateBroadcastSettingsInput!) { updateBroadcastSettings(input: $input) { broadcastSettings { id title game { id name displayName } } error } }",
            "variables": {"input": input},
        }),
    )?;
    if let Some(e) = v["errors"][0]["message"].as_str() {
        return Err(format!("Twitch said no: {e}"));
    }
    let r = &v["data"]["updateBroadcastSettings"];
    if let Some(e) = r["error"].as_str().filter(|e| !e.is_empty()) {
        return Err(format!("Twitch said no: {e}"));
    }
    if r.is_null() {
        return Err("Twitch didn't answer the change".into());
    }
    Ok(if found.is_empty() { "Saved".into() } else { format!("Saved (category: {found})") })
}

/// Kick: its site's own request (`PATCH /api/v2/channels/<slug>/stream-info`), from a
/// signed-in hidden page; the category found through Kick's own search. Kick changes the title
/// only while live.
fn edit_kick(channel: &str, title: Option<&str>, category: Option<&str>) -> Result<String, String> {
    if !crate::accounts::signed_in("kick") {
        return Err("sign in to Kick first (Outputs › the destination › Sending)".into());
    }
    let slug = crate::kick::slug(channel);
    let script = format!(
        r#"(async function () {{
  function cookie(n) {{ var m = document.cookie.match(new RegExp('(?:^|; )' + n + '=([^;]*)')); return m ? decodeURIComponent(m[1]) : ''; }}
  try {{
    var title = {title}, category = {category}, body = {{}}, found = '';
    if (title) body.stream_title = title;
    if (category) {{
      var r = await fetch('https://search.kick.com/collections/subcategory_index/documents/search?q=' + encodeURIComponent(category) + '&query_by=name', {{ headers: {{ 'X-Typesense-Api-Key': '{kick_key}' }} }});
      var hit = ((await r.json()).hits || [])[0];
      if (!hit) return lightair('no:Kick has no category like “' + category + '”');
      body.category_id = +hit.document.id; found = hit.document.name || '';
    }}
    var headers = {{ 'Accept': 'application/json', 'Content-Type': 'application/json', 'X-XSRF-TOKEN': cookie('XSRF-TOKEN') }};
    var session = cookie('session_token');
    if (session) headers['Authorization'] = 'Bearer ' + session;
    var res = await fetch('/api/v2/channels/{slug}/stream-info', {{ method: 'PATCH', credentials: 'include', headers: headers, body: JSON.stringify(body) }});
    lightair(res.ok ? 'ok:' + found : 'no:' + res.status + ' ' + (await res.text()).slice(0, 200));
  }} catch (e) {{ lightair('no:' + e); }}
}})();"#,
        title = serde_json::to_string(&title).unwrap_or_default(),
        category = serde_json::to_string(&category).unwrap_or_default(),
        kick_key = KICK_SEARCH_KEY,
    );
    let answer = crate::pages::run(
        &format!("https://kick.com/{slug}"),
        &script,
        std::time::Duration::from_secs(30),
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    match (answer.strip_prefix("ok:"), answer.strip_prefix("no:")) {
        (Some(""), _) => Ok("Saved".into()),
        (Some(found), _) => Ok(format!("Saved (category: {found})")),
        (_, Some(why)) => Err(format!("Kick said no: {why}")),
        _ => Err("Kick didn't answer".into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_times() {
        assert_eq!(super::iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(super::iso_ms("2026-11-16T13:00:00Z"), Some(1_794_834_000_000));
    }
}

#[cfg(test)]
mod category_tests {
    use super::*;

    #[test]
    fn youtube_categories_filter_by_name() {
        let all = categories("youtube", "").unwrap();
        assert_eq!(all.len(), YOUTUBE_CATEGORIES.len());
        let g = categories("youtube", "gam").unwrap();
        assert_eq!(g, vec![Category { id: "20".into(), name: "Gaming".into(), image: None }]);
        assert!(categories("tiktok", "x").is_err());
    }

    /// Asks Twitch itself (network): `cargo test -p lightair-live -- --ignored categories_live`
    #[test]
    #[ignore]
    fn categories_live() {
        // (Kick's goes through a hidden page: the app's, not a test's)
        let found = categories("twitch", "just chatting").unwrap();
        eprintln!("twitch: {:?}", found.iter().take(3).collect::<Vec<_>>());
        assert!(found.iter().any(|c| c.name.eq_ignore_ascii_case("just chatting")), "{found:?}");
    }
}
