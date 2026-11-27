//! Says a text with a profile's voice and writes the sound to a WAV file: `cargo run -p
//! lightair-voice --example say -- <vendor> <out.wav> <text…>` (the key from `LIGHTAIR_VOICE_KEY`).

use std::sync::mpsc::channel;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vendor = lightair_voice::Vendor::parse(args.first().map_or("system", String::as_str)).unwrap_or_default();
    let out = args.get(1).cloned().unwrap_or_else(|| "say.wav".into());
    let text = args[2..].join(" ");
    let profile = lightair_voice::Profile { vendor, ..Default::default() };
    let voice = lightair_voice::Voice::new(&profile, std::env::var("LIGHTAIR_VOICE_KEY").ok()).expect("a voice");
    let (tx, rx) = channel();
    let started = std::time::Instant::now();
    let _u = voice.say_all(
        &text,
        Box::new(move |e| {
            let _ = tx.send(e);
        }),
    );
    let (mut pcm, mut rate, mut first) = (Vec::new(), 24_000, None);
    for e in rx {
        match e {
            lightair_voice::Event::Audio(c) => {
                first.get_or_insert(started.elapsed());
                rate = c.rate;
                pcm.extend(c.pcm);
            }
            lightair_voice::Event::Done => break,
            lightair_voice::Event::Failed(why) => panic!("{why}"),
        }
    }
    std::fs::write(&out, lightair_voice::write_wav(&pcm, rate)).expect("written");
    println!("{} samples at {rate} Hz, first sound after {:?}", pcm.len(), first.unwrap_or_default());
}
