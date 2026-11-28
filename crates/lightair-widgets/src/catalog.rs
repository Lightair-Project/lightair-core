//! Lightair's own widgets, built on the same bridge any widget uses. The counter and the
//! subathon timer keep their state here (in the widget's storage), not in their pages: pages
//! only exist while on air, and a sub during a break still adds time.

use lightair_live::ChatEvent;
use serde_json::{Map, Value, json};

pub struct Builtin {
    pub kind: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
    pub about: &'static str,
    /// Its page's size (CSS pixels).
    pub size: (u32, u32),
    /// Where it's put on the canvas: across and down the room left (0: the left or top edge,
    /// 1: the right or bottom).
    pub place: (f32, f32),
    /// It plays sounds (its page gets a mixer track).
    pub sound: bool,
    pub fields: &'static str,
    pub page: &'static str,
}

pub const BUILTIN: &[Builtin] = &[
    Builtin {
        kind: "alert-box",
        name: "Alert box",
        icon: "bell",
        about: "Follows, subs, donations, cheers and raids, one at a time",
        size: (1000, 600),
        place: (0.5, 0.45),
        sound: true,
        fields: include_str!("../assets/widgets/alert-box.json"),
        page: include_str!("../assets/widgets/alert-box.html"),
    },
    Builtin {
        kind: "chat-box",
        name: "Chat box",
        icon: "message-square-text",
        about: "Every platform's chat on stream",
        size: (500, 700),
        place: (0.02, 0.95),
        sound: false,
        fields: include_str!("../assets/widgets/chat-box.json"),
        page: include_str!("../assets/widgets/chat-box.html"),
    },
    Builtin {
        kind: "event-list",
        name: "Event list",
        icon: "list",
        about: "The latest follows, subs and donations",
        size: (420, 400),
        place: (0.98, 0.05),
        sound: false,
        fields: include_str!("../assets/widgets/event-list.json"),
        page: include_str!("../assets/widgets/event-list.html"),
    },
    Builtin {
        kind: "goal",
        name: "Goal",
        icon: "target",
        about: "A bar filling up towards a sub, follow or donation goal",
        size: (700, 110),
        place: (0.5, 0.03),
        sound: false,
        fields: include_str!("../assets/widgets/goal.json"),
        page: include_str!("../assets/widgets/goal.html"),
    },
    Builtin {
        kind: "label",
        name: "Label",
        icon: "tag",
        about: "Latest follower, top donation, counts",
        size: (600, 80),
        place: (0.02, 0.03),
        sound: false,
        fields: include_str!("../assets/widgets/label.json"),
        page: include_str!("../assets/widgets/label.html"),
    },
    Builtin {
        kind: "counter",
        name: "Counter",
        icon: "hash",
        about: "A death counter or anything you count",
        size: (500, 110),
        place: (0.02, 0.25),
        sound: false,
        fields: include_str!("../assets/widgets/counter.json"),
        page: include_str!("../assets/widgets/counter.html"),
    },
    Builtin {
        kind: "subathon",
        name: "Subathon timer",
        icon: "hourglass",
        about: "A countdown that subs, follows and donations add time to",
        size: (600, 220),
        place: (0.98, 0.95),
        sound: false,
        fields: include_str!("../assets/widgets/subathon.json"),
        page: include_str!("../assets/widgets/subathon.html"),
    },
    Builtin {
        kind: "now-playing",
        name: "Now playing",
        icon: "music",
        about: "The song playing in Spotify or Apple Music",
        size: (620, 120),
        place: (0.02, 0.97),
        sound: false,
        fields: include_str!("../assets/widgets/now-playing.json"),
        page: include_str!("../assets/widgets/now-playing.html"),
    },
    Builtin {
        kind: "emote-wall",
        name: "Emote wall",
        icon: "sparkles",
        about: "Emotes from chat float up the screen",
        size: (1920, 1080),
        place: (0.5, 0.5),
        sound: false,
        fields: include_str!("../assets/widgets/emote-wall.json"),
        page: include_str!("../assets/widgets/emote-wall.html"),
    },
    Builtin {
        kind: "credits",
        name: "Credits",
        icon: "clapperboard",
        about: "This stream's followers, subscribers and supporters, rolling",
        size: (1920, 1080),
        place: (0.5, 0.5),
        sound: false,
        fields: include_str!("../assets/widgets/credits.json"),
        page: include_str!("../assets/widgets/credits.html"),
    },
    Builtin {
        kind: "social",
        name: "Social rotator",
        icon: "at-sign",
        about: "Your handles, one at a time",
        size: (600, 80),
        place: (0.02, 0.85),
        sound: false,
        fields: include_str!("../assets/widgets/social.json"),
        page: include_str!("../assets/widgets/social.html"),
    },
    Builtin {
        kind: "poll",
        name: "Poll",
        icon: "chart-bar",
        about: "Chat votes by typing a number",
        size: (560, 300),
        place: (0.98, 0.2),
        sound: false,
        fields: include_str!("../assets/widgets/poll.json"),
        page: include_str!("../assets/widgets/poll.html"),
    },
    Builtin {
        kind: "wheel",
        name: "Wheel",
        icon: "disc-3",
        about: "Spin for a winner",
        size: (700, 780),
        place: (0.5, 0.5),
        sound: false,
        fields: include_str!("../assets/widgets/wheel.json"),
        page: include_str!("../assets/widgets/wheel.html"),
    },
    Builtin {
        kind: "leaderboard",
        name: "Leaderboard",
        icon: "trophy",
        about: "The viewers with the most points",
        size: (420, 320),
        place: (0.98, 0.8),
        sound: false,
        fields: include_str!("../assets/widgets/leaderboard.json"),
        page: include_str!("../assets/widgets/leaderboard.html"),
    },
    Builtin {
        kind: "featured",
        name: "Featured message",
        icon: "message-square-quote",
        about: "A chat message you pick (Chat › a message › Feature)",
        size: (900, 220),
        place: (0.03, 0.8),
        sound: false,
        fields: include_str!("../assets/widgets/featured.json"),
        page: include_str!("../assets/widgets/featured.html"),
    },
    Builtin {
        kind: "emote-combo",
        name: "Emote combo",
        icon: "flame",
        about: "The same emote again and again in chat, counted",
        size: (500, 220),
        place: (0.5, 0.15),
        sound: false,
        fields: include_str!("../assets/widgets/emote-combo.json"),
        page: include_str!("../assets/widgets/emote-combo.html"),
    },
    Builtin {
        kind: "prediction",
        name: "Prediction",
        icon: "coins",
        about: "Chat bets points on the outcome (!bet 1 100); winners split the pot",
        size: (560, 320),
        place: (0.98, 0.2),
        sound: false,
        fields: include_str!("../assets/widgets/prediction.json"),
        page: include_str!("../assets/widgets/prediction.html"),
    },
    Builtin {
        kind: "heart-rate",
        name: "Heart rate",
        icon: "heart-pulse",
        about: "Your heart rate from Pulsoid (Settings › Connections)",
        size: (360, 120),
        place: (0.98, 0.03),
        sound: false,
        fields: include_str!("../assets/widgets/heart-rate.json"),
        page: include_str!("../assets/widgets/heart-rate.html"),
    },
    Builtin {
        kind: "scoreboard",
        name: "Scoreboard",
        icon: "trophy",
        about: "Two sides, their scores, a period and a clock",
        size: (900, 120),
        place: (0.5, 0.04),
        sound: false,
        fields: include_str!("../assets/widgets/scoreboard.json"),
        page: include_str!("../assets/widgets/scoreboard.html"),
    },
    Builtin {
        kind: "lower-third",
        name: "Lower third",
        icon: "rectangle-horizontal",
        about: "A name and title that slides in and out",
        size: (1100, 220),
        place: (0.02, 0.92),
        sound: false,
        fields: include_str!("../assets/widgets/lower-third.json"),
        page: include_str!("../assets/widgets/lower-third.html"),
    },
    Builtin {
        kind: "web-data",
        name: "Web data",
        icon: "globe",
        about: "A number or text from a web address (an API), kept up to date",
        size: (700, 120),
        place: (0.5, 0.5),
        sound: false,
        fields: include_str!("../assets/widgets/web-data.json"),
        page: include_str!("../assets/widgets/web-data.html"),
    },
    Builtin {
        kind: "input-overlay",
        name: "Input overlay",
        icon: "keyboard",
        about: "Shortcuts pressed, mouse buttons and a game controller, on screen",
        size: (700, 300),
        place: (0.98, 0.95),
        sound: false,
        fields: include_str!("../assets/widgets/input-overlay.json"),
        page: include_str!("../assets/widgets/input-overlay.html"),
    },
    Builtin {
        kind: "captions",
        name: "Captions",
        icon: "captions",
        about: "What's said, as live captions (and a translation)",
        size: (1400, 220),
        place: (0.5, 0.95),
        sound: false,
        fields: include_str!("../assets/widgets/captions.json"),
        page: include_str!("../assets/widgets/captions.html"),
    },
    Builtin {
        kind: "visualiser",
        name: "Audio visualiser",
        icon: "audio-waveform",
        about: "Bars, a wave or a circle that move with the sound",
        size: (800, 240),
        place: (0.5, 0.9),
        sound: false,
        fields: include_str!("../assets/widgets/visualiser.json"),
        page: include_str!("../assets/widgets/visualiser.html"),
    },
];

