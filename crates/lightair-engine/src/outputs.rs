//! Going live: the stream's encoded packets to each destination, and each destination's state
//! machine and bitrate.
//!
//! Destinations: **Test** (streams nowhere: a sink paced at a chosen upload rate, so going
//! live, congestion and the stepping can be tried without a network or an account), **RTMP(S)**
//! (`lightair-feed`'s publisher), **SRT** (its caller, with the MPEG-TS muxer) and **bonded SRT**
//! (the same through `lightair-feed`'s SRTLA relay, over every network at once). Senders run on
//! their own threads and reconnect by themselves; this module watches them once a second.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use lightair_feed::ts::{AudioConfig, Muxer, NalFormat, VideoCodec};
use lightair_feed::{RtmpSender, RtmpTarget, SrtSender, SrtTarget};
use parking_lot::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub enum DestinationKind {
    /// Streams nowhere; `upload_kbps`: the network it pretends to have (`None`: plenty).
    Test {
        upload_kbps: Option<u32>,
    },
    Rtmp {
        url: String,
        key: String,
    },
    /// `srt://host:port?streamid=…&passphrase=…&latency=…` (ms).
    Srt {
        url: String,
    },
    /// Bonded SRT: `srtla://host:port?…` (the same parts as SRT's), an SRTLA receiver
    /// the person runs; sent over every network the phone has at once.
    Srtla {
        url: String,
    },
    /// WHIP: the endpoint's `https://` URL and its bearer token, if any.
    Whip {
        url: String,
        token: Option<String>,
    },
    /// A platform from `platforms::PLATFORMS`: `server` `None` is chosen
    /// automatically (Twitch: the nearest ingest), `key` `None` comes through sign-in (F4).
    Platform {
        platform: String,
        server: Option<String>,
        key: Option<String>,
    },
}

impl Destination {
    /// Within its platform's limits (bitrate, size, frame rate), if it's a platform's.
    pub fn within_limits(mut self) -> Destination {
        let DestinationKind::Platform { platform, .. } = &self.kind else {
            return self;
        };
        let Some(p) = crate::platforms::by_id(platform) else {
            return self;
        };
        let l = crate::platforms::limits(p);
        if let Some(max) = l.max_kbps {
            self.video_kbps = self.video_kbps.min(max);
        }
        if let Some(max) = l.max_height {
            self.height = Some(self.height.map_or(max, |h| h.min(max)));
        }
        if let Some(max) = l.max_fps {
            self.fps = Some(self.fps.map_or(max, |f| f.min(max)));
        }
        self
    }

    /// A platform destination as the RTMP (or SRT) one it is: its server (chosen if not
    /// given) and key (pasted, or through sign-in: Twitch's key, or a YouTube broadcast made
    /// now with `title`, whose id comes back to end with the stream). Network calls: not on the
    /// control thread.
    pub fn resolve(self, title: &str) -> Result<(Destination, Option<String>), String> {
        let d = self.within_limits();
        let DestinationKind::Platform {
            platform,
            server,
            key,
        } = &d.kind
        else {
            return Ok((d, None));
        };
        let p = crate::platforms::by_id(platform)
            .ok_or_else(|| format!("{} isn't a platform Lightair knows.", d.name))?;
        let pasted = key.as_deref().map(str::trim).filter(|k| !k.is_empty());
        let given = server.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let signed_in = crate::platforms::key_by_sign_in(p)
            .filter(|provider| lightair_live::official::signed_in(provider));
        // YouTube through sign-in: a broadcast made now, on the channel's reusable stream
        if pasted.is_none() && signed_in == Some("youtube") {
            let privacy = d.visibility.as_deref().unwrap_or("public");
            let made = lightair_live::official::youtube_create_broadcast(title, privacy)
                .map_err(|e| format!("{}: YouTube didn't make the broadcast: {e}", d.name))?;
            let rtmp = Destination {
                kind: DestinationKind::Rtmp {
                    url: made.url,
                    key: made.key,
                },
                ..d
            };
            return Ok((rtmp, made.broadcast));
        }
        let url = match given {
            Some(s) => s.to_string(),
            None => crate::platforms::automatic_server(p)?,
        };
        if url.starts_with("srt://") {
            let srt = Destination {
                kind: DestinationKind::Srt { url },
                ..d
            };
            return Ok((srt, None));
        }
        let key = match (pasted, signed_in) {
            (Some(k), _) => k.to_string(),
            (None, Some(provider)) => lightair_live::official::stream_key(provider)
                .map_err(|e| format!("{}: {e}", d.name))?,
            (None, None) => {
                return Err(match crate::platforms::key_by_sign_in(p) {
                    Some(_) => format!(
                        "{}: paste the stream key, or sign in to {} so Lightair can get it.",
                        d.name, p.name
                    ),
                    None => format!("{}: paste the stream key.", d.name),
                });
            }
        };
        let rtmp = Destination {
            kind: DestinationKind::Rtmp { url, key },
            ..d
        };
        Ok((rtmp, None))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    pub id: String,
    pub name: String,
    pub kind: DestinationKind,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    /// The stream's picture height (`None`: the format's).
    pub height: Option<u32>,
    /// Frames a second (`None`: the format's; a divisor of it, else the format's).
    pub fps: Option<u32>,
    /// A broadcast made through sign-in (YouTube): "public", "unlisted" or "private"
    /// (`None`: public).
    pub visibility: Option<String>,
    /// The network it's sent on (`None`: the system's choice).
    pub network: Option<crate::host::NetworkKind>,
    /// Held this many seconds before it's sent (at most 120).
    pub delay_s: Option<u32>,
    /// Given up (stopped, to start again) after this long away (`None`: never).
    pub give_up_after_s: Option<u32>,
    /// The format it streams (a canvas's id; `None` or one that's gone: the active
    /// one).
    pub format: Option<String>,
}

/// What waits in a delayed destination's queue, in order.
enum Held {
    Sets(Vec<Vec<u8>>),
    Video(Vec<u8>, NalFormat, u64, bool),
    Audio(Vec<u8>, u64),
}

/// A delayed destination's queue: each packet sent `delay` after it came.
#[derive(Default)]
struct Delay {
    delay: Duration,
    queue: std::collections::VecDeque<(Instant, Held)>,
    bytes: usize,
}

/// The most a delay holds (a minute at 8 Mbps); past it the oldest go.
const DELAY_BYTES: usize = 64 << 20;

/// One stream encode and the destinations that share it (same size, frame rate and bitrate).
#[derive(Debug, Clone, PartialEq)]
pub struct Encode {
    /// Its format (a canvas's id; "" where only one was given).
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub destinations: Vec<String>,
}

/// What going live on some destinations takes: its encodes, and whether the phone
/// can run them beside a recording's.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPlan {
    pub encodes: Vec<Encode>,
    pub allowed: bool,
    pub message: Option<String>,
}

