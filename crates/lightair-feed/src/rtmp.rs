//! RTMP and RTMPS out: the encoded picture and sound to a platform's
//! ingest (Twitch, YouTube, Kick, Facebook, a custom server). H.264 and AAC in FLV tags over the
//! RTMP chunk stream; HEVC in Enhanced RTMP's video tags (FourCC `hvc1`, announced in
//! `connect`'s `fourCcList`); TLS 1.2/1.3 for `rtmps://` with the system-independent Mozilla
//! roots.
//!
//! **Several picture tracks in one stream** ([`RtmpSender::start_multitrack`]: renditions of one
//! program, as Twitch's Enhanced Broadcasting plans them): track 0 goes in Enhanced RTMP's
//! single-track video tags, the others in multitrack tags (one track each, with its track id);
//! `connect` announces the codecs and multitrack, and `onMetaData` describes every track.
//!
//! **Several sound tracks** (Twitch's VOD track after the live sound, in a stream of one picture
//! track or of several): track 0 goes in FLV's own AAC tags, the others in Enhanced RTMP's
//! multitrack audio tags (one track each, FourCC `mp4a`, with its track id), each with its own
//! AAC config ([`RtmpSender::track_audio_config`], [`RtmpSender::track_audio`]).
//!
//! The sender runs on its own thread with a bounded queue, like [`crate::srt::SrtSender`]: what
//! has waited longer than [`MAX_WAIT`] (a network slower than the bitrate) or doesn't fit is
//! dropped, and the pictures after a dropped one are left out until a keyframe (asked for), so
//! the server always gets pictures it can decode and the delay doesn't grow; a dropped
//! connection comes back by itself and asks for a keyframe, and the parameter sets and
//! the audio config are sent again after each reconnect.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::srt::SenderStats;
use crate::ts::{NalFormat, split_nals};

/// Messages waiting to go out at most (about 5 s at 30 fps with audio).
const QUEUE: usize = 400;
/// The chunk size this side sends with.
const CHUNK: usize = 4096;
/// A message that waited longer than this to go out is dropped: the network is slower than the
/// stream (1.5 s queued counts as congestion; the bitrate steps down meanwhile).
const MAX_WAIT: Duration = Duration::from_secs(2);

/// Where to publish: `rtmp(s)://host[:port]/app[/instance]` and the stream key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpTarget {
    pub url: String,
    pub key: String,
    /// The network to send on (`None`: the system's choice).
    pub bind: Option<crate::net::Bind>,
}

/// The parts of an RTMP URL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Url {
    tls: bool,
    host: String,
    port: u16,
    app: String,
    tc_url: String,
}

fn parse_url(url: &str) -> Result<Url, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("rtmps://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("rtmp://") {
        (false, r)
    } else {
        return Err("The address has to start with rtmp:// or rtmps://.".into());
    };
    let (authority, app) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().map_err(|_| "That port isn't a number.".to_string())?),
        None => (authority.to_owned(), if tls { 443 } else { 1935 }),
    };
    if host.is_empty() {
        return Err("The address names no server.".into());
    }
    Ok(Url { tls, host, port, app: app.trim_end_matches('/').to_owned(), tc_url: url.trim_end_matches('/').to_owned() })
}

// ── AMF0 ────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Amf {
    Number(f64),
    Bool(bool),
    Str(String),
    Object(Vec<(String, Amf)>),
    Null,
    Ecma(Vec<(String, Amf)>),
    /// AMF0's strict array.
    Array(Vec<Amf>),
}

fn amf_write(out: &mut Vec<u8>, v: &Amf) {
    let key = |out: &mut Vec<u8>, k: &str| {
        out.extend((k.len() as u16).to_be_bytes());
        out.extend(k.as_bytes());
    };
    match v {
        Amf::Number(n) => {
            out.push(0);
            out.extend(n.to_be_bytes());
        }
        Amf::Bool(b) => out.extend([1, *b as u8]),
        Amf::Str(s) => {
            out.push(2);
            key(out, s);
        }
        Amf::Object(fields) | Amf::Ecma(fields) => {
            if let Amf::Ecma(_) = v {
                out.push(8);
                out.extend((fields.len() as u32).to_be_bytes());
            } else {
                out.push(3);
            }
            for (k, v) in fields {
                key(out, k);
                amf_write(out, v);
            }
            out.extend([0, 0, 9]);
        }
        Amf::Null => out.push(5),
        Amf::Array(items) => {
            out.push(0x0A);
            out.extend((items.len() as u32).to_be_bytes());
            for item in items {
                amf_write(out, item);
            }
        }
    }
}

/// The values of a command message, as far as this side needs them (unknown types end it).
fn amf_read(mut b: &[u8]) -> Vec<Amf> {
    fn one(b: &mut &[u8]) -> Option<Amf> {
        let (&t, rest) = b.split_first()?;
        *b = rest;
        let take = |b: &mut &[u8], n: usize| -> Option<Vec<u8>> {
            (b.len() >= n).then(|| {
                let (a, r) = b.split_at(n);
                *b = r;
                a.to_vec()
            })
        };
        let string = |b: &mut &[u8]| -> Option<String> {
            let n = u16::from_be_bytes(take(b, 2)?.try_into().ok()?) as usize;
            String::from_utf8(take(b, n)?).ok()
        };
        let fields = |b: &mut &[u8]| -> Option<Vec<(String, Amf)>> {
            let mut out = vec![];
            loop {
                let k = string(b)?;
                if k.is_empty() && b.first() == Some(&9) {
                    *b = &b[1..];
                    return Some(out);
                }
                out.push((k, one(b)?));
            }
        };
        Some(match t {
            0 => Amf::Number(f64::from_be_bytes(take(b, 8)?.try_into().ok()?)),
            1 => Amf::Bool(take(b, 1)?[0] != 0),
            2 => Amf::Str(string(b)?),
            3 => Amf::Object(fields(b)?),
            5 | 6 => Amf::Null,
            8 => {
                take(b, 4)?;
                Amf::Ecma(fields(b)?)
            }
            _ => return None,
        })
    }
    let mut out = vec![];
    while let Some(v) = one(&mut b) {
        out.push(v);
    }
    out
}

fn command(name: &str, txn: f64, args: &[Amf]) -> Vec<u8> {
    let mut out = vec![];
    amf_write(&mut out, &Amf::Str(name.into()));
    amf_write(&mut out, &Amf::Number(txn));
    for a in args {
        amf_write(&mut out, a);
    }
    out
}

// ── the chunk stream ────────────────────────────────────────────────────────────────────────

/// One RTMP message, whole.
#[derive(Debug, Clone, PartialEq)]
struct Message {
    csid: u32,
    kind: u8,
    stream: u32,
    timestamp: u32,
    body: Vec<u8>,
}

