//! Reads a live channel through the hub, as the app runs it, and prints what came.
//! `LIGHTAIR_LIVE_CHECK=platform:channel [LIGHTAIR_LIVE_SECONDS=20] cargo run -p lightair-live --example live_check`

fn main() {
    let spec = std::env::var("LIGHTAIR_LIVE_CHECK").expect("LIGHTAIR_LIVE_CHECK=platform:channel");
    let (platform, channel) = spec.split_once(':').expect("platform:channel");
    let secs = std::env::var("LIGHTAIR_LIVE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let hub = lightair_live::Hub::new();
    let c = lightair_live::Channel { platform: platform.into(), channel: channel.into() };
    hub.set_channels(std::slice::from_ref(&c));
    std::thread::sleep(std::time::Duration::from_secs(secs));
    for e in hub.history() {
        let who = e.user.as_ref().map(|u| u.display_name.clone()).unwrap_or_default();
        println!("{:?} {who}: {} {}", e.kind, e.headline.unwrap_or_default(), e.text);
    }
    println!("status {:?}, {} events", hub.status().get(&c), hub.history().len());
}