/// The encodes for `dests` on a format of `size` at `fps`: one per distinct (size, frame rate,
/// bitrate), in the destinations' order; `max` encodes at once, `recording` of them taken by a
/// recording's own.
pub fn plan(
    dests: &[Destination],
    size: (u32, u32),
    fps: u32,
    max: u32,
    recording: u32,
) -> StreamPlan {
    let one = Format {
        id: String::new(),
        size,
        fps,
    };
    plan_in(dests, std::slice::from_ref(&one), max, recording, 1)
}

/// A format the stream can be drawn in: a canvas's id, size and frame rate.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    pub id: String,
    pub size: (u32, u32),
    pub fps: u32,
}

/// [`plan`] over the project's formats (the active one first): each destination in its own
/// (`Destination.format`), `at_once` formats at most.
pub fn plan_in(
    dests: &[Destination],
    formats: &[Format],
    max: u32,
    recording: u32,
    at_once: u32,
) -> StreamPlan {
    let mut encodes: Vec<Encode> = Vec::new();
    for d in dests.iter().cloned().map(Destination::within_limits) {
        let d = &d;
        let Some(f) = d
            .format
            .as_ref()
            .and_then(|id| formats.iter().find(|f| f.id == *id))
            .or(formats.first())
        else {
            continue;
        };
        let (size, fps) = (f.size, f.fps);
        let (width, height) = stream_size(size, d.height);
        let fps = match d.fps {
            Some(f) if f > 0 && f < fps && fps.is_multiple_of(f) => f,
            _ => fps,
        };
        let video_kbps = d.video_kbps.max(300);
        match encodes.iter_mut().find(|e| {
            (&e.format, e.width, e.height, e.fps, e.video_kbps)
                == (&f.id, width, height, fps, video_kbps)
        }) {
            Some(e) => e.destinations.push(d.id.clone()),
            None => encodes.push(Encode {
                format: f.id.clone(),
                width,
                height,
                fps,
                video_kbps,
                destinations: vec![d.id.clone()],
            }),
        }
    }
    let room = max.saturating_sub(recording);
    let n = encodes.len() as u32;
    let mut in_formats: Vec<&str> = encodes.iter().map(|e| e.format.as_str()).collect();
    in_formats.dedup();
    in_formats.sort();
    in_formats.dedup();
    let too_many_formats = (in_formats.len() as u32 > at_once.max(1)).then(|| {
        "This phone streams one format at a time: give every destination the same format."
            .to_string()
    });
    let message = too_many_formats.or_else(|| (n > room).then(|| {
        let what = if n == 1 { "one stream".to_string() } else { format!("{n} different stream settings") };
        let can = match room {
            0 => "no stream while it records".to_string(),
            1 => "one at a time".to_string(),
            r => format!("{r} at once"),
        };
        let beside = if recording > 0 { " beside the recording" } else { "" };
        format!(
            "This phone can't encode {what}{beside}: it can do {can}. Give destinations the same quality to share one."
        )
    }));
    StreamPlan {
        allowed: message.is_none(),
        encodes,
        message,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputState {
    Connecting,
    Live,
    Congested,
    Reconnecting { in_s: u32 },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputHealth {
    pub id: String,
    pub name: String,
    pub state: OutputState,
    pub sent_kbps: u32,
    pub target_kbps: u32,
    pub dropped_frames: u64,
    pub queue_ms: u32,
    pub reconnects: u32,
    pub message: Option<String>,
    /// Frames its encoder didn't take in time (`dropped_frames` is the network's).
    pub encoder_dropped: u64,
    /// Its encode (the index in `StreamPlan.encodes`) and that encode's size now (smaller
    /// than chosen while the resolution floor holds it down).
    pub encode: u32,
    pub width: u32,
    pub height: u32,
    /// A bonded destination's links, one per network; empty for the others.
    pub links: Vec<BondedLink>,
}

/// One network a bonded destination sends over.
#[derive(Debug, Clone, PartialEq)]
pub struct BondedLink {
    /// The network's name ("Wi-Fi", "Mobile data").
    pub name: String,
    /// Registered with the receiver and carrying the stream.
    pub up: bool,
    pub sent_kbps: u32,
}

/// The networks a bonded destination may use: each one's name and how to bind to it.
pub type Links = Vec<(String, Option<lightair_feed::net::Bind>)>;

/// What a sender says about itself.
struct Status {
    connected: bool,
    sent_bytes: u64,
    dropped: u64,
    queued_bytes: u64,
    error: Option<String>,
    /// While away: when the sender tries again.
    retry_in: Option<Duration>,
}

trait Sink: Send {
    fn parameter_sets(&mut self, nals: &[Vec<u8>]);
    fn video(&mut self, data: &[u8], format: NalFormat, pts_us: u64, keyframe: bool);
    fn audio(&mut self, data: &[u8], pts_us: u64);
    fn status(&mut self) -> Status;
    fn take_keyframe_request(&mut self) -> bool;
    /// A bonded sender's links.
    fn links(&self) -> Vec<BondedLink> {
        Vec::new()
    }
    fn set_links(&mut self, _links: Links) {}
    /// The stream's bitrate now, picture and sound (a sender may size its buffers to it).
    fn set_bitrate(&mut self, _kbps: u32) {}
}

struct Rtmp(RtmpSender);

struct Whip(crate::whip::WhipSender);

impl Sink for Whip {
    fn parameter_sets(&mut self, nals: &[Vec<u8>]) {
        self.0.parameter_sets(nals);
    }
    fn video(&mut self, data: &[u8], format: NalFormat, pts_us: u64, keyframe: bool) {
        self.0
            .video(data, format == NalFormat::AnnexB, pts_us, keyframe);
    }
    fn audio(&mut self, data: &[u8], pts_us: u64) {
        self.0.audio(data, pts_us);
    }
    fn status(&mut self) -> Status {
        let s = self.0.stats();
        Status {
            connected: s.connected,
            sent_bytes: s.sent_bytes,
            dropped: s.dropped,
            queued_bytes: self.0.queued_bytes(),
            error: s.error,
            retry_in: s.retry_in,
        }
    }
    fn take_keyframe_request(&mut self) -> bool {
        self.0.take_keyframe_request()
    }
}

impl Sink for Rtmp {
    fn parameter_sets(&mut self, nals: &[Vec<u8>]) {
        self.0.parameter_sets(nals);
    }
    fn video(&mut self, data: &[u8], format: NalFormat, pts_us: u64, keyframe: bool) {
        self.0.video(data, format, pts_us, pts_us, keyframe);
    }
    fn audio(&mut self, data: &[u8], pts_us: u64) {
        self.0.audio(data, pts_us);
    }
    fn status(&mut self) -> Status {
        let s = self.0.stats();
        Status {
            connected: s.connected,
            sent_bytes: s.sent_bytes,
            dropped: s.dropped,
            queued_bytes: self.0.queued_bytes(),
            error: s.error,
            retry_in: s.retry_in,
        }
    }
    fn take_keyframe_request(&mut self) -> bool {
        self.0.take_keyframe_request()
    }
    fn set_bitrate(&mut self, kbps: u32) {
        self.0.set_bitrate(kbps);
    }
}

struct Srt {
    sender: SrtSender,
    mux: Muxer,
    /// Bonded: the relay the sender talks to.
    relay: Option<lightair_feed::SrtlaRelay>,
}

impl Sink for Srt {
    fn parameter_sets(&mut self, nals: &[Vec<u8>]) {
        self.mux.parameter_sets(nals.to_vec());
    }
    fn video(&mut self, data: &[u8], format: NalFormat, pts_us: u64, keyframe: bool) {
        // every connection starts at a keyframe with its parameter sets in front
        if !self.sender.begin_picture(keyframe) {
            return;
        }
        let ts = self.mux.video(data, format, pts_us, None, keyframe);
        self.sender.write(&ts);
        self.sender.end_frame();
    }
    fn audio(&mut self, data: &[u8], pts_us: u64) {
        if self.sender.waiting_for_keyframe() {
            return;
        }
        let ts = self.mux.audio(data, pts_us);
        self.sender.write(&ts);
        self.sender.flush();
    }
    fn status(&mut self) -> Status {
        let s = self.sender.stats();
        Status {
            connected: s.connected,
            sent_bytes: s.sent_bytes,
            dropped: s.dropped,
            queued_bytes: self.sender.queued() as u64 * 1316,
            error: s.error,
            retry_in: s.retry_in,
        }
    }
    fn take_keyframe_request(&mut self) -> bool {
        self.sender.take_keyframe_request()
    }
    fn links(&self) -> Vec<BondedLink> {
        self.relay.as_ref().map_or_else(Vec::new, |r| {
            r.links()
                .into_iter()
                .map(|l| BondedLink {
                    name: l.name,
                    up: l.registered,
                    sent_kbps: l.sent_kbps,
                })
                .collect()
        })
    }
    fn set_links(&mut self, links: Links) {
        if let Some(r) = &self.relay {
            r.set_links(links);
        }
    }
}

/// The Test destination: takes everything as a network of `kbps` would (a queue that drains at
/// that rate, dropping what would wait more than 3 s), and sends it nowhere.
struct Test {
    kbps: Option<u32>,
    queued: f64,
    drained_at: Instant,
    sent: u64,
    dropped: u64,
    keyframe: bool,
    started: Instant,
}

impl Test {
    fn drain(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.drained_at).as_secs_f64();
        self.drained_at = now;
        let can = match self.kbps {
            Some(k) => k as f64 * 1000.0 / 8.0 * dt,
            None => f64::MAX,
        };
        let out = self.queued.min(can);
        self.queued -= out;
        self.sent += out as u64;
    }
    fn take(&mut self, n: usize) -> bool {
        self.drain();
        if let Some(k) = self.kbps
            && (self.queued + n as f64) * 8.0 / (k as f64 * 1000.0) > 3.0
        {
            self.dropped += 1;
            self.keyframe = true;
            return false;
        }
        self.queued += n as f64;
        true
    }
}

impl Sink for Test {
    fn parameter_sets(&mut self, _: &[Vec<u8>]) {}
    fn video(&mut self, data: &[u8], _: NalFormat, _: u64, _: bool) {
        self.take(data.len());
    }
    fn audio(&mut self, data: &[u8], _: u64) {
        self.take(data.len());
    }
    fn status(&mut self) -> Status {
        self.drain();
        // "connects" after a moment, as a server would
        let connected = self.started.elapsed() > Duration::from_millis(300);
        Status {
            connected,
            sent_bytes: self.sent,
            dropped: self.dropped,
            queued_bytes: self.queued as u64,
            error: None,
            retry_in: None,
        }
    }
    fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.keyframe)
    }
}