pub fn by_kind(kind: &str) -> Option<&'static Builtin> {
    BUILTIN.iter().find(|b| b.kind == kind)
}

pub fn page(kind: &str) -> Option<&'static str> {
    by_kind(kind).map(|b| b.page)
}

/// Alerts wait their turn for it (the host's queue).
pub fn queues_alerts(kind: &str) -> bool {
    kind == "alert-box"
}

fn now_ms() -> i64 {
    lightair_live::event::now_ms()
}

fn num(v: &Map<String, Value>, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0.0)
}

/// What a button does to a built-in widget's storage: the changed keys, or `None` (not one
/// of its buttons: the page gets it).
pub fn button(
    kind: &str,
    name: &str,
    fields: &Map<String, Value>,
    stored: &Map<String, Value>,
) -> Option<Vec<(String, Value)>> {
    match (kind, name) {
        ("counter", "increment" | "decrement" | "reset") => {
            let v = stored.get("value").and_then(Value::as_f64).unwrap_or(0.0);
            let step = num(fields, "step").max(1.0);
            let next = match name {
                "increment" => v + step,
                "decrement" => v - step,
                _ => 0.0,
            };
            Some(vec![("value".into(), json!(next))])
        }
        ("subathon", "start" | "pause" | "add5" | "reset") => {
            let running = stored.get("running").and_then(Value::as_bool).unwrap_or(false);
            let remaining = subathon_remaining(fields, stored);
            Some(match name {
                "start" if !running => {
                    vec![("running".into(), json!(true)), ("endAt".into(), json!(now_ms() + remaining))]
                }
                "pause" if running => vec![("running".into(), json!(false)), ("remaining".into(), json!(remaining))],
                "add5" => subathon_add(fields, stored, 5.0 * 60_000.0, "by hand"),
                "reset" => vec![
                    ("running".into(), json!(false)),
                    ("remaining".into(), json!((num(fields, "startMinutes").max(1.0) * 60_000.0) as i64)),
                    ("endAt".into(), Value::Null),
                ],
                _ => Vec::new(),
            })
        }
        ("scoreboard", _) => {
            let n = |k: &str| stored.get(k).and_then(Value::as_i64).unwrap_or(0);
            let running = stored.get("clockRunning").and_then(Value::as_bool).unwrap_or(false);
            let since = stored.get("clockSince").and_then(Value::as_i64).unwrap_or(0);
            let used = stored.get("clockUsed").and_then(Value::as_i64).unwrap_or(0);
            Some(match name {
                "homeUp" => vec![("home".into(), json!(n("home") + 1))],
                "homeDown" => vec![("home".into(), json!((n("home") - 1).max(0)))],
                "awayUp" => vec![("away".into(), json!(n("away") + 1))],
                "awayDown" => vec![("away".into(), json!((n("away") - 1).max(0)))],
                "resetScore" => vec![("home".into(), json!(0)), ("away".into(), json!(0)), ("period".into(), json!(1))],
                "periodUp" => vec![("period".into(), json!(n("period").max(1) + 1))],
                "clockStart" if !running => {
                    vec![("clockRunning".into(), json!(true)), ("clockSince".into(), json!(now_ms()))]
                }
                "clockPause" if running => {
                    vec![
                        ("clockRunning".into(), json!(false)),
                        ("clockUsed".into(), json!(used + (now_ms() - since).max(0))),
                    ]
                }
                "clockReset" => vec![
                    ("clockRunning".into(), json!(false)),
                    ("clockUsed".into(), json!(0)),
                    ("clockSince".into(), Value::Null),
                ],
                _ => return None,
            })
        }
        ("lower-third", "show") => Some(vec![("shown".into(), json!(true)), ("shownAt".into(), json!(now_ms()))]),
        ("lower-third", "hide") => Some(vec![("shown".into(), json!(false))]),
        _ => None,
    }
}

