//! Third-party emotes for Twitch chat: 7TV, FrankerFaceZ and BetterTTV, each's global set and
//! the channel's own (by Twitch channel id). A word in a message that is one of their codes is
//! drawn as the emote; when two share a code, 7TV wins over FFZ over BTTV (Chatterino's order).

use std::collections::HashMap;

use serde_json::Value;

use crate::event::Fragment;
use crate::net;

#[derive(Clone, Debug, Default)]
pub struct Emotes {
    /// code → (provider, id, image)
    by_code: HashMap<String, (String, String, String)>,
}

impl Emotes {
    pub fn len(&self) -> usize {
        self.by_code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }

    fn add(&mut self, provider: &str, id: &str, code: &str, image: String) {
        // later providers don't replace earlier ones (loaded in precedence order)
        self.by_code.entry(code.to_string()).or_insert((provider.into(), id.into(), image));
    }

    /// The globals and the channel's own (Twitch channel id), from the three services; a service
    /// that doesn't answer is left out.
    pub fn load(twitch_channel_id: &str) -> Emotes {
        let agent = net::agent();
        let mut e = Emotes::default();
        let get = |url: &str| net::get_json(&agent, url, &[]).ok();
        // 7TV
        for set in [
            get(&format!("https://7tv.io/v3/users/twitch/{twitch_channel_id}")).map(|u| u["emote_set"].clone()),
            get("https://7tv.io/v3/emote-sets/global"),
        ]
        .into_iter()
        .flatten()
        {
            for em in set["emotes"].as_array().into_iter().flatten() {
                let (Some(id), Some(name)) = (em["id"].as_str(), em["name"].as_str()) else { continue };
                let host = em["data"]["host"]["url"].as_str().unwrap_or("");
                let base = if host.starts_with("//") { format!("https:{host}") } else { host.to_string() };
                e.add("7tv", id, name, format!("{base}/2x.webp"));
            }
        }
        // FrankerFaceZ
        let ffz_sets = |v: Option<Value>| -> Vec<Value> {
            v.map(|v| v["sets"].as_object().map(|s| s.values().cloned().collect()).unwrap_or_default()).unwrap_or_default()
        };
        let mut ffz = ffz_sets(get(&format!("https://api.frankerfacez.com/v1/room/id/{twitch_channel_id}")));
        ffz.extend(ffz_sets(get("https://api.frankerfacez.com/v1/set/global")));
        for set in ffz {
            for em in set["emoticons"].as_array().into_iter().flatten() {
                let (Some(id), Some(name)) = (em["id"].as_u64(), em["name"].as_str()) else { continue };
                let urls = &em["urls"];
                let image = urls["2"].as_str().or(urls["1"].as_str()).unwrap_or("");
                let image = if image.starts_with("//") { format!("https:{image}") } else { image.to_string() };
                e.add("ffz", &id.to_string(), name, image);
            }
        }
        // BetterTTV
        let mut bttv: Vec<Value> = Vec::new();
        if let Some(u) = get(&format!("https://api.betterttv.net/3/cached/users/twitch/{twitch_channel_id}")) {
            for k in ["channelEmotes", "sharedEmotes"] {
                bttv.extend(u[k].as_array().cloned().unwrap_or_default());
            }
        }
        bttv.extend(get("https://api.betterttv.net/3/cached/emotes/global").and_then(|g| g.as_array().cloned()).unwrap_or_default());
        for em in bttv {
            let (Some(id), Some(code)) = (em["id"].as_str(), em["code"].as_str()) else { continue };
            let kind = em["imageType"].as_str().unwrap_or("png");
            e.add("bttv", id, code, format!("https://cdn.betterttv.net/emote/{id}/2x.{kind}"));
        }
        e
    }

    /// The message with every word that's an emote code drawn as the emote.
    pub fn apply(&self, fragments: Vec<Fragment>) -> Vec<Fragment> {
        if self.by_code.is_empty() {
            return fragments;
        }
        let mut out: Vec<Fragment> = Vec::new();
        let push_text = |out: &mut Vec<Fragment>, s: &str| {
            if s.is_empty() {
                return;
            }
            if let Some(Fragment::Text { text }) = out.last_mut() {
                text.push_str(s);
            } else {
                out.push(Fragment::Text { text: s.into() });
            }
        };
        for f in fragments {
            let Fragment::Text { text } = &f else {
                out.push(f);
                continue;
            };
            for (i, word) in text.split(' ').enumerate() {
                if i > 0 {
                    push_text(&mut out, " ");
                }
                match self.by_code.get(word) {
                    Some((provider, id, image)) => out.push(Fragment::Emote {
                        provider: provider.clone(),
                        id: id.clone(),
                        code: word.into(),
                        image: image.clone(),
                    }),
                    None => push_text(&mut out, word),
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_that_are_codes_become_emotes_and_the_first_provider_wins() {
        let mut e = Emotes::default();
        e.add("7tv", "1", "catJAM", "https://7tv/1".into());
        e.add("bttv", "2", "catJAM", "https://bttv/2".into());
        e.add("bttv", "3", "monkaS", "https://bttv/3".into());
        let out = e.apply(vec![Fragment::Text { text: "hello catJAM and monkaS!".into() }]);
        assert!(matches!(&out[1], Fragment::Emote { provider, .. } if provider == "7tv"));
        // "monkaS!" isn't the code
        assert_eq!(crate::event::plain_text(&out), "hello catJAM and monkaS!");
        assert_eq!(out.iter().filter(|f| matches!(f, Fragment::Emote { .. })).count(), 1);
    }
}
