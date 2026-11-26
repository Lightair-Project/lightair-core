//! RTMP's keyframes and times as a server sees them: a new stream doesn't ask the encoder for an
//! extra keyframe (its first picture is one), a connection that comes up after pictures were
//! offered does, and with B-frames the stream's clock starts at the first decode time so every
//! picture keeps its true presentation time.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lightair_feed::{NalFormat, RtmpSender, RtmpTarget};

/// A picture as the server got it: the message's timestamp (the decode time, ms), the tag's
/// composition time (ms), and whether it's a keyframe.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Picture {
    dts: u32,
    cts: i32,
    key: bool,
}

/// A tiny RTMP server for one publish. It answers the handshake once `go` is sent (or at once
/// when `go` is `None`), then accepts the stream and records every coded picture.
fn server(go: Option<Receiver<()>>) -> (String, Arc<Mutex<Vec<Picture>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let got = Arc::new(Mutex::new(Vec::new()));
    let out = got.clone();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        if let Some(go) = go {
            let _ = go.recv();
        }
        let mut c0c1 = vec![0u8; 1537];
        s.read_exact(&mut c0c1).unwrap();
        let mut s0s1s2 = vec![3u8];
        s0s1s2.extend(&c0c1[1..]);
        s0s1s2.extend(&c0c1[1..]);
        s.write_all(&s0s1s2).unwrap();
        let mut c2 = vec![0u8; 1536];
        s.read_exact(&mut c2).unwrap();
        // createStream's answer (transaction 4) and publish's start: the client waits for both
        let result = amf(&[Amf::S("_result"), Amf::N(4.0), Amf::Null, Amf::N(1.0)]);
        let start =
            amf(&[Amf::S("onStatus"), Amf::N(0.0), Amf::Null, Amf::Obj(&[("code", "NetStream.Publish.Start")])]);
        s.write_all(&message(3, 20, &result)).unwrap();
        s.write_all(&message(3, 20, &start)).unwrap();
        read_pictures(s, &out);
    });
    (format!("rtmp://127.0.0.1:{port}/live"), got)
}

enum Amf<'a> {
    S(&'a str),
    N(f64),
    Null,
    Obj(&'a [(&'a str, &'a str)]),
}

fn amf(values: &[Amf]) -> Vec<u8> {
    let mut out = Vec::new();
    let string = |out: &mut Vec<u8>, s: &str| {
        out.extend((s.len() as u16).to_be_bytes());
        out.extend(s.as_bytes());
    };
    for v in values {
        match v {
            Amf::S(s) => {
                out.push(2);
                string(&mut out, s);
            }
            Amf::N(n) => {
                out.push(0);
                out.extend(n.to_be_bytes());
            }
            Amf::Null => out.push(5),
            Amf::Obj(fields) => {
                out.push(3);
                for (k, v) in *fields {
                    string(&mut out, k);
                    out.push(2);
                    string(&mut out, v);
                }
                out.extend([0, 0, 9]);
            }
        }
    }
    out
}

/// One message in a single chunk (bodies under the default 128-byte chunk size).
fn message(csid: u8, kind: u8, body: &[u8]) -> Vec<u8> {
    assert!(body.len() <= 128);
    let mut out = vec![csid];
    out.extend([0, 0, 0]);
    out.extend(&(body.len() as u32).to_be_bytes()[1..]);
    out.push(kind);
    out.extend(0u32.to_le_bytes());
    out.extend(body);
    out
}

/// Reads the client's chunk stream until it closes, recording its coded pictures (video
/// messages whose AVC packet type is 1).
fn read_pictures(mut s: TcpStream, out: &Mutex<Vec<Picture>>) {
    // per chunk stream: timestamp, length, kind, and the message so far
    let mut streams: std::collections::HashMap<u32, (u32, usize, u8, Vec<u8>)> = Default::default();
    let mut chunk = 128usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut read = [0u8; 16384];
    loop {
        // whole chunks out of what's in
        while let Some(&first) = buf.first() {
            let fmt = first >> 6;
            let (csid, mut at) = match first & 0x3F {
                0 if buf.len() > 1 => (64 + buf[1] as u32, 2),
                1 if buf.len() > 2 => (64 + buf[1] as u32 + 256 * buf[2] as u32, 3),
                0 | 1 => break,
                n => (n as u32, 1),
            };
            let header = [11, 7, 3, 0][fmt as usize];
            if buf.len() < at + header {
                break;
            }
            let u24 = |b: &[u8]| u32::from_be_bytes([0, b[0], b[1], b[2]]);
            let (mut ts, mut len, mut kind, so_far) = streams.get(&csid).cloned().unwrap_or((0, 0, 0, Vec::new()));
            let fresh = so_far.is_empty();
            let mut stamp = None;
            if fmt <= 2 {
                stamp = Some(u24(&buf[at..]));
            }
            if fmt <= 1 {
                len = u24(&buf[at + 3..]) as usize;
                kind = buf[at + 6];
            }
            at += header;
            if stamp == Some(0xFF_FFFF) {
                if buf.len() < at + 4 {
                    break;
                }
                stamp = Some(u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()));
                at += 4;
            }
            if let Some(t) = stamp
                && fresh
            {
                ts = if fmt == 0 { t } else { ts.wrapping_add(t) };
            }
            let take = (len - so_far.len()).min(chunk);
            if buf.len() < at + take {
                break;
            }
            let mut body = so_far;
            body.extend_from_slice(&buf[at..at + take]);
            buf.drain(..at + take);
            if body.len() < len {
                streams.insert(csid, (ts, len, kind, body));
                continue;
            }
            streams.insert(csid, (ts, len, kind, Vec::new()));
            match kind {
                1 if body.len() >= 4 => {
                    chunk = (u32::from_be_bytes(body[..4].try_into().unwrap()) & 0x7FFF_FFFF) as usize
                }
                9 if body.len() >= 5 && body[1] == 1 => {
                    let cts = i32::from_be_bytes([0, body[2], body[3], body[4]]) << 8 >> 8;
                    out.lock().unwrap().push(Picture { dts: ts, cts, key: body[0] >> 4 == 1 });
                }
                _ => {}
            }
        }
        match s.read(&mut read) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&read[..n]),
        }
    }
}