fn subathon_remaining(fields: &Map<String, Value>, stored: &Map<String, Value>) -> i64 {
    let running = stored.get("running").and_then(Value::as_bool).unwrap_or(false);
    match (running, stored.get("endAt").and_then(Value::as_i64), stored.get("remaining").and_then(Value::as_i64)) {
        (true, Some(end), _) => (end - now_ms()).max(0),
        (_, _, Some(r)) => r,
        _ => (num(fields, "startMinutes").max(1.0) * 60_000.0) as i64,
    }
}

fn subathon_add(fields: &Map<String, Value>, stored: &Map<String, Value>, ms: f64, from: &str) -> Vec<(String, Value)> {
    let max = num(fields, "maxHours") * 3_600_000.0;
    let now = subathon_remaining(fields, stored) as f64;
    let next = if max > 0.0 { (now + ms).min(max) } else { now + ms };
    let added = (next - now).max(0.0) as i64;
    let mut out = vec![("lastAdded".into(), json!({"ms": added, "from": from, "at": now_ms()}))];
    if stored.get("running").and_then(Value::as_bool).unwrap_or(false) {
        out.push(("endAt".into(), json!(now_ms() + next as i64)));
    } else {
        out.push(("remaining".into(), json!(next as i64)));
    }
    out
}