/// An SRT address's parts (moved to the kit: the broadcast extension reads them too).
fn parse_srt(url: &str) -> Result<SrtTarget, String> {
    lightair_feed::srt::parse_url(url)
}

/// One destination being streamed to.
pub struct Output {
    pub dest: Destination,
    video_encoder: std::sync::atomic::AtomicU32,
    pub audio_encoder: u32,
    sink: Mutex<Box<dyn Sink>>,
    watch: Mutex<Watch>,
    delay: Mutex<Delay>,
}

/// A destination's state machine and the bitrate it has stepped to.
struct Watch {
    state: OutputState,
    target: u32,
    calm_since: Option<Instant>,
    last_step: Option<Instant>,
    sent: (Instant, u64),
    sent_kbps: u32,
    dropped: u64,
    reconnects: u32,
    lost_at: Option<Instant>,
    ever_connected: bool,
    /// Since when it's been congested at the floor.
    floor_since: Option<Instant>,
    message: Option<String>,
    /// Its encode's picture now, as a share of the chosen one's pixels (the floor follows it),
    /// and whether no smaller size is left (then there's no floor above the least bitrate:
    /// stepping down is all that's left).
    size: (f32, bool),
    /// What the encode actually gives this destination (picture and sound, kbps, smoothed),
    /// from the bytes sent plus queued: a simple picture can come out far under the target.
    produced: Option<u32>,
    produced_bytes: u64,
    /// The rate the sender's buffers were last sized to.
    buffered_kbps: u32,
    /// Since when the network has carried the stream with no congestion (the floor's clock
    /// is cleared only after a few seconds of that).
    clear_since: Option<Instant>,
}

