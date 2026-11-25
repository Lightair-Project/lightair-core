//! The feed over real SRT: an srt-tokio listener stands in for the desktop's Phone camera layer
//! (it checks the stream id and the passphrase, as the desktop will), the feed sends two seconds
//! of ffmpeg's H.264 and AAC at the pace a camera would, and ffprobe counts what arrived. A
//! wrong passphrase doesn't get in. Skipped where ffmpeg isn't installed.

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{ffmpeg, material, probe, scratch};
use futures::StreamExt;
use lightair_feed::{AudioConfig, Feed, NalFormat, SrtTarget, VideoCodec};
use srt_protocol::options::KeySize;
use srt_protocol::settings::{KeySettings, Passphrase};
use srt_tokio::SrtListener;

/// What the "desktop" got: the stream ids it accepted, and the bytes.
#[derive(Default)]
struct Received {
    ids: std::sync::Mutex<Vec<String>>,
    bytes: std::sync::Mutex<Vec<u8>>,
}

const PASSPHRASE: &str = "phone-camera-1-session";

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The desktop's listener for one source, on its own thread; returns once it's bound, so a
/// sender started after this connects on its first try (no reconnect backoff in the way).
fn listen(port: u16, got: Arc<Received>) {
    let (bound, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let (_server, mut incoming) = SrtListener::builder().bind(port).await.unwrap();
            let _ = bound.send(());
            while let Some(request) = incoming.incoming().next().await {
                let id = request
                    .stream_id()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let key = KeySettings {
                    key_size: KeySize::AES128,
                    passphrase: Passphrase::from(PASSPHRASE),
                };
                let got = got.clone();
                tokio::spawn(async move {
                    let Ok(mut socket) = request.accept(Some(key)).await else {
                        return;
                    };
                    got.ids.lock().unwrap().push(id);
                    while let Some(Ok((_, data))) = socket.next().await {
                        got.bytes.lock().unwrap().extend_from_slice(&data);
                    }
                });
            }
        });
    });
    ready.recv().expect("the listener bound its port");
}

fn target(port: u16, passphrase: &str) -> SrtTarget {
    SrtTarget {
        address: SocketAddr::from(([127, 0, 0, 1], port)),
        source: "cam-1".into(),
        passphrase: passphrase.into(),
        latency: Duration::from_millis(40),
        stream_id: None,
        bind: None,
    }
}

fn wait(what: &str, ok: impl Fn() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_camera_feed_reaches_the_desktop_over_srt() {
    if !ffmpeg() {
        eprintln!("ffmpeg isn't installed: skipped");
        return;
    }
    let (units, sounds) = material(VideoCodec::H264, "srt");
    let port = free_port();
    let got = Arc::new(Received::default());
    listen(port, got.clone());

    let feed = Feed::srt(
        target(port, PASSPHRASE),
        VideoCodec::H264,
        Some(AudioConfig {
            sample_rate: 48000,
            channels: 2,
        }),
    )
    .unwrap();
    wait("the connection", || feed.stats().connected);
    assert!(
        feed.take_keyframe_request(),
        "a new connection asks for a keyframe"
    );
    assert_eq!(
        got.ids.lock().unwrap().as_slice(),
        ["#!::r=cam-1,m=publish"]
    );

    // at a camera's pace: each frame when it's due
    let start = Instant::now();
    let (mut v, mut a) = (0, 0);
    while v < units.len() || a < sounds.len() {
        let vt = v as u64 * 1_000_000 / 30;
        let at = a as u64 * 1024 * 1_000_000 / 48000;
        let (t, video) = if v < units.len() && (a >= sounds.len() || vt <= at) {
            (vt, true)
        } else {
            (at, false)
        };
        if let Some(wait) = Duration::from_micros(t).checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
        if video {
            feed.video(&units[v].0, NalFormat::LengthPrefixed, vt, units[v].1);
            v += 1;
        } else {
            feed.audio(&sounds[a], at);
            a += 1;
        }
    }
    let sent = feed.stats();
    assert_eq!(sent.dropped, 0, "{sent:?}");
    assert!(sent.bitrate_kbps > 0, "{sent:?}");
    std::thread::sleep(Duration::from_millis(500)); // the latency window, and a little more
    drop(feed);

    let out = scratch("received.ts");
    std::fs::write(&out, got.bytes.lock().unwrap().as_slice()).unwrap();
    let probe = probe(&out);
    assert!(
        probe.lines().any(|l| l == format!("h264,{}", units.len())),
        "{probe}"
    );
    assert!(probe.lines().any(|l| l.starts_with("aac,")), "{probe}");
}

#[test]
fn a_wrong_passphrase_doesnt_get_in() {
    let port = free_port();
    let got = Arc::new(Received::default());
    listen(port, got.clone());
    let feed = Feed::srt(target(port, "not-the-passphrase"), VideoCodec::H264, None).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    feed.video(
        &[0, 0, 0, 2, 0x65, 0x88],
        NalFormat::LengthPrefixed,
        0,
        true,
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(got.bytes.lock().unwrap().is_empty());
}

#[test]
fn each_connection_starts_at_a_keyframe() {
    // nobody listening: nothing is sent, not even a keyframe
    let mut alone = lightair_feed::SrtSender::start(target(free_port(), PASSPHRASE)).unwrap();
    assert!(!alone.begin_picture(true), "nobody to send to yet");
    drop(alone);
    // the listener first, so the sender connects on its first try
    let port = free_port();
    let got = Arc::new(Received::default());
    listen(port, got.clone());
    let mut sender = lightair_feed::SrtSender::start(target(port, PASSPHRASE)).unwrap();
    let start = Instant::now();
    while !sender.stats().connected {
        // a first try's handshake takes milliseconds; the bound only catches a broken sender
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the connection never came"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !sender.begin_picture(false),
        "a delta picture first: left out"
    );
    assert!(sender.waiting_for_keyframe());
    assert!(sender.take_keyframe_request());
    assert!(sender.begin_picture(true));
    assert!(
        sender.begin_picture(false),
        "after the keyframe, everything"
    );
    assert!(!sender.waiting_for_keyframe());
}
