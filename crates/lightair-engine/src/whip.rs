//! WHIP (WebRTC-HTTP ingestion, RFC 9725) for a destination: WebRTC in Rust (webrtc-rs) on its
//! own thread and runtime, sending the stream encoder's H.264 and the host's Opus. One `POST` of
//! the offer to the destination's URL (with its bearer token), the answer back, the session's
//! `Location` deleted at the end. Reconnects by itself with `lightair_feed::backoff`, asking for a
//! keyframe each time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine,
};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::default_runtime;

/// What waits to be sent: at most this many samples (about 2 s of both).
const QUEUE: usize = 180;

enum Media {
    Video(Bytes, u64),
    Audio(Bytes, u64),
}

#[derive(Default)]
struct Counters {
    connected: AtomicBool,
    sent_bytes: AtomicU64,
    dropped: AtomicU64,
    keyframe: AtomicBool,
    queued: AtomicU64,
    /// Bytes of what waits.
    queued_bytes: AtomicU64,
    error: Mutex<Option<String>>,
    retry_at: Mutex<Option<Instant>>,
    stop: AtomicBool,
}

/// What the sender has done (as the RTMP and SRT senders say it).
pub struct WhipStats {
    pub connected: bool,
    pub sent_bytes: u64,
    pub dropped: u64,
    pub error: Option<String>,
    pub retry_in: Option<Duration>,
}

pub struct WhipSender {
    tx: tokio::sync::mpsc::Sender<Media>,
    counters: Arc<Counters>,
    /// SPS and PPS (each without a start code), put in front of every keyframe.
    parameter_sets: Vec<Vec<u8>>,
    /// Pictures wait for a keyframe after each connection and after a drop.
    waiting_key: bool,
}

impl WhipSender {
    pub fn start(url: String, token: Option<String>) -> Result<WhipSender, String> {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("A WHIP address starts with https://.".into());
        }
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
        let counters = Arc::new(Counters::default());
        let c = counters.clone();
        std::thread::Builder::new()
            .name("lightair-whip".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        *c.error.lock() = Some(e.to_string());
                        return;
                    }
                };
                rt.block_on(run(url, token, rx, c));
            })
            .map_err(|e| e.to_string())?;
        Ok(WhipSender {
            tx,
            counters,
            parameter_sets: Vec::new(),
            waiting_key: true,
        })
    }

    pub fn parameter_sets(&mut self, nals: &[Vec<u8>]) {
        self.parameter_sets = nals.to_vec();
    }

    /// One picture, as Annex B NAL units (`annex_b`) or with 4-byte lengths.
    pub fn video(&mut self, data: &[u8], annex_b: bool, pts_us: u64, keyframe: bool) {
        let c = &self.counters;
        if !c.connected.load(Ordering::Relaxed) {
            self.waiting_key = true;
            return;
        }
        if self.waiting_key && !keyframe {
            c.keyframe.store(true, Ordering::Relaxed);
            return;
        }
        let mut out = Vec::with_capacity(data.len() + 64);
        if keyframe {
            for p in &self.parameter_sets {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(p);
            }
        }
        if annex_b {
            out.extend_from_slice(data);
        } else {
            let mut rest = data;
            while rest.len() >= 4 {
                let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                let Some(nal) = rest.get(4..4 + n) else { break };
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
                rest = &rest[4 + n..];
            }
        }
        let n = out.len() as u64;
        if self.tx.try_send(Media::Video(out.into(), pts_us)).is_ok() {
            self.waiting_key &= !keyframe;
            c.queued.fetch_add(1, Ordering::Relaxed);
            c.queued_bytes.fetch_add(n, Ordering::Relaxed);
        } else {
            // full: start again from a keyframe
            self.waiting_key = true;
            c.dropped.fetch_add(1, Ordering::Relaxed);
            c.keyframe.store(true, Ordering::Relaxed);
        }
    }

    /// One Opus packet.
    pub fn audio(&mut self, data: &[u8], pts_us: u64) {
        let c = &self.counters;
        if !c.connected.load(Ordering::Relaxed) || self.waiting_key {
            return;
        }
        if self
            .tx
            .try_send(Media::Audio(Bytes::copy_from_slice(data), pts_us))
            .is_ok()
        {
            c.queued.fetch_add(1, Ordering::Relaxed);
            c.queued_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
    }

    /// Bytes waiting to go out.
    pub fn queued_bytes(&self) -> u64 {
        self.counters.queued_bytes.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> WhipStats {
        let c = &self.counters;
        WhipStats {
            connected: c.connected.load(Ordering::Relaxed),
            sent_bytes: c.sent_bytes.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            error: c.error.lock().clone(),
            retry_in: c
                .retry_at
                .lock()
                .map(|t| t.saturating_duration_since(Instant::now())),
        }
    }

    pub fn take_keyframe_request(&self) -> bool {
        self.counters.keyframe.swap(false, Ordering::Relaxed)
    }
}

impl Drop for WhipSender {
    fn drop(&mut self) {
        self.counters.stop.store(true, Ordering::Relaxed);
    }
}

async fn run(
    url: String,
    token: Option<String>,
    mut rx: tokio::sync::mpsc::Receiver<Media>,
    c: Arc<Counters>,
) {
    let mut backoff = lightair_feed::backoff::Backoff::default();
    while !c.stop.load(Ordering::Relaxed) {
        let result = session(&url, token.as_deref(), &mut rx, &c, &mut backoff).await;
        c.connected.store(false, Ordering::Relaxed);
        match result {
            Ok(()) => return,
            Err(e) => {
                tracing::debug!("whip: {e}");
                *c.error.lock() = Some(e);
                let wait = backoff.next(Instant::now());
                *c.retry_at.lock() = Some(Instant::now() + wait);
                // what waits meanwhile is stale
                let until = tokio::time::Instant::now() + wait;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(until) => break,
                        m = rx.recv() => match m {
                            Some(m) => taken(&c, &m),
                            None => return,
                        },
                    }
                }
            }
        }
    }
}

