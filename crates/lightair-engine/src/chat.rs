//! Chat: lightair-core's hub, one reader per channel (the signed-in accounts' and those
//! typed in), every platform in one list; sending as the signed-in account.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lightair_live::{Channel, ChatEvent, Hub, Status};

pub struct Chat {
    pub hub: Hub,
    /// The last event handed to the interface.
    seen: AtomicU64,
}

/// The app's chat hub (one: sessions come one at a time, and the widget server listens to it).
pub fn hub() -> Hub {
    static HUB: std::sync::OnceLock<Hub> = std::sync::OnceLock::new();
    HUB.get_or_init(Hub::new).clone()
}

impl Default for Chat {
    fn default() -> Self {
        let hub = hub();
        // a session hears what's said from now on (the hub outlives sessions)
        let seen = hub.history().last().map_or(0, |e| e.id);
        Chat { hub, seen: AtomicU64::new(seen) }
    }
}

impl Chat {
    pub fn set_channels(&self, channels: &[Channel]) {
        self.hub.set_channels(channels);
    }

    pub fn channels(&self) -> Vec<(Channel, Status)> {
        let mut v: Vec<(Channel, Status)> = self.hub.status().into_iter().collect();
        v.sort_by(|a, b| (&a.0.platform, &a.0.channel).cmp(&(&b.0.platform, &b.0.channel)));
        v
    }

    /// What's new since the last call (the control thread's, 10 times a second at most).
    pub fn take_new(&self) -> Vec<ChatEvent> {
        let seen = self.seen.load(Ordering::Acquire);
        let new = self.hub.since(seen);
        if let Some(last) = new.last() {
            self.seen.store(last.id, Ordering::Release);
        }
        new
    }
}

/// Sends `text` to every channel read (or only `platform`'s), as the signed-in account, on a
/// thread of its own; `failed` hears what couldn't be sent.
pub fn send(chat: &Arc<Chat>, text: String, platform: Option<String>, failed: impl Fn(String) + Send + 'static) {
    let targets: Vec<Channel> = chat
        .channels()
        .into_iter()
        .map(|(c, _)| c)
        .filter(|c| platform.as_ref().is_none_or(|p| *p == c.platform))
        .collect();
    let _ = std::thread::Builder::new().name("lightair-chat-send".into()).spawn(move || {
        for c in targets {
            if let Err(e) = lightair_live::send::send(&c.platform, &c.channel, &text, None) {
                failed(format!("Not sent to {}: {e}", c.platform));
            }
        }
    });
}