/// What an event does to a subathon timer (time added), if anything.
pub fn subathon_event(
    fields: &Map<String, Value>,
    stored: &Map<String, Value>,
    e: &ChatEvent,
) -> Option<Vec<(String, Value)>> {
    if e.test && !fields.get("countTests").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let minutes = match e.alert_kind()? {
        "sub" => num(fields, "perSub"),
        "follow" => num(fields, "perFollow"),
        "cheer" => num(fields, "perHundredBits") * e.paid.as_ref().map_or(0.0, |p| p.amount) / 100.0,
        // money only (gifts in a platform's own units aren't money)
        "paid" => {
            let p = e.paid.as_ref()?;
            if p.currency.len() != 3 || !p.currency.chars().all(|c| c.is_ascii_uppercase()) {
                return None;
            }
            num(fields, "perMoney") * p.amount
        }
        _ => 0.0,
    };
    (minutes > 0.0).then(|| subathon_add(fields, stored, minutes * 60_000.0, e.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn every_builtin_has_fields() {
        for b in BUILTIN {
            assert!(!crate::fields::parse(b.fields).is_empty(), "{}", b.kind);
            assert!(b.page.contains("/lightair.js"), "{}", b.kind);
        }
    }

    #[test]
    fn counter_buttons() {
        let f = map(json!({"step": 2}));
        let out = button("counter", "increment", &f, &map(json!({"value": 3}))).unwrap();
        assert_eq!(out, vec![("value".into(), json!(5.0))]);
        assert!(button("counter", "other", &f, &Map::new()).is_none());
    }

    #[test]
    fn subathon_adds_time_for_subs_and_money() {
        let f = map(json!({"startMinutes": 60, "perSub": 5, "perMoney": 1, "maxHours": 0}));
        let stored = map(json!({"running": false, "remaining": 600_000}));
        let mut sub = ChatEvent::new("twitch", "c", lightair_live::Kind::Membership);
        sub.user = Some(lightair_live::User { display_name: "fan".into(), ..Default::default() });
        let out = subathon_event(&f, &stored, &sub).unwrap();
        assert!(out.contains(&("remaining".into(), json!(900_000))));
        let mut gift = ChatEvent::new("douyu", "c", lightair_live::Kind::Paid);
        gift.paid = Some(lightair_live::Paid { display: "gift".into(), amount: 3.0, currency: "DOUYU_GIFT_1".into() });
        assert!(subathon_event(&f, &stored, &gift).is_none());
    }
}