/// The least bitrate a stream steps down to.
const LEAST_KBPS: u32 = 300;

/// The rate a stream really runs at: what its encode gives it (once known), at most `target`,
/// at least the least bitrate.
fn stream_rate(target: u32, produced: Option<u32>) -> u32 {
    produced
        .map_or(target, |p| p.clamp(LEAST_KBPS, target.max(LEAST_KBPS)))
        .max(1)
}

/// More than this waiting is congestion (1.5 s); less than `CALM` is a short queue.
const CONGESTED_MS: u32 = 1500;
const CALM_MS: u32 = 300;

/// The bitrate floor: 30 % of the chosen bitrate, in proportion to the picture's pixels
/// (`size`: its share of the chosen picture's, and whether it's the smallest). Under it the
/// picture steps down a size; at the smallest there's nothing to step to, so only the least.
fn floor(chosen: u32, (share, smallest): (f32, bool)) -> u32 {
    if smallest {
        return LEAST_KBPS;
    }
    (((chosen * 3 / 10) as f32 * share.clamp(0.0, 1.0)) as u32).max(LEAST_KBPS)
}

impl Output {
    pub fn start(
        dest: Destination,
        video_encoder: u32,
        audio_encoder: u32,
        bind: Option<lightair_feed::net::Bind>,
        links: Links,
    ) -> Result<Output, String> {
        let sink: Box<dyn Sink> = match &dest.kind {
            DestinationKind::Test { upload_kbps } => Box::new(Test {
                kbps: *upload_kbps,
                queued: 0.0,
                drained_at: Instant::now(),
                sent: 0,
                dropped: 0,
                keyframe: false,
                started: Instant::now(),
            }),
            DestinationKind::Rtmp { url, key } => {
                let s = RtmpSender::start(RtmpTarget {
                    url: url.clone(),
                    key: key.clone(),
                    bind: bind.clone(),
                })?;
                s.audio_config(48_000, 2);
                s.set_bitrate(dest.video_kbps + dest.audio_kbps);
                Box::new(Rtmp(s))
            }
            DestinationKind::Srt { url } => {
                let mut target = parse_srt(url)?;
                target.bind = bind.clone();
                let sender = SrtSender::start(target).map_err(|e| e.to_string())?;
                let mux = Muxer::new(
                    VideoCodec::H264,
                    Some(AudioConfig {
                        sample_rate: 48_000,
                        channels: 2,
                    }),
                );
                Box::new(Srt {
                    sender,
                    mux,
                    relay: None,
                })
            }
            DestinationKind::Srtla { url } => {
                let srt = url
                    .strip_prefix("srtla://")
                    .map(|rest| format!("srt://{rest}"))
                    .ok_or("A bonded address has to start with srtla://.")?;
                let mut target = parse_srt(&srt)?;
                // the receiver's address, and the relay in its place for the SRT sender
                let relay = lightair_feed::SrtlaRelay::start(target.address, links)
                    .map_err(|e| format!("Bonding couldn't start: {e}"))?;
                target.address = relay.local();
                let sender = SrtSender::start(target).map_err(|e| e.to_string())?;
                let mux = Muxer::new(
                    VideoCodec::H264,
                    Some(AudioConfig {
                        sample_rate: 48_000,
                        channels: 2,
                    }),
                );
                Box::new(Srt {
                    sender,
                    mux,
                    relay: Some(relay),
                })
            }
            DestinationKind::Whip { url, token } => Box::new(Whip(crate::whip::WhipSender::start(
                url.clone(),
                token.clone(),
            )?)),
            DestinationKind::Platform { .. } => {
                return Err("its server and key weren't found".into());
            }
        };
        let target = dest.video_kbps;
        let buffered_kbps = target + dest.audio_kbps;
        let dest_delay = dest.delay_s.unwrap_or(0);
        Ok(Output {
            dest,
            video_encoder: std::sync::atomic::AtomicU32::new(video_encoder),
            audio_encoder,
            sink: Mutex::new(sink),
            watch: Mutex::new(Watch {
                state: OutputState::Connecting,
                target,
                calm_since: None,
                last_step: None,
                sent: (Instant::now(), 0),
                sent_kbps: 0,
                dropped: 0,
                reconnects: 0,
                lost_at: None,
                ever_connected: false,
                floor_since: None,
                message: None,
                size: (1.0, false),
                produced: None,
                produced_bytes: 0,
                buffered_kbps,
                clear_since: None,
            }),
            delay: Mutex::new(Delay {
                delay: Duration::from_secs(dest_delay.min(120) as u64),
                ..Default::default()
            }),
        })
    }

    pub fn parameter_sets(&self, nals: &[Vec<u8>]) {
        if !self.hold(Held::Sets(nals.to_vec())) {
            self.sink.lock().parameter_sets(nals);
        }
    }

    pub fn video(&self, data: &[u8], length_prefixed: bool, pts_ns: i64, keyframe: bool) {
        let format = if length_prefixed {
            NalFormat::LengthPrefixed
        } else {
            NalFormat::AnnexB
        };
        let us = (pts_ns.max(0) / 1000) as u64;
        if !self.hold(Held::Video(data.to_vec(), format, us, keyframe)) {
            self.sink.lock().video(data, format, us, keyframe);
        }
    }

    pub fn audio(&self, data: &[u8], pts_ns: i64) {
        let us = (pts_ns.max(0) / 1000) as u64;
        if !self.hold(Held::Audio(data.to_vec(), us)) {
            self.sink.lock().audio(data, us);
        }
    }