/// A sample left the queue.
fn taken(c: &Counters, m: &Media) {
    let n = match m {
        Media::Video(d, _) | Media::Audio(d, _) => d.len() as u64,
    };
    c.queued.fetch_sub(1, Ordering::Relaxed);
    c.queued_bytes.fetch_sub(n, Ordering::Relaxed);
}

#[derive(Clone)]
struct Handler {
    gathered: tokio::sync::mpsc::Sender<()>,
    state: tokio::sync::watch::Sender<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state.send(state);
    }
}

fn codecs() -> (RTCRtpCodecParameters, RTCRtpCodecParameters) {
    let video = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
    };
    let audio = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
    };
    (video, audio)
}

fn track(
    kind: RtpCodecKind,
    codec: &RTCRtpCodec,
    ssrc: u32,
) -> Result<TrackLocalStaticSample, String> {
    TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "lightair".into(),
            format!("lightair-{kind}"),
            format!("lightair-{kind}"),
            kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec.clone(),
                ..Default::default()
            }],
        ),
    )
    .map_err(|e| e.to_string())
}

/// The WHIP exchange: the offer posted, the answer and the session's address back.
fn post_offer(
    url: &str,
    token: Option<&str>,
    offer: &str,
) -> Result<(String, Option<String>), String> {
    let agent = lightair_live::net::agent();
    let mut req = agent.post(url).header("Content-Type", "application/sdp");
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut res = req.send(offer).map_err(|e| match e {
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => {
            "the server refused the token".to_string()
        }
        e => e.to_string(),
    })?;
    match res.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err("the server refused the token".into()),
        s => return Err(format!("the server answered {s}")),
    }
    let location = res
        .headers()
        .get("location")
        .and_then(|l| l.to_str().ok())
        .map(|l| resolve_location(url, l));
    let answer = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok((answer, location))
}