/// A message cut into chunks of `chunk` bytes: a full header first, then continuation headers.
fn chunks(m: &Message, chunk: usize) -> Vec<u8> {
    let extended = m.timestamp >= 0xFF_FFFF;
    let ts = if extended { 0xFF_FFFF } else { m.timestamp };
    let mut out = Vec::with_capacity(m.body.len() + 16 + m.body.len() / chunk);
    out.push(m.csid as u8 & 0x3F);
    out.extend(&ts.to_be_bytes()[1..]);
    out.extend(&(m.body.len() as u32).to_be_bytes()[1..]);
    out.push(m.kind);
    out.extend(m.stream.to_le_bytes());
    if extended {
        out.extend(m.timestamp.to_be_bytes());
    }
    for (i, piece) in m.body.chunks(chunk).enumerate() {
        if i > 0 {
            out.push(0xC0 | (m.csid as u8 & 0x3F));
            if extended {
                out.extend(m.timestamp.to_be_bytes());
            }
        }
        out.extend(piece);
    }
    out
}

/// Reads the server's chunk stream into whole messages.
#[derive(Default)]
struct Reader {
    buf: Vec<u8>,
    chunk: usize,
    /// Per chunk stream: the last header (timestamp, length, kind, stream) and the message so far.
    streams: std::collections::HashMap<u32, (u32, usize, u8, u32, Vec<u8>)>,
}

impl Reader {
    fn new() -> Reader {
        Reader { chunk: 128, ..Reader::default() }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, when its last chunk is in.
    fn next(&mut self) -> Option<Message> {
        loop {
            let b = &self.buf;
            let first = *b.first()?;
            let fmt = first >> 6;
            let (csid, mut at) = match first & 0x3F {
                0 => (64 + *b.get(1)? as u32, 2),
                1 => (64 + *b.get(1)? as u32 + (*b.get(2)? as u32) * 256, 3),
                n => (n as u32, 1),
            };
            let header = [11, 7, 3, 0][fmt as usize];
            if b.len() < at + header {
                return None;
            }
            let u24 = |s: &[u8]| u32::from_be_bytes([0, s[0], s[1], s[2]]);
            let prev = self.streams.get(&csid).cloned().unwrap_or((0, 0, 0, 0, vec![]));
            let (mut ts, mut len, mut kind, mut stream) = (prev.0, prev.1, prev.2, prev.3);
            let new_message = prev.4.is_empty();
            if fmt <= 2 {
                let t = u24(&b[at..]);
                ts = if fmt == 0 {
                    t
                } else if new_message {
                    prev.0.wrapping_add(t)
                } else {
                    prev.0
                };
            }
            if fmt <= 1 {
                len = u24(&b[at + 3..]) as usize;
                kind = b[at + 6];
            }
            if fmt == 0 {
                stream = u32::from_le_bytes(b[at + 7..at + 11].try_into().ok()?);
            }
            at += header;
            if fmt <= 2 && u24(&b[at - header..]) == 0xFF_FFFF {
                if b.len() < at + 4 {
                    return None;
                }
                ts = u32::from_be_bytes(b[at..at + 4].try_into().ok()?);
                at += 4;
            }
            let have = prev.4.len();
            let take = (len - have.min(len)).min(self.chunk);
            if b.len() < at + take {
                return None;
            }
            let mut body = prev.4;
            body.extend_from_slice(&b[at..at + take]);
            self.buf.drain(..at + take);
            if body.len() >= len {
                self.streams.insert(csid, (ts, len, kind, stream, vec![]));
                if kind == 1 && body.len() >= 4 {
                    self.chunk = (u32::from_be_bytes(body[..4].try_into().ok()?) & 0x7FFF_FFFF) as usize;
                }
                return Some(Message { csid, kind, stream, timestamp: ts, body });
            }
            self.streams.insert(csid, (ts, len, kind, stream, body));
        }
    }
}

// ── FLV bodies ──────────────────────────────────────────────────────────────────────────────

/// AVCDecoderConfigurationRecord from an SPS and a PPS (no start codes).
fn avc_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = vec![
        1,
        sps.get(1).copied().unwrap_or(100),
        sps.get(2).copied().unwrap_or(0),
        sps.get(3).copied().unwrap_or(40),
        0xFF,
        0xE1,
    ];
    out.extend((sps.len() as u16).to_be_bytes());
    out.extend(sps);
    out.push(1);
    out.extend((pps.len() as u16).to_be_bytes());
    out.extend(pps);
    out
}

/// AudioSpecificConfig for AAC-LC.
fn aac_config(sample_rate: u32, channels: u8) -> [u8; 2] {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let fi = RATES.iter().position(|&r| r == sample_rate).unwrap_or(4) as u8;
    [(2 << 3) | (fi >> 1), ((fi & 1) << 7) | ((channels & 0x0F) << 3)]
}

/// The picture's codec in RTMP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RtmpVideo {
    /// H.264 in FLV's own video tags.
    #[default]
    Avc,
    /// HEVC in Enhanced RTMP's video tags (FourCC `hvc1`).
    Hevc,
}

/// The FourCC Enhanced RTMP names a codec by.
fn fourcc(codec: RtmpVideo) -> &'static [u8; 4] {
    match codec {
        RtmpVideo::Avc => b"avc1",
        RtmpVideo::Hevc => b"hvc1",
    }
}

/// Enhanced RTMP's video packet type for a multitrack packet, and its one-track kind.
const PACKET_MULTITRACK: u8 = 6;
const ONE_TRACK: u8 = 0;
/// Enhanced RTMP's audio: the sound format that says an extended header follows, and its packet
/// type for a multitrack packet.
const AUDIO_EX_HEADER: u8 = 9 << 4;
const AUDIO_PACKET_MULTITRACK: u8 = 5;

/// An AAC tag's body: `packet` 0 is the AudioSpecificConfig, 1 a raw access unit. Track 0 in
/// FLV's own AAC tags; another track in an Enhanced RTMP multitrack audio tag carrying one track
/// (its real packet type after the track kind, the FourCC `mp4a`, the track id).
fn audio_tag(packet: u8, track: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = if track == 0 {
        vec![0xAF, packet]
    } else {
        let mut v = vec![AUDIO_EX_HEADER | AUDIO_PACKET_MULTITRACK, (ONE_TRACK << 4) | packet];
        v.extend(b"mp4a");
        v.push(track);
        v
    };
    out.extend(payload);
    out
}

/// A video tag's body: `packet` 0 is the decoder configuration record, 1 a coded picture
/// (`cts_ms` its composition time offset). `track`: `None` in a stream of one picture track
/// (H.264 in FLV's own tags); `Some(0)` the first of several (an Enhanced RTMP single-track
/// tag), `Some(n)` another one (a multitrack tag carrying one track, with its id).
fn video_tag(codec: RtmpVideo, keyframe: bool, packet: u8, cts_ms: i32, payload: &[u8], track: Option<u8>) -> Vec<u8> {
    let frame: u8 = if keyframe { 1 } else { 2 };
    let mut out = match (codec, track) {
        (RtmpVideo::Avc, None) => vec![(frame << 4) | 7, packet],
        // Enhanced RTMP: the extended header bit, the frame type, the packet type (0 sequence
        // start, 1 coded frames, with a composition time), then the FourCC
        (_, None | Some(0)) => {
            let mut v = vec![0x80 | (frame << 4) | packet];
            v.extend(fourcc(codec));
            v
        }
        // a multitrack packet: its real packet type after the track kind, the FourCC, the track id
        (_, Some(id)) => {
            let mut v = vec![0x80 | (frame << 4) | PACKET_MULTITRACK, (ONE_TRACK << 4) | packet];
            v.extend(fourcc(codec));
            v.push(id);
            v
        }
    };
    if packet == 1 || (codec == RtmpVideo::Avc && track.is_none()) {
        out.extend(&cts_ms.to_be_bytes()[1..]);
    }
    out.extend(payload);
    out
}

