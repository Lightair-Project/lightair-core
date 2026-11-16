//! Signing in to platforms (docs/plans/live-integrations.md, 2.10): in a window of Lightair's
//! own browser, on the platform's own sign-in page (no app approval, no passwords through
//! Lightair). The browser keeps the session; sending chat uses it.

use std::sync::Arc;

use crate::pages;

pub struct Account {
    pub platform: &'static str,
    /// Where to sign in.
    pub sign_in: &'static str,
    /// A cookie that's there once signed in: (address, name).
    pub cookie: (&'static str, &'static str),
}

pub const ACCOUNTS: &[Account] = &[
    Account { platform: "twitch", sign_in: "https://www.twitch.tv/login", cookie: ("https://www.twitch.tv", "auth-token") },
    Account {
        platform: "youtube",
        sign_in: "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F",
        cookie: ("https://www.youtube.com", "SAPISID"),
    },
    Account { platform: "kick", sign_in: "https://kick.com/", cookie: ("https://kick.com", "session_token") },
    Account { platform: "tiktok", sign_in: "https://www.tiktok.com/login", cookie: ("https://www.tiktok.com", "sessionid") },
];

pub fn account(platform: &str) -> Option<&'static Account> {
    let platform = if platform == "youtube-shorts" { "youtube" } else { platform };
    ACCOUNTS.iter().find(|a| a.platform == platform)
}

/// Signed in on `platform` (looked up in the browser's cookies: a moment).
pub fn signed_in(platform: &str) -> bool {
    account(platform).is_some_and(|a| pages::cookie(a.cookie.0, a.cookie.1).is_some())
}

/// Opens the platform's sign-in page in a window; `done` runs when it's closed.
pub fn sign_in(platform: &str, done: Arc<dyn Fn() + Send + Sync>) -> Result<Box<dyn pages::Page>, String> {
    let a = account(platform).ok_or_else(|| format!("signing in to {platform} isn't supported yet"))?;
    let host = pages::host()?;
    Ok(host.open_window(a.sign_in, &format!("Sign in to {platform} · Lightair"), done))
}