/// A `Location` header against the endpoint's URL (it may be a path).
fn resolve_location(url: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let origin_end = url
        .find("://")
        .and_then(|i| url[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(url.len());
    if location.starts_with('/') {
        format!("{}{location}", &url[..origin_end])
    } else {
        let base = url.rsplit_once('/').map_or(url, |(b, _)| b);
        format!("{base}/{location}")
    }
}

fn delete_session(location: &str, token: Option<&str>) {
    let agent = lightair_live::net::agent();
    let mut req = agent.delete(location);
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let _ = req.call();
}

async fn session(
    url: &str,
    token: Option<&str>,
    rx: &mut tokio::sync::mpsc::Receiver<Media>,
    c: &Arc<Counters>,
    backoff: &mut lightair_feed::backoff::Backoff,
) -> Result<(), String> {
    let e = |e: rtc::shared::error::Error| e.to_string();
    let (video_codec, audio_codec) = codecs();
    let mut media = MediaEngine::default();
    media
        .register_codec(video_codec.clone(), RtpCodecKind::Video)
        .map_err(e)?;
    media
        .register_codec(audio_codec.clone(), RtpCodecKind::Audio)
        .map_err(e)?;
    let registry = register_default_interceptors(Registry::new(), &mut media).map_err(e)?;
    let (gathered_tx, mut gathered) = tokio::sync::mpsc::channel(1);
    let (state_tx, mut state) = tokio::sync::watch::channel(RTCPeerConnectionState::New);
    let runtime = default_runtime().ok_or("no runtime")?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            gathered: gathered_tx,
            state: state_tx,
        }))
        .with_runtime(runtime)
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .map_err(e)?;
    let (video_ssrc, audio_ssrc) = (rand_ssrc(1), rand_ssrc(2));
    let video = Arc::new(track(
        RtpCodecKind::Video,
        &video_codec.rtp_codec,
        video_ssrc,
    )?);
    let audio = Arc::new(track(
        RtpCodecKind::Audio,
        &audio_codec.rtp_codec,
        audio_ssrc,
    )?);
    let video_sender = pc
        .add_track(Arc::clone(&video) as Arc<dyn TrackLocal>)
        .await
        .map_err(e)?;
    let audio_sender = pc
        .add_track(Arc::clone(&audio) as Arc<dyn TrackLocal>)
        .await
        .map_err(e)?;
    let offer = pc.create_offer(None).await.map_err(e)?;
    pc.set_local_description(offer).await.map_err(e)?;
    // the candidates in the offer (WHIP sends one offer)
    let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
    let offer = pc.local_description().await.ok_or("no offer was made")?.sdp;
    let (url_owned, token_owned) = (url.to_string(), token.map(String::from));
    let (answer, location) =
        tokio::task::spawn_blocking(move || post_offer(&url_owned, token_owned.as_deref(), &offer))
            .await
            .map_err(|e| e.to_string())??;
    pc.set_remote_description(RTCSessionDescription::answer(answer).map_err(e)?)
        .await
        .map_err(e)?;
    // connected within 10 s, else try again
    let connected = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match *state.borrow_and_update() {
                RTCPeerConnectionState::Connected => return true,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => return false,
                _ => {}
            }
            if state.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false);
    if !connected {
        let _ = pc.close().await;
        if let Some(l) = location {
            let t = token.map(String::from);
            let _ = tokio::task::spawn_blocking(move || delete_session(&l, t.as_deref())).await;
        }
        return Err("the WebRTC connection didn't come up".into());
    }
    let payload = |sender: &Arc<dyn webrtc::rtp_transceiver::RtpSender>| {
        let sender = sender.clone();
        async move {
            sender
                .get_parameters()
                .await
                .ok()
                .and_then(|p| p.rtp_parameters.codecs.first().map(|c| c.payload_type))
        }
    };
    let video_pt = payload(&video_sender).await.unwrap_or(102);
    let audio_pt = payload(&audio_sender).await.unwrap_or(111);
    *c.error.lock() = None;
    *c.retry_at.lock() = None;
    backoff.connected(Instant::now());
    c.connected.store(true, Ordering::Relaxed);
    c.keyframe.store(true, Ordering::Relaxed);
    let (mut last_video, mut last_audio) = (None::<u64>, None::<u64>);
    let result = loop {
        tokio::select! {
            m = rx.recv() => {
                let Some(m) = m else { break Ok(()) };
                taken(c, &m);
                let (track, ssrc, pt, data, pts, last, default) = match m {
                    Media::Video(d, pts) => (&video, video_ssrc, video_pt, d, pts, &mut last_video, 33_333),
                    Media::Audio(d, pts) => (&audio, audio_ssrc, audio_pt, d, pts, &mut last_audio, 20_000),
                };
                let us = last.map_or(default, |l| pts.saturating_sub(l).clamp(1_000, 200_000));
                *last = Some(pts);
                let n = data.len() as u64;
                let sample = Sample {
                    data,
                    duration: Duration::from_micros(us),
                    ..Sample::new(Instant::now())
                };
                if let Err(err) = track.sample_writer(ssrc, pt).write_sample(&sample).await {
                    break Err(err.to_string());
                }
                c.sent_bytes.fetch_add(n, Ordering::Relaxed);
            }
            changed = state.changed() => {
                let s = *state.borrow();
                if changed.is_err() || matches!(s, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed) {
                    break Err("the WebRTC connection was lost".into());
                }
            }
        }
        if c.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
    };
    c.connected.store(false, Ordering::Relaxed);
    let _ = pc.close().await;
    if let Some(l) = location {
        let t = token.map(String::from);
        let _ = tokio::task::spawn_blocking(move || delete_session(&l, t.as_deref())).await;
    }
    result
}