/// One picture track of a stream of several, as `onMetaData` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct RtmpTrack {
    pub codec: RtmpVideo,
    pub width: u32,
    pub height: u32,
    /// Frames per second.
    pub fps: f64,
    pub kbps: u32,
}

/// One AAC sound track of a stream, as `onMetaData` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct RtmpAudioTrack {
    pub kbps: u32,
    pub sample_rate: u32,
    pub channels: u8,
}

/// What a stream of several picture tracks sends: the picture tracks in order (track 0 first,
/// the main one) and the sound tracks in order (track 0 first, the live sound).
#[derive(Debug, Clone, PartialEq)]
pub struct RtmpTracks {
    pub video: Vec<RtmpTrack>,
    pub audio: Vec<RtmpAudioTrack>,
}

/// A FourCC as `onMetaData`'s codec id number.
fn fourcc_number(codec: RtmpVideo) -> f64 {
    u32::from_be_bytes(*fourcc(codec)) as f64
}

/// `@setDataFrame onMetaData` for a stream of several picture tracks: track 0 of each kind at
/// the top level, the others in `videoTrackIdInfoMap` and `audioTrackIdInfoMap` by their ids.
fn metadata(t: &RtmpTracks) -> Vec<u8> {
    let info = |v: &RtmpTrack| {
        vec![
            ("width".to_string(), Amf::Number(v.width as f64)),
            ("height".to_string(), Amf::Number(v.height as f64)),
            ("framerate".to_string(), Amf::Number(v.fps)),
            ("videodatarate".to_string(), Amf::Number(v.kbps as f64)),
            ("videocodecid".to_string(), Amf::Number(fourcc_number(v.codec))),
        ]
    };
    let mut fields = vec![("duration".to_string(), Amf::Number(0.0))];
    if let Some(first) = t.video.first() {
        fields.extend(info(first));
    }
    let sound = |a: &RtmpAudioTrack| {
        vec![
            ("audiodatarate".to_string(), Amf::Number(a.kbps as f64)),
            ("audiosamplerate".to_string(), Amf::Number(a.sample_rate as f64)),
            ("audiosamplesize".to_string(), Amf::Number(16.0)),
            ("audiochannels".to_string(), Amf::Number(a.channels as f64)),
            ("stereo".to_string(), Amf::Bool(a.channels == 2)),
        ]
    };
    if let Some(first) = t.audio.first() {
        fields.extend(sound(first));
        // FLV's own number for AAC: track 0 goes in FLV's own tags
        fields.push(("audiocodecid".to_string(), Amf::Number(10.0)));
    }
    fields.push(("encoder".to_string(), Amf::Str("Lightair".into())));
    if t.video.len() > 1 {
        let map = t.video.iter().enumerate().skip(1).map(|(i, v)| (i.to_string(), Amf::Object(info(v)))).collect();
        fields.push(("videoTrackIdInfoMap".to_string(), Amf::Object(map)));
    }
    if t.audio.len() > 1 {
        let mp4a = u32::from_be_bytes(*b"mp4a") as f64;
        let map = t
            .audio
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, a)| {
                let mut f = sound(a);
                f.push(("audiocodecid".to_string(), Amf::Number(mp4a)));
                (i.to_string(), Amf::Object(f))
            })
            .collect();
        fields.push(("audioTrackIdInfoMap".to_string(), Amf::Object(map)));
    }
    let mut out = vec![];
    amf_write(&mut out, &Amf::Str("@setDataFrame".into()));
    amf_write(&mut out, &Amf::Str("onMetaData".into()));
    amf_write(&mut out, &Amf::Ecma(fields));
    out
}

// ── the sender ──────────────────────────────────────────────────────────────────────────────

enum Out {
    Video { track: u8, data: Vec<u8>, pts_ms: u32, dts_ms: u32, keyframe: bool, queued_at: Instant },
    Audio { track: u8, data: Vec<u8>, pts_ms: u32, queued_at: Instant },
}

/// What became of a message handed to the queue.
enum Pushed {
    Queued,
    /// The queue was full: dropped.
    Full,
    /// Not connected: dropped.
    Away,
}

#[derive(Default)]
struct Counters {
    connected: AtomicBool,
    sent_bytes: AtomicU64,
    dropped: AtomicU64,
    keyframe: AtomicBool,
    /// A picture was offered (sent or dropped): a connection coming up after that needs a
    /// keyframe asked for; before it, the first picture is one anyway.
    video_offered: AtomicBool,
    error: Mutex<Option<String>>,
    stop: AtomicBool,
    /// Bytes queued and not yet taken by the connection (congestion).
    queued: AtomicU64,
    /// When the next attempt is, while away.
    retry_at: Mutex<Option<std::time::Instant>>,
    /// When the current connection came up (for the backoff).
    connected_at: Mutex<Option<std::time::Instant>>,
    /// Each picture track's decoder configuration record (`avcC` or `hvcC`) and each sound
    /// track's AAC config, sent first after every connect.
    video_config: Mutex<Vec<Option<Vec<u8>>>>,
    audio_config: Mutex<Vec<Option<[u8; 2]>>>,
    /// The stream's bitrate (0: not said): the socket's send buffer is sized to it.
    kbps: AtomicU32,
}

/// The socket's send buffer for a stream of `kbps`: about a second of it. The system's own
/// (auto-tuned up to MBs) holds several seconds at a phone's 600 kbps, out of reach of the
/// queue's dropping, so the delay grew by that much; 16 KB at least, 4 MB at most.
fn send_buffer(kbps: u32) -> usize {
    (kbps as usize * 1000 / 8).clamp(16 << 10, 4 << 20)
}

/// What a connection sends: each picture track's codec, and with several, what `onMetaData`
/// says about them.
#[derive(Debug, Clone)]
struct Layout {
    codecs: Vec<RtmpVideo>,
    multitrack: Option<RtmpTracks>,
}

impl Layout {
    /// A video tag's track (see [`video_tag`]): `None` with one picture track.
    fn tag_track(&self, track: u8) -> Option<u8> {
        self.multitrack.as_ref().map(|_| track)
    }

    fn codec(&self, track: u8) -> RtmpVideo {
        self.codecs.get(track as usize).copied().unwrap_or_default()
    }
}

/// Publishes to one RTMP(S) ingest. Dropping it closes the connection.
pub struct RtmpSender {
    tx: SyncSender<Out>,
    codecs: Vec<RtmpVideo>,
    counters: Arc<Counters>,
    base: Mutex<Option<u64>>,
    /// Per picture track: a picture didn't fit in the queue, so the ones after it are left out
    /// until a keyframe.
    skip: Vec<AtomicBool>,
}

