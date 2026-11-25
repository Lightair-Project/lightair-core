//! The RTMP output into ffmpeg's own RTMP server: the publish handshake, H.264 and AAC
//! in FLV tags, and a file ffprobe reads back with both streams.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{ffmpeg, material, probe, scratch};
use lightair_feed::ts::split_nals;
use lightair_feed::{NalFormat, RtmpSender, RtmpTarget, VideoCodec};

#[test]
fn studio_publishes_to_an_rtmp_server() {
    publish(false);
}

/// An encoder gives its parameter sets with its first keyframe, after a local server has
/// already taken the connection: the sequence header must still go before that keyframe.
#[test]
fn the_sequence_header_follows_parameter_sets_that_come_after_connecting() {
    publish(true);
}

fn publish(late_config: bool) {
    if !ffmpeg() {
        eprintln!("no ffmpeg: skipped");
        return;
    }
    let (video, audio) = material(VideoCodec::H264, "rtmp");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = scratch(if late_config {
        "rtmp-received-late.flv"
    } else {
        "rtmp-received.flv"
    });
    let _ = std::fs::remove_file(&out);
    let mut server = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-listen",
            "1",
            "-i",
            &format!("rtmp://127.0.0.1:{port}/live/secret-key"),
            "-c",
            "copy",
        ])
        .arg(&out)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));

    let sender = RtmpSender::start(RtmpTarget {
        url: format!("rtmp://127.0.0.1:{port}/live"),
        key: "secret-key".into(),
        bind: None,
    })
    .unwrap();
    let first = &video[0].0;
    let nals: Vec<Vec<u8>> = split_nals(first, NalFormat::LengthPrefixed)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    if !late_config {
        sender.parameter_sets(&nals);
        sender.audio_config(48000, 2);
    }
    let start = Instant::now();
    while !sender.stats().connected {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "not connected: {:?}",
            sender.stats().error
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    if late_config {
        sender.parameter_sets(&nals);
        sender.audio_config(48000, 2);
    }

    // picture and sound in time order
    let (mut v, mut a) = (0usize, 0usize);
    while v < video.len() || a < audio.len() {
        let tv = v as u64 * 33_333;
        let ta = a as u64 * 1024 * 1_000_000 / 48_000;
        if v < video.len() && (a >= audio.len() || tv <= ta) {
            sender.video(&video[v].0, NalFormat::LengthPrefixed, tv, tv, video[v].1);
            v += 1;
        } else {
            sender.audio(&audio[a], ta);
            a += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(sender.stats().dropped, 0);
    drop(sender);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = server.try_wait().unwrap() {
            assert!(status.success(), "ffmpeg failed");
            break;
        }
        if Instant::now() > deadline {
            let _ = server.kill();
            panic!("ffmpeg didn't finish");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let probed = probe(&out);
    assert!(
        probed.contains("h264") && probed.contains("aac"),
        "{probed}"
    );
    let frames: u32 = probed
        .lines()
        .find(|l| l.starts_with("h264"))
        .and_then(|l| l.split(',').nth(1))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    assert!(frames >= 55, "{probed}");
}