/// An SSRC that differs between sessions and tracks.
fn rand_ssrc(salt: u32) -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    t.wrapping_mul(2_654_435_761)
        .wrapping_add(salt.wrapping_mul(40_503))
        | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_are_read_against_the_endpoint() {
        let url = "https://whip.example.com/rtc/v1/whip/?app=live&stream=me";
        assert_eq!(
            resolve_location(url, "/rtc/v1/whip/session/1"),
            "https://whip.example.com/rtc/v1/whip/session/1"
        );
        assert_eq!(
            resolve_location("https://h:8889/live/me/whip", "abc"),
            "https://h:8889/live/me/abc"
        );
        assert_eq!(resolve_location(url, "https://other/x"), "https://other/x");
    }

    /// A WHIP server in the test: one HTTP exchange on 127.0.0.1, answered by a webrtc-rs
    /// peer that counts the RTP packets of each kind it receives.
    struct Receiver {
        packets: Arc<[AtomicU64; 2]>,
        connected: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Receiver {
        async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
            if state == RTCPeerConnectionState::Connected {
                self.connected.store(true, Ordering::Relaxed);
            }
        }
        async fn on_track(&self, track: Arc<dyn webrtc::media_stream::track_remote::TrackRemote>) {
            let i = (track.kind().await == RtpCodecKind::Audio) as usize;
            let packets = self.packets.clone();
            tokio::spawn(async move {
                while let Some(e) = track.poll().await {
                    if let webrtc::media_stream::track_remote::TrackRemoteEvent::OnRtpPacket(_) = e
                    {
                        packets[i].fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    }

    fn serve_whip(packets: Arc<[AtomicU64; 2]>, connected: Arc<AtomicBool>) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut pcs = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut line, mut length, mut auth) = (String::new(), 0, String::new());
                reader.read_line(&mut line).unwrap();
                let method = line.split(' ').next().unwrap_or("").to_string();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    let lower = h.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = h.trim().to_string();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if method != "POST" {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    continue;
                }
                if !auth.eq_ignore_ascii_case("Authorization: Bearer secret") {
                    let _ =
                        stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
                    continue;
                }
                let offer = String::from_utf8(body).unwrap();
                let handler = Arc::new(Receiver {
                    packets: packets.clone(),
                    connected: connected.clone(),
                });
                let (pc, answer) = rt.block_on(async move {
                    let (video, audio) = codecs();
                    let mut media = MediaEngine::default();
                    media.register_codec(video, RtpCodecKind::Video).unwrap();
                    media.register_codec(audio, RtpCodecKind::Audio).unwrap();
                    let registry =
                        register_default_interceptors(Registry::new(), &mut media).unwrap();
                    let pc = PeerConnectionBuilder::new()
                        .with_configuration(RTCConfigurationBuilder::new().build())
                        .with_media_engine(media)
                        .with_interceptor_registry(registry)
                        .with_handler(handler)
                        .with_runtime(default_runtime().unwrap())
                        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
                        .build()
                        .await
                        .unwrap();
                    pc.set_remote_description(RTCSessionDescription::offer(offer).unwrap())
                        .await
                        .unwrap();
                    let answer = pc.create_answer(None).await.unwrap();
                    pc.set_local_description(answer).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let sdp = pc.local_description().await.unwrap().sdp;
                    (pc, sdp)
                });
                pcs.push(pc);
                let reply = format!(
                    "HTTP/1.1 201 Created\r\nContent-Type: application/sdp\r\nLocation: /whip/session/1\r\nContent-Length: {}\r\n\r\n{answer}",
                    answer.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        format!("http://127.0.0.1:{port}/whip/live")
    }

    #[test]
    fn a_stream_reaches_a_whip_server() {
        let packets = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
        let connected = Arc::new(AtomicBool::new(false));
        let url = serve_whip(packets.clone(), connected.clone());
        let url_again = url.clone();
        let mut s = WhipSender::start(url, Some("secret".into())).unwrap();
        s.parameter_sets(&[vec![0x67, 0x42, 0xe0, 0x1f], vec![0x68, 0xce, 0x3c, 0x80]]);
        let start = Instant::now();
        while !s.stats().connected {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "no connection: {:?}",
                s.stats().error
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // a second of pictures (a keyframe first) and 20 ms Opus packets
        for i in 0..50u64 {
            let key = i % 25 == 0;
            let nal = if key { 0x65 } else { 0x41 };
            let mut picture = vec![0, 0, 4, 0, nal];
            picture.extend(std::iter::repeat_n(0x55, 1023));
            s.video(&picture, false, i * 20_000, key);
            s.audio(&[0xfc, 0xff, 0xfe], i * 20_000);
            std::thread::sleep(Duration::from_millis(20));
        }
        let start = Instant::now();
        while packets[0].load(Ordering::Relaxed) == 0 || packets[1].load(Ordering::Relaxed) == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "video {} audio {}",
                packets[0].load(Ordering::Relaxed),
                packets[1].load(Ordering::Relaxed)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(connected.load(Ordering::Relaxed));
        assert!(s.stats().sent_bytes > 50_000);
        drop(s);
        // a wrong token is refused, and said so
        let wrong = WhipSender::start(url_again, Some("wrong".into())).unwrap();
        let start = Instant::now();
        while !wrong
            .stats()
            .error
            .is_some_and(|e| e.contains("refused the token"))
        {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{:?}",
                wrong.stats().error
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn pictures_go_out_as_annex_b_from_a_keyframe_with_their_parameter_sets() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let c = Arc::new(Counters::default());
        c.connected.store(true, Ordering::Relaxed);
        let mut s = WhipSender {
            tx,
            counters: c.clone(),
            parameter_sets: vec![vec![0x67, 1], vec![0x68, 2]],
            waiting_key: true,
        };
        // a delta picture first: left out, a keyframe asked for
        s.video(&[0, 0, 0, 2, 0x41, 9], false, 0, false);
        assert!(s.take_keyframe_request());
        s.video(&[0, 0, 0, 2, 0x65, 7], false, 33_333, true);
        let Some(Media::Video(d, pts)) = rx.try_recv().ok() else {
            panic!("the keyframe")
        };
        assert_eq!(pts, 33_333);
        assert_eq!(
            &d[..],
            [
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 7
            ]
        );
        s.video(&[0, 0, 0, 1, 0x41, 3], true, 66_666, false);
        assert!(matches!(rx.try_recv(), Ok(Media::Video(..))));
        s.audio(&[0xfc, 1, 2], 70_000);
        assert!(matches!(rx.try_recv(), Ok(Media::Audio(..))));
    }
}