impl RtmpSender {
    /// Publishes H.264 and AAC.
    pub fn start(target: RtmpTarget) -> Result<RtmpSender, String> {
        RtmpSender::start_with(target, RtmpVideo::Avc)
    }

    /// Publishes pictures in `codec` and AAC. With HEVC, give the `hvcC` record
    /// ([`RtmpSender::config_record`]) before the first keyframe.
    pub fn start_with(target: RtmpTarget, codec: RtmpVideo) -> Result<RtmpSender, String> {
        RtmpSender::start_layout(target, Layout { codecs: vec![codec], multitrack: None })
    }

    /// Publishes several picture tracks and AAC in one stream (Enhanced RTMP's multitrack):
    /// `tracks.video[i]` is picture track `i`, `tracks.audio[i]` sound track `i`. Give each
    /// picture track's decoder configuration record ([`RtmpSender::track_config_record`]) before
    /// its first keyframe, and its pictures with [`RtmpSender::track_video`]; each sound track's
    /// config with [`RtmpSender::track_audio_config`] and its sound with
    /// [`RtmpSender::track_audio`]. Everything is sent in the order given, so give it in decode
    /// time order.
    pub fn start_multitrack(target: RtmpTarget, tracks: RtmpTracks) -> Result<RtmpSender, String> {
        if tracks.video.is_empty() || tracks.video.len() > 256 {
            return Err("A stream needs between 1 and 256 picture tracks.".into());
        }
        if tracks.audio.len() > 256 {
            return Err("A stream has at most 256 sound tracks.".into());
        }
        let codecs = tracks.video.iter().map(|t| t.codec).collect();
        RtmpSender::start_layout(target, Layout { codecs, multitrack: Some(tracks) })
    }

    fn start_layout(target: RtmpTarget, layout: Layout) -> Result<RtmpSender, String> {
        let url = parse_url(&target.url)?;
        let tracks = layout.codecs.len().max(1);
        // room for each picture track's messages, and half as many for each sound track after
        // the first
        let more_audio = layout.multitrack.as_ref().map_or(0, |t| t.audio.len().saturating_sub(1));
        let (tx, rx) = sync_channel(QUEUE * tracks + QUEUE / 2 * more_audio);
        let counters = Arc::new(Counters { video_config: Mutex::new(vec![None; tracks]), ..Counters::default() });
        let c = counters.clone();
        let codecs = layout.codecs.clone();
        std::thread::Builder::new()
            .name("lightair-rtmp".into())
            .spawn(move || run(url, target.key, target.bind, layout, rx, c))
            .map_err(|e| e.to_string())?;
        let skip = (0..tracks).map(|_| AtomicBool::new(false)).collect();
        Ok(RtmpSender { tx, codecs, counters, base: Mutex::new(None), skip })
    }

    /// The H.264 SPS and PPS (each without a start code or length). Nothing for HEVC: give its
    /// record instead ([`RtmpSender::config_record`]).
    pub fn parameter_sets(&self, nals: &[Vec<u8>]) {
        self.track_parameter_sets(0, nals);
    }

    /// [`RtmpSender::parameter_sets`] for picture track `track`.
    pub fn track_parameter_sets(&self, track: usize, nals: &[Vec<u8>]) {
        if self.codecs.get(track) != Some(&RtmpVideo::Avc) {
            return;
        }
        let sps = nals.iter().find(|n| n.first().map(|b| b & 0x1F) == Some(7));
        let pps = nals.iter().find(|n| n.first().map(|b| b & 0x1F) == Some(8));
        if let (Some(s), Some(p)) = (sps, pps) {
            self.counters.video_config.lock()[track] = Some(avc_config(s, p));
        }
    }

    /// The picture's decoder configuration record as it is (`avcC` for H.264, `hvcC` for HEVC,
    /// as an MP4 carries it), in place of [`RtmpSender::parameter_sets`].
    pub fn config_record(&self, record: &[u8]) {
        self.track_config_record(0, record);
    }

    /// [`RtmpSender::config_record`] for picture track `track`.
    pub fn track_config_record(&self, track: usize, record: &[u8]) {
        if !record.is_empty()
            && let Some(slot) = self.counters.video_config.lock().get_mut(track)
        {
            *slot = Some(record.to_vec());
        }
    }

    /// The stream's bitrate now, picture and sound (it steps with the network): the
    /// connection's send buffer holds about a second of it.
    pub fn set_bitrate(&self, kbps: u32) {
        self.counters.kbps.store(kbps, Ordering::Relaxed);
    }

    /// AAC-LC's sample rate and channels (the audio config tag).
    pub fn audio_config(&self, sample_rate: u32, channels: u8) {
        self.track_audio_config(0, sample_rate, channels);
    }

    /// [`RtmpSender::audio_config`] for sound track `track` (0 to 255; 1 and on in Enhanced
    /// RTMP's multitrack audio tags, as Twitch's VOD track goes). A sound track's sound isn't
    /// sent before its config.
    pub fn track_audio_config(&self, track: usize, sample_rate: u32, channels: u8) {
        if track > u8::MAX as usize {
            return;
        }
        let mut configs = self.counters.audio_config.lock();
        if configs.len() <= track {
            configs.resize(track + 1, None);
        }
        configs[track] = Some(aac_config(sample_rate, channels));
    }

    fn ms(&self, us: u64) -> u32 {
        let base = *self.base.lock().get_or_insert(us);
        (us.saturating_sub(base) / 1000) as u32
    }

    /// One access unit (length-prefixed or Annex B), timestamps in microseconds.
    pub fn video(&self, data: &[u8], format: NalFormat, pts_us: u64, dts_us: u64, keyframe: bool) {
        self.track_video(0, data, format, pts_us, dts_us, keyframe);
    }

    /// [`RtmpSender::video`] for picture track `track`.
    pub fn track_video(&self, track: usize, data: &[u8], format: NalFormat, pts_us: u64, dts_us: u64, keyframe: bool) {
        let (Some(&codec), Some(skip)) = (self.codecs.get(track), self.skip.get(track)) else { return };
        // after a picture that didn't fit, none until a keyframe: they wouldn't decode
        if !keyframe && skip.load(Ordering::Relaxed) {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.counters.keyframe.store(true, Ordering::Relaxed);
            return;
        }
        // each NAL with a 4-byte length; parameter sets and access unit delimiters travel in
        // the config or not at all (H.264: SPS, PPS, AUD; HEVC: VPS, SPS, PPS, AUD)
        let mut avcc = Vec::with_capacity(data.len() + 16);
        for nal in split_nals(data, format) {
            let leave = match codec {
                RtmpVideo::Avc => matches!(nal.first().map(|b| b & 0x1F), Some(7..=9)),
                RtmpVideo::Hevc => matches!(nal.first().map(|b| (b >> 1) & 0x3F), Some(32..=35)),
            };
            if leave {
                continue;
            }
            avcc.extend((nal.len() as u32).to_be_bytes());
            avcc.extend(nal);
        }
        // the decode time first: the first one seen is the stream's origin, and it is the earlier
        let dts_ms = self.ms(dts_us);
        let pts_ms = self.ms(pts_us);
        let queued = self.push(Out::Video {
            track: track as u8,
            data: avcc,
            pts_ms,
            dts_ms,
            keyframe,
            queued_at: Instant::now(),
        });
        match queued {
            Pushed::Full => skip.store(true, Ordering::Relaxed),
            Pushed::Queued if keyframe => skip.store(false, Ordering::Relaxed),
            // away: each connection starts at a keyframe anyway
            _ => {}
        }
    }