    /// A delayed destination: `item` queued, and what's due sent. False without a
    /// delay (the caller sends it at once).
    fn hold(&self, item: Held) -> bool {
        let mut d = self.delay.lock();
        if d.delay.is_zero() {
            return false;
        }
        let now = Instant::now();
        d.bytes += match &item {
            Held::Sets(n) => n.iter().map(Vec::len).sum(),
            Held::Video(b, ..) | Held::Audio(b, _) => b.len(),
        };
        let due = now + d.delay;
        d.queue.push_back((due, item));
        while d.bytes > DELAY_BYTES {
            let Some((_, old)) = d.queue.pop_front() else {
                break;
            };
            d.bytes -= match &old {
                Held::Sets(n) => n.iter().map(Vec::len).sum(),
                Held::Video(b, ..) | Held::Audio(b, _) => b.len(),
            };
        }
        drop(d);
        self.release(now);
        true
    }

    /// Sends what's waited its delay.
    fn release(&self, now: Instant) {
        let mut d = self.delay.lock();
        let mut sink = self.sink.lock();
        while d.queue.front().is_some_and(|(due, _)| *due <= now) {
            let Some((_, item)) = d.queue.pop_front() else {
                break;
            };
            match item {
                Held::Sets(n) => {
                    d.bytes -= n.iter().map(Vec::len).sum::<usize>();
                    sink.parameter_sets(&n);
                }
                Held::Video(b, f, us, key) => {
                    d.bytes -= b.len();
                    sink.video(&b, f, us, key);
                }
                Held::Audio(b, us) => {
                    d.bytes -= b.len();
                    sink.audio(&b, us);
                }
            }
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.watch.lock().target
    }

    /// Once a second: the state, the bitrate step, what to say. Returns (the new target
    /// bitrate if it changed, whether a keyframe is wanted, a message if the state changed).
    pub fn watch(&self, now: Instant) -> (Option<u32>, bool, Option<String>) {
        // a delayed destination's due packets, even when nothing new comes
        self.release(now);
        let st = self.sink.lock().status();
        let keyframe = self.sink.lock().take_keyframe_request();
        let mut w = self.watch.lock();
        let chosen = self.dest.video_kbps.max(1);
        let dt = now.duration_since(w.sent.0).as_secs_f64().max(0.001);
        w.sent_kbps = ((st.sent_bytes.saturating_sub(w.sent.1)) as f64 * 8.0 / 1000.0 / dt) as u32;
        w.sent = (now, st.sent_bytes);
        // the queue's wait at the rate the stream really runs at: the encode's own (a simple
        // picture comes out far under the target), never more than the target. Measured at the
        // target alone, a 640 kbps stream throttled to 250 waited many seconds before it counted
        // as congested, and never stepped down
        let fed = st.sent_bytes + st.queued_bytes;
        if st.connected && fed >= w.produced_bytes && w.produced_bytes > 0 {
            let now_kbps = ((fed - w.produced_bytes) as f64 * 8.0 / 1000.0 / dt) as u32;
            w.produced = Some(w.produced.map_or(now_kbps, |p| (p * 3 + now_kbps) / 4));
        }
        w.produced_bytes = fed;
        let rate_kbps = stream_rate(w.target + self.dest.audio_kbps, w.produced);
        let queue_ms = (st.queued_bytes * 8 / rate_kbps as u64) as u32;
        let dropped_more = st.dropped > w.dropped;
        w.dropped = st.dropped;
        let name = &self.dest.name;
        let before = w.state.clone();
        let mut new_target = None;
        if !st.connected {
            let lost = *w.lost_at.get_or_insert(now);
            if w.ever_connected {
                // the sender's backoff (0.5, 1, 2, 4, 8 s, then every 8 s): when it tries again
                let in_s = st
                    .retry_in
                    .map_or(1, |d| d.as_secs_f32().ceil() as u32)
                    .max(1);
                w.state = OutputState::Reconnecting { in_s };
            } else if now.duration_since(lost) > Duration::from_secs(20) {
                w.state = OutputState::Failed {
                    reason: st
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("{name} didn't answer.")),
                };
            }
        } else {
            if !w.ever_connected || w.lost_at.is_some() {
                if w.ever_connected {
                    w.reconnects += 1;
                }
                w.ever_connected = true;
                w.lost_at = None;
            }
            let congested = queue_ms > CONGESTED_MS || dropped_more;
            let may_step = w
                .last_step
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2));
            let floor = floor(chosen, w.size);
            // at the floor, or the network carries less than it (the 20 % steps would take
            // many seconds to get there): only a smaller picture helps
            let carried = w.sent_kbps.saturating_sub(self.dest.audio_kbps);
            // clear: no congestion, and the network carries what the stream really sends.
            // While it's short the floor's clock keeps running: dropping a whole GOP empties
            // the queue for a second or two, which used to reset it every time, so a throttled
            // stream dropped pictures for the whole throttle and never went a size down
            let short = (w.sent_kbps as u64) * 5
                < stream_rate(w.target + self.dest.audio_kbps, w.produced) as u64 * 4;
            if congested || short {
                w.clear_since = None;
            } else {
                w.clear_since.get_or_insert(now);
            }
            if congested && (w.target <= floor || carried < floor) {
                w.floor_since.get_or_insert(now);
            } else if w
                .clear_since
                .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(3))
            {
                w.floor_since = None;
            }
            if congested {
                w.calm_since = None;
                if may_step && w.target > floor {
                    w.target = (w.target * 4 / 5).max(floor);
                    w.last_step = Some(now);
                    new_target = Some(w.target);
                }
                w.state = OutputState::Congested;
            } else {
                if queue_ms < CALM_MS {
                    let since = *w.calm_since.get_or_insert(now);
                    if now.duration_since(since) >= Duration::from_secs(10) && w.target < chosen {
                        w.target = (w.target + chosen / 10).min(chosen);
                        w.calm_since = Some(now);
                        w.last_step = Some(now);
                        new_target = Some(w.target);
                    }
                }
                w.state = OutputState::Live;
            }
        }
        let message = match (&before, &w.state) {
            (a, b)
                if std::mem::discriminant(a) == std::mem::discriminant(b)
                    && new_target.is_none() =>
            {
                None
            }
            (_, OutputState::Live) if new_target.is_some() => Some(format!(
                "The network is better: streaming to {name} at {} kbps.",
                w.target
            )),
            (_, OutputState::Live)
                if w.reconnects > 0 && matches!(before, OutputState::Reconnecting { .. }) =>
            {
                Some(format!("Back on {name}. Nothing ended."))
            }
            (_, OutputState::Live) => Some(format!("Live on {name}.")),
            (_, OutputState::Congested) => Some(format!(
                "Your network slowed down: streaming to {name} at {} kbps.",
                w.target
            )),
            (_, OutputState::Reconnecting { in_s }) => {
                Some(format!("Reconnecting to {name}… {in_s} s"))
            }
            (_, OutputState::Failed { reason }) => {
                Some(format!("{name} couldn't be reached: {reason}"))
            }
            (_, OutputState::Connecting) => None,
        };
        if message.is_some() {
            w.message = message.clone();
        }
        let keyframe = keyframe
            || (st.connected
                && before != OutputState::Live
                && w.state == OutputState::Live
                && w.reconnects > 0);
        // the sender's buffers hold about a second of what the stream really sends
        let buffer_kbps = stream_rate(w.target + self.dest.audio_kbps, w.produced);
        let resize =
            new_target.is_some() || buffer_kbps.abs_diff(w.buffered_kbps) > w.buffered_kbps / 4;
        if resize {
            w.buffered_kbps = buffer_kbps;
        }
        drop(w);
        if resize {
            self.sink.lock().set_bitrate(buffer_kbps);
        }
        (new_target, keyframe, message)
    }

    /// The phone's networks changed: a bonded destination's links follow them.
    pub fn set_links(&self, links: Links) {
        self.sink.lock().set_links(links);
    }

    pub fn bonded(&self) -> bool {
        matches!(self.dest.kind, DestinationKind::Srtla { .. })
    }

    pub fn health(&self) -> OutputHealth {
        let mut sink = self.sink.lock();
        let st = sink.status();
        let links = sink.links();
        drop(sink);
        let w = self.watch.lock();
        let rate = (w.target + self.dest.audio_kbps).max(1) as u64;
        OutputHealth {
            id: self.dest.id.clone(),
            name: self.dest.name.clone(),
            state: w.state.clone(),
            sent_kbps: w.sent_kbps,
            target_kbps: w.target,
            dropped_frames: st.dropped,
            queue_ms: (st.queued_bytes * 8 / rate) as u32,
            reconnects: w.reconnects,
            message: w.message.clone(),
            encoder_dropped: 0,
            encode: 0,
            width: 0,
            height: 0,
            links,
        }
    }

    /// Congested at the floor (30 % of the chosen bitrate, for the picture's size), or carrying
    /// less than it, for 4 s: only a smaller picture helps now.
    pub fn at_floor(&self, now: Instant) -> bool {
        self.watch
            .lock()
            .floor_since
            .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(4))
    }

    /// Live at its full chosen bitrate with a short queue for a minute: the picture may grow.
    pub fn steady_at_full(&self, now: Instant) -> bool {
        let w = self.watch.lock();
        w.state == OutputState::Live
            && w.target >= self.dest.video_kbps.max(300)
            && w.calm_since
                .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(60))
    }

    /// How long it's been away since it was last connected (reconnecting), if it is.
    pub fn away_for(&self, now: Instant) -> Option<Duration> {
        let w = self.watch.lock();
        w.lost_at
            .filter(|_| w.ever_connected)
            .map(|t| now.duration_since(t))
    }

    /// The bitrate it would start at with its encode at `size` (a share of the chosen
    /// picture's pixels, and whether it's the smallest): what the network carried lately, not
    /// above the bitrate it has stepped to nor under the floor there.
    pub fn kbps_at(&self, size: (f32, bool)) -> u32 {
        let w = self.watch.lock();
        let carried = w.sent_kbps.saturating_sub(self.dest.audio_kbps);
        w.target
            .min(carried.max(floor(self.dest.video_kbps.max(1), size)))
    }

    /// Its encode's picture (see [`Self::kbps_at`]), for the floor.
    pub fn set_size(&self, size: (f32, bool)) {
        self.watch.lock().size = size;
    }

    /// Its encode was reopened at `size`, a smaller one (`down`, starting at
    /// [`Self::kbps_at`]) or a larger one: the floor follows the picture, and the floor and the
    /// calm count again.
    pub fn size_changed(&self, size: (f32, bool), down: bool) {
        let kbps = self.kbps_at(size);
        let mut w = self.watch.lock();
        if down {
            w.target = kbps;
        }
        w.size = size;
        w.floor_since = None;
        w.calm_since = None;
        let total = stream_rate(w.target + self.dest.audio_kbps, w.produced);
        w.buffered_kbps = total;
        drop(w);
        self.sink.lock().set_bitrate(total);
    }

    pub fn video_encoder(&self) -> u32 {
        self.video_encoder.load(Ordering::Acquire)
    }

    /// Its packets come from another encoder from now on (the same picture at another size).
    pub fn set_video_encoder(&self, encoder: u32) {
        self.video_encoder.store(encoder, Ordering::Release);
    }
}