fn sender(url: String) -> RtmpSender {
    let s = RtmpSender::start(RtmpTarget { url, key: "key".into(), bind: None }).unwrap();
    s.parameter_sets(&[vec![0x67, 0x42, 0xe0, 0x1f], vec![0x68, 0xce, 0x3c, 0x80]]);
    s
}

fn wait_connected(s: &RtmpSender) {
    let start = Instant::now();
    while !s.stats().connected {
        assert!(start.elapsed() < Duration::from_secs(10), "not connected: {:?}", s.stats().error);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An H.264 access unit (length-prefixed): an IDR slice or a non-IDR one.
fn picture(key: bool) -> Vec<u8> {
    let mut au = 8u32.to_be_bytes().to_vec();
    au.push(if key { 0x65 } else { 0x41 });
    au.extend([0x88; 7]);
    au
}

/// A new stream's first picture is a keyframe anyway: asking for another as it connects would
/// give a multitrack stream a short second GOP on every track (Stream Check flags it).
#[test]
fn a_new_stream_asks_for_no_extra_keyframe() {
    let (url, _) = server(None);
    let s = sender(url);
    wait_connected(&s);
    assert!(!s.take_keyframe_request(), "nothing was offered before the connection: no keyframe asked for");
}

/// Pictures offered before the connection came up (a reconnect, or a server slower than the
/// encoders) went nowhere: the connection starts with a keyframe asked for, as nothing until
/// one decodes.
#[test]
fn a_connection_after_pictures_were_offered_asks_for_a_keyframe() {
    let (go, wait): (Sender<()>, Receiver<()>) = channel();
    let (url, _) = server(Some(wait));
    let s = sender(url);
    s.video(&picture(true), NalFormat::LengthPrefixed, 0, 0, true);
    s.video(&picture(false), NalFormat::LengthPrefixed, 33_333, 33_333, false);
    go.send(()).unwrap();
    wait_connected(&s);
    assert!(s.take_keyframe_request(), "the pictures before the connection were lost: a keyframe is due");
}

/// With B-frames an encoder's decode times run up to two frames ahead of the presentation
/// times (Apple's are moved two frames earlier). The stream's origin is the first decode time,
/// so the first keyframe isn't clamped to 0 and every picture's presentation time (decode
/// time plus composition time) is the one it was given.
#[test]
fn with_b_frames_each_picture_keeps_its_presentation_time() {
    let (url, got) = server(None);
    let s = sender(url);
    wait_connected(&s);
    // 30 fps, I P B B P B B …: presentation order I B B P, decode times two frames earlier
    let frame = 33_333u64;
    let base = 5_000_000u64;
    let order: [(u64, bool); 7] = [(0, true), (3, false), (1, false), (2, false), (6, false), (4, false), (5, false)];
    let mut want = Vec::new();
    for (i, (shown, key)) in order.iter().enumerate() {
        let pts = base + shown * frame;
        let dts = base + i as u64 * frame - 2 * frame;
        s.video(&picture(*key), NalFormat::LengthPrefixed, pts, dts, *key);
        want.push(((pts - (base - 2 * frame)) / 1000) as i64);
    }
    let start = Instant::now();
    while got.lock().unwrap().len() < order.len() {
        assert!(start.elapsed() < Duration::from_secs(10), "pictures arrived: {:?}", got.lock().unwrap());
        std::thread::sleep(Duration::from_millis(5));
    }
    let got = got.lock().unwrap().clone();
    assert_eq!(got[0].dts, 0, "the first decode time is the stream's origin: {got:?}");
    assert!(got[0].key, "{got:?}");
    let shown: Vec<i64> = got.iter().map(|p| p.dts as i64 + p.cts as i64).collect();
    for (i, (have, want)) in shown.iter().zip(&want).enumerate() {
        assert!((have - want).abs() <= 1, "picture {i} shown at {have} ms, given {want} ms: {got:?}");
    }
    assert!(got.iter().all(|p| p.cts >= 0), "no picture is shown before it's decoded: {got:?}");
}