    /// One raw AAC access unit (no ADTS header).
    pub fn audio(&self, data: &[u8], pts_us: u64) {
        self.track_audio(0, data, pts_us);
    }

    /// [`RtmpSender::audio`] for sound track `track` (see [`RtmpSender::track_audio_config`]).
    pub fn track_audio(&self, track: usize, data: &[u8], pts_us: u64) {
        let Ok(track) = u8::try_from(track) else { return };
        let pts_ms = self.ms(pts_us);
        self.push(Out::Audio { track, data: data.to_vec(), pts_ms, queued_at: Instant::now() });
    }

    fn push(&self, m: Out) -> Pushed {
        if matches!(m, Out::Video { .. }) {
            self.counters.video_offered.store(true, Ordering::Relaxed);
        }
        if !self.counters.connected.load(Ordering::Relaxed) {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return Pushed::Away;
        }
        let len = match &m {
            Out::Video { data, .. } | Out::Audio { data, .. } => data.len() as u64,
        };
        match self.tx.try_send(m) {
            Ok(()) => {
                self.counters.queued.fetch_add(len, Ordering::Relaxed);
                Pushed::Queued
            }
            Err(TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                self.counters.keyframe.store(true, Ordering::Relaxed);
                Pushed::Full
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                Pushed::Away
            }
        }
    }

    pub fn stats(&self) -> SenderStats {
        let c = &self.counters;
        SenderStats {
            connected: c.connected.load(Ordering::Relaxed),
            sent_bytes: c.sent_bytes.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            error: c.error.lock().clone(),
            retry_in: c.retry_at.lock().map(|t| t.saturating_duration_since(std::time::Instant::now())),
        }
    }

    /// Bytes waiting for the connection (with the bitrate, how long the queue is).
    pub fn queued_bytes(&self) -> u64 {
        self.counters.queued.load(Ordering::Relaxed)
    }

    /// Whether the encoder should make a keyframe now (a reconnect, or data dropped).
    pub fn take_keyframe_request(&self) -> bool {
        self.counters.keyframe.swap(false, Ordering::Relaxed)
    }
}

impl Drop for RtmpSender {
    fn drop(&mut self) {
        self.counters.stop.store(true, Ordering::Relaxed);
    }
}

/// The connection: plain or TLS.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(t) => &t.sock,
        }
    }

    /// What the server has sent, without waiting (`WouldBlock` when nothing has come). The
    /// socket is non-blocking only for this read: a short read timeout instead waits a whole
    /// timer tick on Windows (about 15 ms), which held a 60 fps stream to fewer messages a
    /// second than it makes.
    fn read_now(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.tcp().set_nonblocking(true)?;
        let read = self.read(buf);
        self.tcp().set_nonblocking(false)?;
        read
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(t) => t.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(t) => t.flush(),
        }
    }
}

fn open(url: &Url, bind: Option<&crate::net::Bind>) -> Result<Conn, String> {
    use std::net::ToSocketAddrs;
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("Can't find {}: {e}", url.host))?
        .next()
        .ok_or_else(|| format!("Can't find {}.", url.host))?;
    let tcp = crate::net::tcp_connect(&addr, Duration::from_secs(5), bind)
        .map_err(|e| format!("{} didn't answer: {e}", url.host))?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();
    if !url.tls {
        return Ok(Conn::Plain(tcp));
    }
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(url.host.clone()).map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(conn, tcp))))
}

/// C0+C1, then S0+S1+S2, then C2 (the simple handshake).
fn handshake(c: &mut Conn) -> io::Result<()> {
    let mut c1 = vec![3u8];
    c1.extend(0u32.to_be_bytes());
    c1.extend(0u32.to_be_bytes());
    let mut rnd = [0u8; 1528];
    getrandom::fill(&mut rnd).ok();
    c1.extend(rnd);
    c.write_all(&c1)?;
    c.flush()?;
    let mut s = vec![0u8; 1 + 1536 + 1536];
    c.read_exact(&mut s)?;
    c.write_all(&s[1..1537])?;
    c.flush()
}

