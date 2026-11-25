//! Alerts: follows, subs, raids, cheers, paid messages and redeems from the chat hub,
//! each shown one at a time for its duration (the alert box widget draws the one on now) and
//! handed to the host for its sound and text-to-speech, which come back on the alerts feed (a
//! track in the mix). The rules are kept with the project (`alerts.json`).

use std::path::Path;

use lightair_live::ChatEvent;
use serde::{Deserialize, Serialize};

pub const KINDS: &[&str] = &["follow", "sub", "raid", "cheer", "paid", "redeem"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub kind: String,
    pub enabled: bool,
    /// Paid and cheers: the least amount (in the platform's units); raids: the fewest viewers.
    pub min_amount: f64,
    /// `{name}`, `{amount}`, `{viewers}`, `{message}` are filled in.
    pub text: String,
    pub duration_ms: u32,
    /// Speak the viewer's message (paid, cheers, redeems) after the alert's own line.
    pub speak: bool,
    /// A sound file to play (the project's `assets/…`, or none).
    pub sound: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub rules: Vec<Rule>,
}

impl Default for Settings {
    fn default() -> Self {
        let rule = |kind: &str, text: &str| Rule {
            kind: kind.into(),
            enabled: true,
            min_amount: 0.0,
            text: text.into(),
            duration_ms: 6000,
            speak: matches!(kind, "paid" | "cheer"),
            sound: None,
        };
        Settings {
            rules: vec![
                rule("follow", "{name} followed!"),
                rule("sub", "{name} subscribed!"),
                rule("raid", "{name} is raiding with {viewers} viewers!"),
                rule("cheer", "{name} cheered {amount}!"),
                rule("paid", "{name} sent {amount}!"),
                rule("redeem", "{name} redeemed {message}"),
            ],
        }
    }
}