/// The stream's picture: the format scaled down so its short side is `target` (even sizes),
/// never up. A target longer than the format's short side is read as its long side (a portrait
/// picture's height: 1280 for 720p on a 1080×1920 format).
pub(crate) fn stream_size((w, h): (u32, u32), target: Option<u32>) -> (u32, u32) {
    let (short, long) = (w.min(h), w.max(h));
    let k = match target {
        Some(t) if t > 0 && t < short => t as f32 / short as f32,
        Some(t) if t > short && t < long => t as f32 / long as f32,
        _ => return (w, h),
    };
    (
        ((w as f32 * k).round() as u32) & !1,
        ((h as f32 * k).round() as u32) & !1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(kbps: Option<u32>) -> Destination {
        Destination {
            id: "t".into(),
            name: "Test".into(),
            kind: DestinationKind::Test { upload_kbps: kbps },
            video_kbps: 4000,
            audio_kbps: 160,
            height: None,
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        }
    }

    /// A network the test drives: what the encode fed, what the network carried, what was
    /// dropped, second by second.
    #[derive(Default)]
    struct Net {
        fed: u64,
        sent: u64,
        dropped_bytes: u64,
        drops: u64,
    }

    struct Scripted(std::sync::Arc<Mutex<Net>>);

    impl Sink for Scripted {
        fn parameter_sets(&mut self, _: &[Vec<u8>]) {}
        fn video(&mut self, _: &[u8], _: NalFormat, _: u64, _: bool) {}
        fn audio(&mut self, _: &[u8], _: u64) {}
        fn status(&mut self) -> Status {
            let n = self.0.lock();
            Status {
                connected: true,
                sent_bytes: n.sent,
                dropped: n.drops,
                queued_bytes: n.fed - n.sent - n.dropped_bytes,
                error: None,
                retry_in: None,
            }
        }
        fn take_keyframe_request(&mut self) -> bool {
            false
        }
    }

    /// `secs` seconds of an encode giving `fed_kbps` into a network carrying `net_kbps`
    /// (`drop_every`: the sender drops what's waiting, a GOP, every that many seconds): the
    /// second the stream asked for a smaller picture, if it did.
    fn throttled(
        video_kbps: u32,
        fed_kbps: u64,
        net_kbps: u64,
        drop_every: Option<u64>,
        secs: u64,
    ) -> Option<u64> {
        let mut d = dest(None);
        (d.video_kbps, d.audio_kbps) = (video_kbps, 128);
        let o = Output::start(d, 1, 2, None, Vec::new()).unwrap();
        let net = std::sync::Arc::new(Mutex::new(Net::default()));
        *o.sink.lock() = Box::new(Scripted(net.clone()));
        let t0 = Instant::now();
        for s in 1..=secs {
            {
                let mut n = net.lock();
                n.fed += fed_kbps * 1000 / 8;
                let waiting = n.fed - n.sent - n.dropped_bytes;
                n.sent += waiting.min(net_kbps * 1000 / 8);
                if drop_every.is_some_and(|e| s % e == 0) {
                    n.dropped_bytes += n.fed - n.sent - n.dropped_bytes;
                    n.drops += 1;
                }
            }
            let now = t0 + Duration::from_secs(s);
            o.watch(now);
            if o.at_floor(now) {
                return Some(s);
            }
        }
        None
    }

    /// A simple picture comes out far under its target: 768 kbps of a 3500 kbps stream into a
    /// 250 kbps throttle. Its queue is measured at what it really sends, so it goes a size down
    /// within seconds (measured at the target, its wait looked short for most of a minute).
    #[test]
    fn a_picture_far_under_its_target_still_steps_down_under_a_throttle() {
        let at = throttled(3500, 768, 250, None, 15);
        assert!(at.is_some_and(|s| s <= 10), "a size down at {at:?}");
    }

    /// Under a throttle the sender drops whole GOPs, emptying the queue for a moment: that no
    /// longer resets the clock towards a smaller picture, so the stream goes a size down instead
    /// of dropping pictures for the whole throttle.
    #[test]
    fn dropping_pictures_under_a_throttle_doesnt_hold_off_a_smaller_picture() {
        let at = throttled(640, 768, 250, Some(2), 15);
        assert!(at.is_some_and(|s| s <= 8), "a size down at {at:?}");
    }

    #[test]
    fn a_phone_drawing_one_format_is_told_so() {
        let d = |id: &str, format: Option<&str>| Destination {
            id: id.into(),
            name: id.into(),
            kind: DestinationKind::Test { upload_kbps: None },
            video_kbps: 2000,
            audio_kbps: 128,
            height: None,
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: format.map(String::from),
        };
        let formats = [
            Format {
                id: "tall".into(),
                size: (1080, 1920),
                fps: 30,
            },
            Format {
                id: "wide".into(),
                size: (1920, 1080),
                fps: 30,
            },
        ];
        let dests = [d("a", None), d("b", Some("wide")), d("c", Some("gone"))];
        let p = plan_in(&dests, &formats, 3, 0, 2);
        assert!(p.allowed);
        assert_eq!(
            p.encodes
                .iter()
                .map(|e| (e.format.as_str(), e.width, e.height, e.destinations.len()))
                .collect::<Vec<_>>(),
            [("tall", 1080, 1920, 2), ("wide", 1920, 1080, 1)],
            "a format that's gone is the active one"
        );
        let p = plan_in(&dests, &formats, 3, 0, 1);
        assert!(!p.allowed);
        assert!(p.message.unwrap().contains("one format at a time"));
    }

    #[test]
    fn a_slow_network_steps_the_bitrate_down_and_back_up() {
        let o = Output::start(dest(Some(1000)), 1, 2, None, Vec::new()).unwrap();
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(350));
        // 4 Mbps into a 1 Mbps network for a few seconds: the queue passes 1.5 s
        let frame = vec![0u8; 4_000_000 / 8 / 30];
        for _ in 0..90 {
            o.video(&frame, false, 0, false);
        }
        let (t, _, msg) = o.watch(t0 + Duration::from_secs(3));
        assert_eq!(t, Some(3200));
        assert!(msg.unwrap().contains("slowed down"));
        assert_eq!(o.health().state, OutputState::Congested);
        // never below 30 % of the chosen rate
        let mut now = t0 + Duration::from_secs(3);
        for _ in 0..20 {
            now += Duration::from_secs(2);
            for _ in 0..30 {
                o.video(&frame, false, 0, false);
            }
            o.watch(now);
        }
        assert_eq!(o.target_kbps(), 1200);
    }

    /// 720p at 3500 kbps on a 600 kbps network (Stream Check's throttle): the 30 % floor
    /// (1050) is above what the network carries, so the picture must step down a size, and
    /// promptly, not after the 20 % steps have walked down to the floor (6 of them, 2 s apart,
    /// then 4 s at the floor: the 15 s throttle was over first, every picture dropped); at the
    /// smaller size the floor follows the picture, and it starts at what the network carried.
    #[test]
    fn the_bitrate_floor_follows_the_picture_size() {
        assert_eq!(
            floor(3500, (0.5625, false)),
            590,
            "the floor follows the picture"
        );
        assert_eq!(
            floor(3500, (0.5625, true)),
            LEAST_KBPS,
            "none at the smallest"
        );
    }

    #[test]
    fn a_calm_network_steps_back_up_to_the_chosen_rate() {
        let o = Output::start(dest(None), 1, 2, None, Vec::new()).unwrap();
        o.watch.lock().target = 2000;
        std::thread::sleep(Duration::from_millis(350));
        let t0 = Instant::now();
        assert_eq!(o.watch(t0).2.as_deref(), Some("Live on Test."));
        assert_eq!(o.watch(t0 + Duration::from_secs(11)).0, Some(2400));
    }

    #[test]
    fn destinations_share_an_encode_only_with_the_same_settings() {
        let d = |id: &str, kbps: u32, height: Option<u32>, fps: Option<u32>| Destination {
            id: id.into(),
            name: id.into(),
            kind: DestinationKind::Test { upload_kbps: None },
            video_kbps: kbps,
            audio_kbps: 160,
            height,
            fps,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        };
        let dests = [
            d("a", 6000, None, None),
            d("b", 6000, Some(1080), Some(60)),
            d("c", 6000, Some(720), None),
            d("d", 6000, None, Some(30)),
            d("e", 6000, None, Some(25)),
        ];
        let p = plan(&dests, (1080, 1920), 60, 3, 0);
        let groups: Vec<_> = p
            .encodes
            .iter()
            .map(|e| (e.width, e.height, e.fps, e.destinations.join("")))
            .collect();
        // 1080 is the format's; 25 isn't a divisor of 60: the format's rate
        assert_eq!(
            groups,
            [
                (1080, 1920, 60, "abe".to_string()),
                (720, 1280, 60, "c".to_string()),
                (1080, 1920, 30, "d".to_string()),
            ]
        );
        assert!(p.allowed);
        // a recording takes one of the three
        let p = plan(&dests, (1080, 1920), 60, 3, 1);
        assert!(!p.allowed);
        assert!(p.message.unwrap().contains("beside the recording"));
        assert!(plan(&dests[..1], (1080, 1920), 60, 1, 0).allowed);
    }

    #[test]
    fn a_platform_destination_is_its_server_and_key_within_its_limits() {
        let d = |platform: &str, server: Option<&str>, key: Option<&str>| Destination {
            id: platform.into(),
            name: platform.into(),
            kind: DestinationKind::Platform {
                platform: platform.into(),
                server: server.map(String::from),
                key: key.map(String::from),
            },
            video_kbps: 8000,
            audio_kbps: 160,
            height: None,
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        };
        let t = d(
            "twitch",
            Some("rtmp://fra05.contribute.live-video.net/app/"),
            Some("k"),
        )
        .resolve("")
        .unwrap()
        .0;
        assert_eq!(
            t.kind,
            DestinationKind::Rtmp {
                url: "rtmp://fra05.contribute.live-video.net/app/".into(),
                key: "k".into()
            }
        );
        // Twitch takes 6 Mbps at 1080p60 at most
        assert_eq!(
            (t.video_kbps, t.height, t.fps),
            (6000, Some(1080), Some(60))
        );
        // YouTube's first server when none is chosen
        let y = d("youtube", None, Some("k")).resolve("").unwrap().0;
        assert!(
            matches!(&y.kind, DestinationKind::Rtmp { url, .. } if url == "rtmps://a.rtmps.youtube.com:443/live2")
        );
        // Kick's server is every account's own; no key, no stream
        assert!(
            d("kick", None, Some("k"))
                .resolve("")
                .unwrap_err()
                .contains("paste it")
        );
        assert!(
            d(
                "kick",
                Some("rtmps://x.global-contribute.live-video.net/app/"),
                None
            )
            .resolve("")
            .unwrap_err()
            .contains("paste the stream key")
        );
        // Twitch's key can come through sign-in: not signed in here, so it says how
        assert!(
            d(
                "twitch",
                Some("rtmp://fra05.contribute.live-video.net/app/"),
                None
            )
            .resolve("")
            .unwrap_err()
            .contains("sign in to Twitch")
        );
        // the plan sees the limits too: an 8 Mbps Twitch destination shares a 6 Mbps encode
        let p = plan(
            &[d("twitch", None, None), {
                let mut o = d("twitch", None, None);
                o.id = "b".into();
                o.video_kbps = 6000;
                o
            }],
            (1920, 1080),
            60,
            3,
            0,
        );
        assert_eq!(p.encodes.len(), 1);
        assert_eq!(p.encodes[0].video_kbps, 6000);
    }

    #[test]
    fn a_delayed_destination_sends_each_packet_its_delay_later() {
        let mut d = dest(None);
        d.delay_s = Some(1);
        let o = Output::start(d, 1, 2, None, Vec::new()).unwrap();
        let sent = || o.sink.lock().status().sent_bytes;
        o.parameter_sets(&[vec![0x67, 1], vec![0x68, 2]]);
        for i in 0..10 {
            o.video(&[0, 0, 0, 1, 0x65, 9, 9, 9], false, i * 33_000_000, i == 0);
            o.audio(&[1, 2, 3], i * 20_000_000);
        }
        assert_eq!(sent(), 0, "held");
        // a second on (watch takes the time: no real wait)
        o.watch(Instant::now() + Duration::from_millis(1100));
        assert_eq!(sent(), 10 * 8 + 10 * 3, "all of it, a second later");
    }

    #[test]
    fn away_counts_only_after_it_was_connected() {
        let o = Output::start(dest(None), 1, 2, None, Vec::new()).unwrap();
        let now = Instant::now();
        assert_eq!(o.away_for(now), None);
        {
            let mut w = o.watch.lock();
            w.lost_at = Some(now - Duration::from_secs(3));
        }
        assert_eq!(o.away_for(now), None, "never connected: starting, not away");
        o.watch.lock().ever_connected = true;
        assert!(o.away_for(now).unwrap() >= Duration::from_secs(3));
    }

    #[test]
    fn srt_addresses_are_read() {
        let t = parse_srt(
            "srt://127.0.0.1:9000?streamid=live%2Fme&passphrase=secret123456&latency=200",
        )
        .unwrap();
        assert_eq!(
            (
                t.address.port(),
                t.stream_id(),
                t.passphrase.as_str(),
                t.latency
            ),
            (
                9000,
                "live/me".to_string(),
                "secret123456",
                Duration::from_millis(200)
            )
        );
        assert!(parse_srt("rtmp://x").is_err());
    }
}
