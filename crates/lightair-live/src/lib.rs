//! Live platforms (docs/plans/live-integrations.md): one event model for every platform's chat
//! and audience events ([`event`]), the hub that reads channels and keeps what they said
//! ([`hub`]), and a reader per platform.
//!
//! Readers need no developer registration or platform approval: Twitch's guest chat, the feed
//! YouTube's own chat page uses, CHZZK's chat socket. Logged-in features (sending, moderation)
//! and the platforms read through their own web pages come next.

pub mod emotes;
pub mod event;
pub mod hub;
pub mod images;
pub mod integrations;
mod net;
pub mod pages;
mod proto;

pub mod accounts;
pub mod chzzk;
pub mod donations;
pub mod douyu;
pub mod kick;
pub mod send;
pub mod showroom;
pub mod soop;
pub mod tiktok;
pub mod twitcasting;
pub mod twitch;
pub mod youtube;

pub use event::{Channel, ChatEvent, Fragment, Kind, Paid, Role, Status, User};
pub use hub::{Hub, Sink};
pub use images::Images;

/// The platforms whose chat Lightair reads (the Outputs list's ids).
pub const READABLE: &[&str] = &["twitch", "youtube", "chzzk", "douyu", "soop", "kick", "tiktok", "twitcasting", "showroom"];

/// The reader for a platform, if Lightair has one.
pub fn reader_for(platform: &str) -> Option<hub::Reader> {
    Some(match platform {
        "twitch" => twitch::read,
        "youtube" | "youtube-shorts" => youtube::read,
        "chzzk" => chzzk::read,
        "douyu" => douyu::read,
        "soop" => soop::read,
        "kick" => kick::read,
        "tiktok" => tiktok::read,
        "twitcasting" => twitcasting::read,
        "showroom" => showroom::read,
        "streamlabs" => donations::streamlabs,
        "streamelements" => donations::streamelements,
        "kofi" => donations::kofi,
        _ => return None,
    })
}

/// How to name the channel for a platform's chat, for the field that asks for it.
pub fn channel_hint(platform: &str) -> Option<&'static str> {
    Some(match platform {
        "twitch" => "Channel name (twitch.tv/…)",
        "youtube" | "youtube-shorts" => "@handle, channel or video link",
        "chzzk" => "Channel link or id (chzzk.naver.com/…)",
        "douyu" => "Room link or number (douyu.com/…)",
        "soop" => "Streamer id or link (sooplive.co.kr/…)",
        "kick" => "Channel name (kick.com/…)",
        "tiktok" => "@username (tiktok.com/@…)",
        "twitcasting" => "User id (twitcasting.tv/…)",
        "showroom" => "Room (showroom-live.com/r/…)",
        _ => return None,
    })
}