impl Settings {
    pub fn load(folder: &Path) -> Settings {
        std::fs::read(folder.join("alerts.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, folder: &Path) -> std::io::Result<()> {
        let tmp = folder.join("alerts.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, folder.join("alerts.json"))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub id: u64,
    pub kind: String,
    /// Who it's from.
    pub name: String,
    /// The rule's line, filled in ("Ana sent $5.00!").
    pub title: String,
    /// The viewer's own message, if any.
    pub message: String,
    pub duration_ms: u32,
    /// What to speak (the title, then the message where the rule says so).
    pub speak: Option<String>,
    pub sound: Option<String>,
}

/// The alerts waiting for the alert box, at most `Queue::MAX`: past that the oldest waiting goes
/// (a raid or a gift flood would otherwise be shown for minutes after the moment, and the
/// broadcast extension must stay inside iOS's memory limit for extensions).
#[derive(Debug, Default)]
pub struct Queue(std::collections::VecDeque<Alert>);

impl Queue {
    /// About two to four minutes of alerts.
    pub const MAX: usize = 30;

    pub fn push(&mut self, a: Alert) {
        self.0.push_back(a);
        while self.0.len() > Self::MAX {
            self.0.pop_front();
        }
    }

    /// The next to show.
    pub fn pop(&mut self) -> Option<Alert> {
        self.0.pop_front()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The alert an event makes under `settings`, if any.
pub fn from_event(e: &ChatEvent, settings: &Settings) -> Option<Alert> {
    let kind = e.alert_kind()?;
    let rule = settings
        .rules
        .iter()
        .find(|r| r.kind == kind && r.enabled)?;
    let amount = e.paid.as_ref().map(|p| p.amount).unwrap_or(0.0);
    let viewers = match &e.kind {
        lightair_live::Kind::Raid { viewers } => *viewers,
        _ => 0,
    };
    let measured = if kind == "raid" {
        viewers as f64
    } else {
        amount
    };
    if measured < rule.min_amount {
        return None;
    }
    let name = if e.name().is_empty() {
        "Someone"
    } else {
        e.name()
    };
    let title = rule
        .text
        .replace("{name}", name)
        .replace(
            "{amount}",
            e.paid.as_ref().map_or("", |p| p.display.as_str()),
        )
        .replace("{viewers}", &viewers.to_string())
        .replace("{message}", &e.text);
    let speak = Some(if rule.speak && !e.text.trim().is_empty() {
        format!("{title} {}", e.text.trim())
    } else {
        title.clone()
    });
    Some(Alert {
        id: e.id,
        kind: kind.into(),
        name: name.into(),
        title,
        message: e.text.clone(),
        duration_ms: rule.duration_ms.clamp(1000, 60_000),
        speak,
        sound: rule.sound.clone(),
    })
}

/// A made-up event of `kind` (the settings' Test buttons).
pub fn test_event(kind: &str) -> ChatEvent {
    use lightair_live::{Kind, Paid, User};
    let (k, paid) = match kind {
        "raid" => (Kind::Raid { viewers: 42 }, None),
        "sub" => (Kind::Membership, None),
        "cheer" => (
            Kind::Paid,
            Some(Paid {
                display: "500 bits".into(),
                amount: 500.0,
                currency: "BITS".into(),
            }),
        ),
        "paid" => (
            Kind::Paid,
            Some(Paid {
                display: "$5.00".into(),
                amount: 5.0,
                currency: "USD".into(),
            }),
        ),
        "redeem" => (
            Kind::Notice,
            Some(Paid {
                display: "Hydrate".into(),
                amount: 100.0,
                currency: "CHANNEL_POINTS".into(),
            }),
        ),
        _ => (Kind::Follow, None),
    };
    let mut e = ChatEvent::new("twitch", "test", k);
    e.paid = paid;
    e.test = true;
    e.text = if matches!(kind, "paid" | "cheer") {
        "Great stream!".into()
    } else if kind == "redeem" {
        "Hydrate".into()
    } else {
        String::new()
    };
    e.user = Some(User {
        id: "test".into(),
        login: "lightair".into(),
        display_name: "Lightair".into(),
        color: None,
        roles: vec![],
        badges: vec![],
    });
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raid or a gift flood: the app and the broadcast extension (iOS's memory limit) both keep
    /// only the newest waiting.
    #[test]
    fn a_flood_of_alerts_keeps_the_newest_waiting() {
        let mut q = Queue::default();
        let s = Settings::default();
        for i in 0..=80 {
            let mut e = test_event("follow");
            if let Some(u) = &mut e.user {
                u.display_name = format!("viewer{i}");
            }
            q.push(from_event(&e, &s).unwrap());
        }
        let shown: Vec<String> = std::iter::from_fn(|| q.pop()).map(|a| a.title).collect();
        assert_eq!(shown.len(), Queue::MAX, "the oldest waiting dropped");
        // 81 came in: the first shown is the 51st, the last the newest, in order
        assert!(shown.first().unwrap().contains("viewer51"), "{shown:?}");
        assert!(shown.last().unwrap().contains("viewer80"), "{shown:?}");
    }

    #[test]
    fn events_become_alerts_by_the_rules() {
        let mut s = Settings::default();
        // the template filled in: who, how much, and the message is read out
        let a = from_event(&test_event("paid"), &s).unwrap();
        assert!(
            a.title.contains("Lightair") && a.title.contains("$5.00"),
            "{}",
            a.title
        );
        assert!(
            a.speak
                .as_deref()
                .is_some_and(|t| t.contains("Great stream!")),
            "{:?}",
            a.speak
        );
        let raid = from_event(&test_event("raid"), &s).unwrap().title;
        assert!(raid.contains("42"), "{raid}");
        // under the minimum, or off: nothing
        s.rules
            .iter_mut()
            .find(|r| r.kind == "paid")
            .unwrap()
            .min_amount = 10.0;
        assert!(from_event(&test_event("paid"), &s).is_none());
        s.rules
            .iter_mut()
            .find(|r| r.kind == "follow")
            .unwrap()
            .enabled = false;
        assert!(from_event(&test_event("follow"), &s).is_none());
        // a plain message is never an alert
        let m = ChatEvent::new("twitch", "x", lightair_live::Kind::Message);
        assert!(from_event(&m, &Settings::default()).is_none());
        // kept with the project
        let tmp = tempfile::tempdir().unwrap();
        s.save(tmp.path()).unwrap();
        assert_eq!(Settings::load(tmp.path()), s);
    }
}