/// Connects, publishes, and sends until the connection drops or the sender stops.
fn session(
    url: &Url,
    key: &str,
    bind: Option<&crate::net::Bind>,
    layout: &Layout,
    rx: &Receiver<Out>,
    c: &Counters,
) -> Result<(), String> {
    let mut conn = open(url, bind)?;
    let io = |e: io::Error| e.to_string();
    handshake(&mut conn).map_err(io)?;
    let mut reader = Reader::new();
    let send = |conn: &mut Conn, m: Message| -> Result<(), String> {
        let bytes = chunks(&m, CHUNK);
        conn.write_all(&bytes).map_err(io)?;
        c.sent_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    };
    let control = |kind: u8, body: Vec<u8>| Message { csid: 2, kind, stream: 0, timestamp: 0, body };
    send(&mut conn, control(1, (CHUNK as u32).to_be_bytes().to_vec()))?;
    let mut fields = vec![
        ("app".into(), Amf::Str(url.app.clone())),
        ("type".into(), Amf::Str("nonprivate".into())),
        ("flashVer".into(), Amf::Str("FMLE/3.0 (compatible; Lightair Live)".into())),
        ("tcUrl".into(), Amf::Str(url.tc_url.clone())),
    ];
    // Enhanced RTMP: the codecs this side sends beyond FLV's own (with several tracks, every
    // one: they all go in Enhanced RTMP's tags), what it does with each (encodes), and that it
    // sends several tracks
    let mut enhanced: Vec<&str> = Vec::new();
    for codec in &layout.codecs {
        let name = std::str::from_utf8(fourcc(*codec)).unwrap_or_default();
        if (*codec != RtmpVideo::Avc || layout.multitrack.is_some()) && !enhanced.contains(&name) {
            enhanced.push(name);
        }
    }
    if !enhanced.is_empty() {
        fields.push(("fourCcList".into(), Amf::Array(enhanced.iter().map(|n| Amf::Str(n.to_string())).collect())));
    }
    if let Some(t) = &layout.multitrack {
        const CAN_ENCODE: f64 = 2.0;
        const CAPS_MULTITRACK: f64 = 2.0;
        let map = enhanced.iter().map(|n| (n.to_string(), Amf::Number(CAN_ENCODE))).collect();
        fields.push(("videoFourCcInfoMap".into(), Amf::Object(map)));
        // sound tracks after the first go in Enhanced RTMP's audio tags
        if t.audio.len() > 1 {
            let map = vec![("mp4a".to_string(), Amf::Number(CAN_ENCODE))];
            fields.push(("audioFourCcInfoMap".into(), Amf::Object(map)));
        }
        fields.push(("capsEx".into(), Amf::Number(CAPS_MULTITRACK)));
    }
    let connect = Amf::Object(fields);
    let cmd = |name: &str, txn: f64, args: &[Amf], stream: u32| Message {
        csid: if stream == 0 { 3 } else { 4 },
        kind: 20,
        stream,
        timestamp: 0,
        body: command(name, txn, args),
    };
    send(&mut conn, cmd("connect", 1.0, &[connect], 0))?;
    send(&mut conn, cmd("releaseStream", 2.0, &[Amf::Null, Amf::Str(key.into())], 0))?;
    send(&mut conn, cmd("FCPublish", 3.0, &[Amf::Null, Amf::Str(key.into())], 0))?;
    send(&mut conn, cmd("createStream", 4.0, &[Amf::Null], 0))?;
    conn.flush().map_err(io)?;

    // the server's answers: createStream's stream id, then publish's start
    let mut stream_id = None;
    let mut published = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = vec![0u8; 16384];
    while !published {
        if Instant::now() > deadline {
            return Err("The server didn't accept the stream (check the stream key).".into());
        }
        while let Some(m) = reader.next() {
            match m.kind {
                4 if m.body.len() >= 6 && m.body[..2] == [0, 6] => {
                    // ping request: answer it
                    let mut body = vec![0, 7];
                    body.extend(&m.body[2..6]);
                    send(&mut conn, control(4, body))?;
                }
                20 => {
                    let v = amf_read(&m.body);
                    let name = match v.first() {
                        Some(Amf::Str(s)) => s.as_str(),
                        _ => "",
                    };
                    let txn = match v.get(1) {
                        Some(Amf::Number(n)) => *n,
                        _ => 0.0,
                    };
                    if name == "_result" && txn == 4.0 {
                        if let Some(Amf::Number(id)) = v.get(3) {
                            stream_id = Some(*id as u32);
                            send(
                                &mut conn,
                                cmd(
                                    "publish",
                                    5.0,
                                    &[Amf::Null, Amf::Str(key.into()), Amf::Str("live".into())],
                                    *id as u32,
                                ),
                            )?;
                            conn.flush().map_err(io)?;
                        }
                    } else if name == "_error" {
                        return Err("The server refused the connection.".into());
                    } else if name == "onStatus" {
                        let code = v.iter().find_map(|a| match a {
                            Amf::Object(f) => f.iter().find(|(k, _)| k == "code").and_then(|(_, v)| match v {
                                Amf::Str(s) => Some(s.clone()),
                                _ => None,
                            }),
                            _ => None,
                        });
                        match code.as_deref() {
                            Some("NetStream.Publish.Start") => published = true,
                            Some(c) if c.contains("BadName") || c.contains("Failed") || c.contains("Rejected") => {
                                return Err(format!("The server refused the stream: {c}"));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if published {
            break;
        }
        let n = conn.read(&mut buf).map_err(io)?;
        if n == 0 {
            return Err("The server closed the connection.".into());
        }
        reader.push(&buf[..n]);
    }
    let stream = stream_id.unwrap_or(1);

    // the configs go before the first keyframe and the first sound (they may come after the
    // connection: an encoder gives its parameter sets with its first keyframe), and again
    // whenever they change; a picture before the video config isn't sent (it can't decode)
    let tracks = layout.codecs.len().max(1);
    let mut sent_video_config: Vec<Option<Vec<u8>>> = vec![None; tracks];
    let mut sent_audio_config: Vec<Option<[u8; 2]>> = Vec::new();
    // several tracks: what they are, before anything else
    if let Some(t) = &layout.multitrack {
        send(&mut conn, Message { csid: 5, kind: 18, stream, timestamp: 0, body: metadata(t) })?;
    }
    conn.flush().map_err(io)?;
    c.connected.store(true, Ordering::Relaxed);
    // pictures came before (a reconnect, or a connection slower than the encoders): a keyframe
    // now, as nothing until one decodes; none yet: the first one is a keyframe anyway
    if c.video_offered.load(Ordering::Relaxed) {
        c.keyframe.store(true, Ordering::Relaxed);
    }
    *c.error.lock() = None;
    *c.retry_at.lock() = None;
    c.connected_at.lock().replace(std::time::Instant::now());
    // a keyframe first on every track: nothing before it decodes
    let mut waiting_for_key = vec![true; tracks];
    conn.tcp().set_read_timeout(Some(Duration::from_millis(1))).ok();
    let mut buffer_for = 0;
    loop {
        let kbps = c.kbps.load(Ordering::Relaxed);
        if kbps != buffer_for {
            buffer_for = kbps;
            if kbps > 0 {
                socket2::SockRef::from(conn.tcp()).set_send_buffer_size(send_buffer(kbps)).ok();
            }
        }
        // the sender is stopped or dropped: the end of the stream on purpose (not a dropped
        // connection), then the stream goes
        let goodbye = |conn: &mut Conn| {
            let _ = send(conn, cmd("FCUnpublish", 6.0, &[Amf::Null, Amf::Str(key.into())], 0));
            let _ = send(conn, cmd("deleteStream", 7.0, &[Amf::Null, Amf::Number(stream as f64)], 0));
            let _ = conn.flush();
        };
        if c.stop.load(Ordering::Relaxed) {
            goodbye(&mut conn);
            return Ok(());
        }
        let got = rx.recv_timeout(Duration::from_millis(100));
        if let Ok(Out::Video { data, .. } | Out::Audio { data, .. }) = &got {
            let n = data.len() as u64;
            let _ = c.queued.try_update(Ordering::Relaxed, Ordering::Relaxed, |q| Some(q.saturating_sub(n)));
        }
        // waited too long (the network is slower than the stream): dropped, so the delay stays
        // short; the pictures after a dropped one wait for a keyframe
        let stale = match &got {
            Ok(Out::Video { queued_at, .. } | Out::Audio { queued_at, .. }) => queued_at.elapsed() > MAX_WAIT,
            Err(_) => false,
        };
        if stale {
            c.dropped.fetch_add(1, Ordering::Relaxed);
            if let Ok(Out::Video { track, .. }) = got
                && let Some(w) = waiting_for_key.get_mut(track as usize)
            {
                *w = true;
                c.keyframe.store(true, Ordering::Relaxed);
            }
            continue;
        }
        match got {
            Ok(Out::Video { track, data, pts_ms, dts_ms, keyframe, .. }) => {
                let t = track as usize;
                if t >= tracks || (waiting_for_key[t] && !keyframe) {
                    continue;
                }
                let (codec, tag_track) = (layout.codec(track), layout.tag_track(track));
                if keyframe {
                    let cfg = c.video_config.lock().get(t).cloned().flatten();
                    match cfg {
                        Some(cfg) if sent_video_config[t].as_ref() != Some(&cfg) => {
                            send(
                                &mut conn,
                                Message {
                                    csid: 6,
                                    kind: 9,
                                    stream,
                                    timestamp: dts_ms,
                                    body: video_tag(codec, true, 0, 0, &cfg, tag_track),
                                },
                            )?;
                            sent_video_config[t] = Some(cfg);
                        }
                        Some(_) => {}
                        None => {
                            // no parameter sets yet: wait for the next keyframe
                            waiting_for_key[t] = true;
                            continue;
                        }
                    }
                }
                waiting_for_key[t] = false;
                let cts = pts_ms as i32 - dts_ms as i32;
                send(
                    &mut conn,
                    Message {
                        csid: 6,
                        kind: 9,
                        stream,
                        timestamp: dts_ms,
                        body: video_tag(codec, keyframe, 1, cts, &data, tag_track),
                    },
                )?;
            }
            Ok(Out::Audio { track, data, pts_ms, .. }) => {
                let t = track as usize;
                let Some(cfg) = c.audio_config.lock().get(t).copied().flatten() else { continue };
                if sent_audio_config.len() <= t {
                    sent_audio_config.resize(t + 1, None);
                }
                if sent_audio_config[t] != Some(cfg) {
                    let body = audio_tag(0, track, &cfg);
                    send(&mut conn, Message { csid: 4, kind: 8, stream, timestamp: pts_ms, body })?;
                    sent_audio_config[t] = Some(cfg);
                }
                let body = audio_tag(1, track, &data);
                send(&mut conn, Message { csid: 4, kind: 8, stream, timestamp: pts_ms, body })?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                goodbye(&mut conn);
                return Ok(());
            }
        }
        conn.flush().map_err(io)?;
        // pings and acknowledgements from the server, without waiting
        match conn.read_now(&mut buf) {
            Ok(0) => return Err("The server closed the connection.".into()),
            Ok(n) => {
                reader.push(&buf[..n]);
                while let Some(m) = reader.next() {
                    if m.kind == 4 && m.body.len() >= 6 && m.body[..2] == [0, 6] {
                        let mut body = vec![0, 7];
                        body.extend(&m.body[2..6]);
                        send(&mut conn, control(4, body))?;
                    }
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn run(url: Url, key: String, bind: Option<crate::net::Bind>, layout: Layout, rx: Receiver<Out>, c: Arc<Counters>) {
    let mut backoff = crate::backoff::Backoff::default();
    while !c.stop.load(Ordering::Relaxed) {
        let result = session(&url, &key, bind.as_ref(), &layout, &rx, &c);
        c.connected.store(false, Ordering::Relaxed);
        if let Some(t) = c.connected_at.lock().take() {
            backoff.connected(t);
        }
        match result {
            Ok(()) => return,
            Err(e) => {
                tracing::debug!("rtmp: {e}");
                *c.error.lock() = Some(e);
                // whatever waited is stale now
                while rx.try_recv().is_ok() {}
                // 0.5, 1, 2, 4, 8 s, then every 8 s, in steps so a stop isn't held up
                let now = std::time::Instant::now();
                let until = now + backoff.next(now);
                *c.retry_at.lock() = Some(until);
                while std::time::Instant::now() < until && !c.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_read() {
        let u = parse_url("rtmps://live-api-s.facebook.com:443/rtmp/").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.app.as_str()), (true, "live-api-s.facebook.com", 443, "rtmp"));
        let u = parse_url("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!((u.tls, u.port, u.app.as_str()), (false, 1935, "app"));
        assert!(parse_url("http://x").is_err());
    }

    #[test]
    fn amf_is_written_as_amf0_and_reads_back() {
        let values = vec![
            Amf::Str("connect".into()),
            Amf::Number(1.0),
            Amf::Object(vec![("app".into(), Amf::Str("live".into()))]),
            Amf::Null,
        ];
        let mut b = vec![];
        for v in &values {
            amf_write(&mut b, v);
        }
        // what an RTMP server reads (AMF0: a type marker, big-endian lengths and numbers, an
        // object ended by an empty key and 0x09)
        let mut want = vec![0x02, 0x00, 0x07];
        want.extend(b"connect");
        want.extend([0x00, 0x3F, 0xF0, 0, 0, 0, 0, 0, 0]);
        want.extend([0x03, 0x00, 0x03]);
        want.extend(b"app");
        want.extend([0x02, 0x00, 0x04]);
        want.extend(b"live");
        want.extend([0x00, 0x00, 0x09, 0x05]);
        assert_eq!(b, want);
        assert_eq!(amf_read(&b), values);
    }

    #[test]
    fn chunks_come_back_whole() {
        let m = Message { csid: 6, kind: 9, stream: 1, timestamp: 1234, body: (0..10_000).map(|i| i as u8).collect() };
        let mut r = Reader::new();
        r.chunk = 4096;
        r.push(&chunks(&m, 4096));
        assert_eq!(r.next(), Some(m));
        assert_eq!(aac_config(48000, 2), [0x11, 0x90]);
    }

    /// Pictures as a server got them: (when, frame number, keyframe).
    type Arrivals = Arc<Mutex<Vec<(Instant, u32, bool)>>>;

    /// An RTMP server that accepts one publish and then reads at `kbps`, as a throttled
    /// network would; each picture it gets: (when, frame number, keyframe).
    fn slow_server(kbps: u64) -> (String, Arrivals) {
        let (url, got, _) = slow_server_taking(kbps);
        (url, got)
    }

    /// [`slow_server`], and how many bytes it has read since the publish.
    fn slow_server_taking(kbps: u64) -> (String, Arrivals, Arc<AtomicU64>) {
        use socket2::{Domain, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        // a small receive buffer, so the throttle and not the kernel decides what's taken
        socket.set_recv_buffer_size(16 * 1024).unwrap();
        socket.bind(&"127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap().into()).unwrap();
        socket.listen(1).unwrap();
        let listener: std::net::TcpListener = socket.into();
        let port = listener.local_addr().unwrap().port();
        let got = Arc::new(Mutex::new(Vec::new()));
        let out = got.clone();
        let read = Arc::new(AtomicU64::new(0));
        let read_out = read.clone();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut c0c1 = vec![0u8; 1537];
            s.read_exact(&mut c0c1).unwrap();
            let mut reply = vec![3u8];
            reply.extend(&c0c1[1..]);
            reply.extend(&c0c1[1..]);
            s.write_all(&reply).unwrap();
            let mut c2 = vec![0u8; 1536];
            s.read_exact(&mut c2).unwrap();
            let mut reader = Reader::new();
            let mut buf = vec![0u8; 64 * 1024];
            // throttled once publishing (the handshake and commands go at once)
            let mut throttled: Option<Instant> = None;
            let mut taken = 0u64;
            loop {
                let allowed =
                    throttled.map_or(u64::MAX, |t| (t.elapsed().as_secs_f64() * kbps as f64 * 1000.0 / 8.0) as u64);
                let room = allowed.saturating_sub(taken).min(buf.len() as u64) as usize;
                if room < 1024 {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let n = match s.read(&mut buf[..room]) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                if throttled.is_some() {
                    taken += n as u64;
                    read_out.store(taken, Ordering::Relaxed);
                }
                reader.push(&buf[..n]);
                while let Some(m) = reader.next() {
                    match m.kind {
                        20 => {
                            let v = amf_read(&m.body);
                            let name = match v.first() {
                                Some(Amf::Str(s)) => s.clone(),
                                _ => String::new(),
                            };
                            let answer = match name.as_str() {
                                "createStream" => command("_result", 4.0, &[Amf::Null, Amf::Number(1.0)]),
                                "publish" => {
                                    throttled = Some(Instant::now());
                                    command(
                                        "onStatus",
                                        0.0,
                                        &[
                                            Amf::Null,
                                            Amf::Object(vec![(
                                                "code".into(),
                                                Amf::Str("NetStream.Publish.Start".into()),
                                            )]),
                                        ],
                                    )
                                }
                                _ => continue,
                            };
                            let reply = Message { csid: 3, kind: 20, stream: 0, timestamp: 0, body: answer };
                            s.write_all(&chunks(&reply, 128)).unwrap();
                        }
                        9 if m.body.len() >= 14 && m.body[1] == 1 => {
                            // the test's pictures carry their number after the NAL header
                            let n = u32::from_be_bytes(m.body[10..14].try_into().unwrap());
                            out.lock().push((Instant::now(), n, m.body[0] == 0x17));
                        }
                        _ => {}
                    }
                }
            }
        });
        (format!("rtmp://127.0.0.1:{port}/live"), got, read)
    }

    /// A network slower than the stream (600 kbps under about 1.3 Mbps, as Stream Check's
    /// throttle): pictures keep arriving, each one decodable (a keyframe, or the picture right
    /// after the last one sent), keyframes every few seconds, and the delay stays bounded; not a
    /// queue that grows for many seconds, then drops pictures at random, keyframes too.
    #[test]
    fn a_slow_network_gets_decodable_pictures_and_keyframes_not_a_stall() {
        let (url, got) = slow_server(600);
        let s = RtmpSender::start(RtmpTarget { url, key: "k".into(), bind: None }).unwrap();
        s.parameter_sets(&[vec![0x67, 0x42, 0xe0, 0x1f], vec![0x68, 0xce, 0x3c, 0x80]]);
        s.audio_config(48_000, 2);
        let start = Instant::now();
        while !s.stats().connected {
            assert!(start.elapsed() < Duration::from_secs(10), "{:?}", s.stats().error);
            std::thread::sleep(Duration::from_millis(10));
        }
        // 20 s at 30 fps: 1.2 Mbps of pictures (a keyframe every 2 s, or when asked, as an
        // encoder does once a second) and 120 kbps of sound
        let start = Instant::now();
        let mut sent_at = Vec::new();
        let mut asked = false;
        for n in 0..600u32 {
            let due = start + Duration::from_micros(n as u64 * 33_333);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            if n % 30 == 0 {
                asked |= s.take_keyframe_request();
            }
            let key = n % 60 == 0 || std::mem::take(&mut asked);
            let size = if key { 20_000 } else { 5_000 };
            let mut au = (size as u32 - 4).to_be_bytes().to_vec();
            au.push(if key { 0x65 } else { 0x41 });
            au.extend(n.to_be_bytes());
            au.resize(size, 0x55);
            sent_at.push(Instant::now());
            let us = n as u64 * 33_333;
            s.video(&au, NalFormat::LengthPrefixed, us, us, key);
            s.audio(&[0x21; 500], us);
        }
        let end = Instant::now();
        let got = got.lock().clone();
        assert!(!got.is_empty(), "no picture arrived");
        let trace = || {
            got.iter()
                .map(|g| {
                    let ms = g.0.duration_since(start).as_millis();
                    format!("{ms}:{}{}", g.1, if g.2 { "K" } else { "" })
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        // each picture decodes: a keyframe, or the one after the last one that arrived
        for w in got.windows(2) {
            assert!(
                w[1].2 || w[1].1 == w[0].1 + 1,
                "picture {} arrived after {} without a keyframe between: {}",
                w[1].1,
                w[0].1,
                trace()
            );
        }
        // once the network's buffers are full (a few seconds in), keyframes keep coming
        let settled = start + Duration::from_secs(8);
        let keys: Vec<Instant> = got.iter().filter(|g| g.2 && g.0 > settled).map(|g| g.0).collect();
        let mut last = settled;
        for k in keys.iter().chain(std::iter::once(&end)) {
            let gap = k.duration_since(last);
            assert!(gap < Duration::from_millis(3500), "{gap:?} without a keyframe: {}", trace());
            last = *k;
        }
        // and the delay is bounded (the sender's 2 s and the system's socket buffer): the
        // newest pictures keep arriving
        let (at, n, _) = *got.last().unwrap();
        let delay = at.duration_since(sent_at[n as usize]);
        assert!(delay < Duration::from_secs(7), "{delay:?} behind: {}", trace());
        assert!(n >= 420, "the last picture to arrive was {n} of 600: {}", trace());
        assert!(s.stats().dropped > 0, "some had to go");
    }

    /// The most bytes the system held for a sender (written to its socket, not yet read by
    /// a server reading at 600 kbps) over 4 s of 2.4 Mbps, with its bitrate said or not.
    fn held_by_the_system(bitrate: Option<u32>) -> u64 {
        let (url, _, read) = slow_server_taking(600);
        let s = RtmpSender::start(RtmpTarget { url, key: "k".into(), bind: None }).unwrap();
        if let Some(kbps) = bitrate {
            s.set_bitrate(kbps);
        }
        s.parameter_sets(&[vec![0x67, 0x42, 0xe0, 0x1f], vec![0x68, 0xce, 0x3c, 0x80]]);
        let start = Instant::now();
        while !s.stats().connected {
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = s.counters.sent_bytes.load(Ordering::Relaxed);
        let mut held = 0;
        for n in 0..120u32 {
            std::thread::sleep(Duration::from_millis(33));
            let key = n % 60 == 0 || s.take_keyframe_request();
            let mut au = 9_996u32.to_be_bytes().to_vec();
            au.push(if key { 0x65 } else { 0x41 });
            au.resize(10_000, 0x55);
            let us = n as u64 * 33_333;
            s.video(&au, NalFormat::LengthPrefixed, us, us, key);
            let written = s.counters.sent_bytes.load(Ordering::Relaxed) - before;
            held = held.max(written.saturating_sub(read.load(Ordering::Relaxed)));
        }
        held
    }

    /// A network slower than the stream: the phone's TCP send buffer holds about a second of
    /// the stream (here 300 kbps: 37.5 KB), not the system's default (128 KB here; auto-tuned
    /// up to MBs on a real network: 3 s more delay at 600 kbps, out of reach of the queue's
    /// dropping). The server's own receive buffer holds the same either way.
    #[test]
    // Windows holds a socket's sends past SO_SNDBUF (86.9 KB sized against 117.9 KB here); the
    // delay a slow network gives is checked there by the slow-network test above
    #[cfg_attr(windows, ignore = "Windows holds a socket's sends past its send buffer size")]
    fn the_send_buffer_holds_about_a_second() {
        let sized = std::thread::spawn(|| held_by_the_system(Some(300)));
        let default = held_by_the_system(None);
        let sized = sized.join().unwrap();
        assert!(sized + 50_000 < default, "sized to the bitrate {sized} bytes, the system's {default}");
    }
}
