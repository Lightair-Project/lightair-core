//! Made-up events to try a widget or an automation with (the Inspector's and Activity panel's
//! test buttons). They carry `test`, so session totals and automations that skip tests leave
//! them out.

use std::sync::atomic::{AtomicU64, Ordering};

use lightair_live::event::text_fragments;
use lightair_live::{ChatEvent, Kind, Paid, Role, User};

pub const KINDS: &[(&str, &str)] = &[
    ("follow", "Follow"),
    ("sub", "Subscription"),
    ("paid", "Donation"),
    ("cheer", "Cheer"),
    ("raid", "Raid"),
    ("message", "Chat message"),
];

const NAMES: &[&str] = &["NightOwl", "pixel_pat", "Mochi", "TurboTaco", "Sora", "lumen", "Kitsune", "Bramble"];
const LINES: &[&str] = &[
    "Love the stream!",
    "Hello from Jakarta 👋",
    "That play was clean",
    "GG everyone",
    "First time here, this is great",
];

/// A test event of `kind` ("follow", "sub", "paid", "cheer", "raid", "message") on `platform`.
pub fn make(kind: &str, platform: &str) -> ChatEvent {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as usize;
    let name = NAMES[n % NAMES.len()];
    let line = LINES[n % LINES.len()];
    let (k, paid, headline, text) = match kind {
        "follow" => (Kind::Follow, None, None, ""),
        "sub" => (Kind::Membership, None, Some(format!("{name} subscribed for {} months", 1 + n % 12)), line),
        "paid" => {
            let amount = [5.0, 10.0, 25.0, 50.0][n % 4];
            (Kind::Paid, Some(Paid { display: format!("${amount:.2}"), amount, currency: "USD".into() }), None, line)
        }
        "cheer" => {
            let amount = [100.0, 500.0, 1000.0][n % 3];
            (Kind::Paid, Some(Paid { display: format!("{amount} bits"), amount, currency: "BITS".into() }), None, line)
        }
        "raid" => (Kind::Raid { viewers: [12, 48, 230][n % 3] }, None, None, ""),
        _ => (Kind::Message, None, None, line),
    };
    let mut e = ChatEvent::new(platform, "test", k).with_fragments(text_fragments(text));
    e.user = Some(User {
        id: format!("test-{n}"),
        login: name.to_lowercase(),
        display_name: name.into(),
        color: Some(["#FF7A59", "#59C3FF", "#B57BFF", "#5BE38A"][n % 4].into()),
        roles: if n.is_multiple_of(3) { vec![Role::Subscriber] } else { Vec::new() },
        badges: Vec::new(),
    });
    e.paid = paid;
    e.headline = headline;
    e.native_id = Some(format!("test-{n}"));
    e.test = true;
    e
}
