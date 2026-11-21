//! Reads chat from live channels for a while and prints it:
//! `cargo run -p lightair-live --example read_chat -- twitch:monstercat youtube:@LofiGirl 20`
use lightair_live::{Channel, Hub};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: u64 = args.last().and_then(|a| a.parse().ok()).unwrap_or(20);
    let channels: Vec<Channel> = args
        .iter()
        .filter_map(|a| a.split_once(':'))
        .map(|(p, c)| Channel { platform: p.into(), channel: c.into() })
        .collect();
    let hub = Hub::new();
    hub.set_channels(&channels);
    let mut seen = 0;
    for _ in 0..secs * 2 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        for e in hub.since(seen) {
            seen = e.id;
            let who = e.user.as_ref().map_or("", |u| u.display_name.as_str());
            println!(
                "[{}] {:?} {who}: {} {}",
                e.platform,
                e.kind,
                e.text,
                e.paid.as_ref().map_or(String::new(), |p| p.display.clone())
            );
        }
    }
    for (c, s) in hub.status() {
        println!("status {} {}: {:?}", c.platform, c.channel, s);
    }
}
