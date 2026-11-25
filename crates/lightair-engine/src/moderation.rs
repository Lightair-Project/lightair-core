//! Chat safety (App Store guideline 1.2): people the person hid, a
//! word filter, and where to report someone on their platform. One set for the app (Remote's
//! chat from a desktop and Studio's own), kept in the app's folder, applied before chat is shown,
//! drawn in a chat box, or spoken as an alert.

use std::collections::BTreeSet;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// A person hidden from chat: by platform and the platform's user id (names change).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hidden {
    pub platform: String,
    pub user_id: String,
    /// As shown when they were hidden (for the list in Settings).
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Masks the filtered words in messages and alerts.
    pub word_filter: bool,
    /// The person's own words, on top of the built-in ones.
    pub words: Vec<String>,
    pub hidden: BTreeSet<Hidden>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { word_filter: true, words: Vec::new(), hidden: BTreeSet::new() }
    }
}

/// Common profanity masked by default (the person adds their own). Matched as whole words, any
/// case, with simple suffixes.
const BUILT_IN: &[&str] = &[
    "fuck",
    "fucking",
    "fucker",
    "shit",
    "shitty",
    "bitch",
    "bastard",
    "asshole",
    "dick",
    "cunt",
    "wanker",
    "twat",
    "motherfucker",
    "bullshit",
    "piss",
    "slut",
    "whore",
];

struct State {
    settings: Settings,
    file: Option<PathBuf>,
}

static STATE: Mutex<State> = Mutex::new(State {
    settings: Settings { word_filter: true, words: Vec::new(), hidden: BTreeSet::new() },
    file: None,
});

/// Where the settings are kept (the app's folder); read once.
pub fn open(dir: &std::path::Path) {
    let file = dir.join("chat-safety.json");
    let settings = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut s = STATE.lock();
    s.settings = settings;
    s.file = Some(file);
}

fn save(s: &State) {
    if let Some(f) = &s.file
        && let Ok(t) = serde_json::to_string_pretty(&s.settings)
    {
        let _ = std::fs::write(f, t);
    }
}

pub fn settings() -> Settings {
    STATE.lock().settings.clone()
}

pub fn set_word_filter(on: bool, words: Vec<String>) {
    let mut s = STATE.lock();
    s.settings.word_filter = on;
    s.settings.words = words.into_iter().map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty()).collect();
    save(&s);
}

pub fn hide(platform: &str, user_id: &str, name: &str) {
    if user_id.is_empty() {
        return;
    }
    let mut s = STATE.lock();
    s.settings.hidden.insert(Hidden {
        platform: platform.to_string(),
        user_id: user_id.to_string(),
        name: name.to_string(),
    });
    save(&s);
}

pub fn unhide(platform: &str, user_id: &str) {
    let mut s = STATE.lock();
    s.settings.hidden.retain(|h| !(h.platform == platform && h.user_id == user_id));
    save(&s);
}

/// Whether a message's author is hidden.
pub fn is_hidden(platform: &str, user_id: &str) -> bool {
    !user_id.is_empty() && STATE.lock().settings.hidden.iter().any(|h| h.platform == platform && h.user_id == user_id)
}

/// The text with filtered words masked (each letter a •), or as it is with the filter off.
pub fn mask(text: &str) -> String {
    let s = STATE.lock();
    if !s.settings.word_filter {
        return text.to_string();
    }
    let words: Vec<String> = BUILT_IN.iter().map(|w| w.to_string()).chain(s.settings.words.iter().cloned()).collect();
    drop(s);
    mask_with(text, &words)
}

fn mask_with(text: &str, words: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let lower = word.to_lowercase();
        let hit = words.iter().any(|w| {
            lower == *w
                || ["s", "es", "ed", "er", "ers", "ing"]
                    .iter()
                    .any(|suffix| lower.strip_suffix(suffix) == Some(w.as_str()))
        });
        if hit {
            out.extend(word.chars().map(|_| '•'));
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '\'' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A chat event as shown: `None` when its author is hidden; its text and text fragments masked.
pub fn apply(mut e: lightair_live::ChatEvent) -> Option<lightair_live::ChatEvent> {
    if let Some(u) = &e.user
        && is_hidden(&e.platform, &u.id)
    {
        return None;
    }
    e.text = mask(&e.text);
    for f in &mut e.fragments {
        if let lightair_live::Fragment::Text { text } = f {
            *text = mask(text);
        }
    }
    Some(e)
}

/// Where to report someone on their platform (the page with the platform's own Report), if
/// known: their channel or profile page.
pub fn report_url(platform: &str, user_id: &str, login: &str) -> Option<String> {
    let enc = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect::<String>();
    match platform {
        "twitch" if !login.is_empty() => Some(format!("https://www.twitch.tv/{}", enc(login))),
        "youtube" if !user_id.is_empty() => Some(format!("https://www.youtube.com/channel/{}", enc(user_id))),
        "kick" if !login.is_empty() => Some(format!("https://kick.com/{}", enc(login))),
        "tiktok" if !login.is_empty() => Some(format!("https://www.tiktok.com/@{}", enc(login))),
        "x" | "twitter" if !login.is_empty() => Some(format!("https://x.com/{}", enc(login))),
        "facebook" if !user_id.is_empty() => Some(format!("https://www.facebook.com/{}", enc(user_id))),
        "instagram" if !login.is_empty() => Some(format!("https://www.instagram.com/{}", enc(login))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_masked_whole_and_people_hidden_by_id() {
        let words = vec!["darn".to_string()];
        assert_eq!(mask_with("Darn it, darned!", &words), "•••• it, ••••••!");
        assert_eq!(mask_with("adarnable", &words), "adarnable", "whole words only");
        let tmp = tempfile::tempdir().unwrap();
        open(tmp.path());
        set_word_filter(true, vec![" Heck ".into()]);
        assert_eq!(mask("what the heck"), "what the ••••");
        assert_eq!(mask("well, shit"), "well, ••••", "built in");
        set_word_filter(false, vec![]);
        assert_eq!(mask("well, shit"), "well, shit");
        hide("twitch", "42", "Troll");
        assert!(is_hidden("twitch", "42") && !is_hidden("youtube", "42"));
        // kept in the app's folder
        open(tmp.path());
        assert!(is_hidden("twitch", "42"));
        unhide("twitch", "42");
        assert!(!is_hidden("twitch", "42"));
        assert_eq!(report_url("twitch", "1", "some_one").as_deref(), Some("https://www.twitch.tv/some_one"));
        assert_eq!(report_url("douyu", "1", "x"), None);
    }
}
