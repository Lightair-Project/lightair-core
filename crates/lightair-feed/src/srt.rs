//! The phone's SRT caller: it calls the desktop's listener for its source, with
//! `streamid` `#!::r=<source>,m=publish`, the session's passphrase (AES-128) and a latency of
//! 40 ms on a local network. Transport stream packets go out seven to a datagram (1316 bytes).
//!
//! It runs on its own thread with a small tokio runtime, reconnects by itself (every second)
//! while the feed lives, and never queues more than a fraction of a second: when the network
//! can't keep up, whole access units are dropped (a picture or a sound goes in the queue whole or
//! not at all, never a keyframe's first half) and a keyframe is asked for, so the picture
//! recovers instead of falling behind.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::SinkExt;
use parking_lot::Mutex;
use srt_tokio::SrtSocket;
use tokio::sync::mpsc;

use crate::ts::PACKET;

/// Seven transport stream packets: SRT's usual payload.
pub const DATAGRAM: usize = 7 * PACKET;
/// How many datagrams may wait: about 300 ms at 25 Mb/s.
const QUEUE: usize = 700;

/// Where the desktop listens for this source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtTarget {
    pub address: SocketAddr,
    /// The desktop layer: `#!::r=<source>,m=publish`.
    pub source: String,
    /// From the control channel's `media` message; empty: no encryption.
    pub passphrase: String,
    pub latency: Duration,
    /// A custom destination's stream id, sent as it is (`None`: the desktop's form, above).
    pub stream_id: Option<String>,
    /// The network to send on (`None`: the system's choice).
    pub bind: Option<crate::net::Bind>,
}

impl SrtTarget {
    /// The stream id given whole (a custom SRT destination's), else the desktop's form for
    /// `source`.
    pub fn stream_id(&self) -> String {
        match &self.stream_id {
            Some(id) => id.clone(),
            None => format!("#!::r={},m=publish", self.source),
        }
    }
}

/// An SRT address's parts: host:port, stream id, passphrase, latency.
pub fn parse_url(url: &str) -> Result<SrtTarget, String> {
    use std::net::ToSocketAddrs;
    let rest = url.strip_prefix("srt://").ok_or("The address has to start with srt://.")?;
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = authority.trim_end_matches('/');
    let address = authority
        .to_socket_addrs()
        .map_err(|e| format!("{authority} can't be found: {e}"))?
        .next()
        .ok_or_else(|| format!("{authority} can't be found"))?;
    let mut target = SrtTarget {
        address,
        source: String::new(),
        passphrase: String::new(),
        latency: Duration::from_millis(120),
        stream_id: Some(String::new()),
        bind: None,
    };
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = percent_decode(v);
        match k {
            "streamid" => target.stream_id = Some(v),
            "passphrase" => target.passphrase = v,
            "latency" => {
                // ms (some tools write µs: anything over 10 s is taken as µs)
                if let Ok(n) = v.parse::<u64>() {
                    target.latency = Duration::from_millis(if n > 10_000 { n / 1000 } else { n });
                }
            }
            _ => {}
        }
    }
    Ok(target)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Default)]
struct Counters {
    connected: AtomicBool,
    sent_bytes: AtomicU64,
    dropped: AtomicU64,
    keyframe: AtomicBool,
    /// Connections made so far.
    connections: AtomicU64,
    /// When the next attempt is, while away.
    retry_at: Mutex<Option<Instant>>,
    error: Mutex<Option<String>>,
    stop: AtomicBool,
    /// How long each picture's last datagram waited before the socket took it.
    send_times: Mutex<Vec<Duration>>,
}

/// What the sender has done.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderStats {
    pub connected: bool,
    pub sent_bytes: u64,
    /// Datagrams dropped because the network couldn't keep up or the desktop was away.
    pub dropped: u64,
    pub error: Option<String>,
    /// While away: how long until the next attempt (the reconnect backoff).
    pub retry_in: Option<Duration>,
}

/// Sends transport stream packets to the desktop. Dropping it closes the connection.
pub struct SrtSender {
    /// Datagrams, the last of a picture with when it was queued.
    tx: mpsc::Sender<(Bytes, Option<Instant>)>,
    counters: Arc<Counters>,
    pending: Vec<u8>,
    /// The datagrams of the access unit being written: queued together by [`Self::flush`] or
    /// [`Self::end_frame`], or dropped together.
    unit: Vec<Bytes>,
    /// The connection [`Self::begin_picture`] last saw.
    seen: u64,
    /// Pictures are left out until a keyframe (see [`Self::begin_picture`]).
    waiting_key: bool,
}

impl SrtSender {
    pub fn start(target: SrtTarget) -> std::io::Result<SrtSender> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let counters = Arc::new(Counters::default());
        let c = counters.clone();
        std::thread::Builder::new().name("lightair-srt".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    *c.error.lock() = Some(e.to_string());
                    return;
                }
            };
            rt.block_on(run(target, rx, c));
        })?;
        Ok(SrtSender {
            tx,
            counters,
            pending: Vec::with_capacity(DATAGRAM),
            unit: Vec::new(),
            seen: 0,
            waiting_key: true,
        })
    }

    /// Before a picture's packets: whether to send it. Each connection, and the stream after
    /// anything was dropped, starts at a keyframe (whose parameter sets the muxer puts in
    /// front), so a receiver never gets pictures it can't decode; until one comes, pictures
    /// are left out and a keyframe is asked for.
    pub fn begin_picture(&mut self, keyframe: bool) -> bool {
        let c = &self.counters;
        let now = c.connections.load(Ordering::Acquire);
        if now != self.seen {
            self.seen = now;
            self.waiting_key = true;
        }
        if !c.connected.load(Ordering::Relaxed) {
            self.waiting_key = true;
            return false;
        }
        if keyframe {
            self.waiting_key = false;
        } else if self.waiting_key {
            c.keyframe.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Whether pictures are being left out until a keyframe (sound waits with them).
    pub fn waiting_for_keyframe(&self) -> bool {
        self.waiting_key
    }

    /// Whole transport stream packets, in datagrams of seven, of the access unit being
    /// written: sent once it ends ([`Self::flush`], [`Self::end_frame`]).
    pub fn write(&mut self, packets: &[u8]) {
        for p in packets.chunks(PACKET) {
            self.pending.extend_from_slice(p);
            if self.pending.len() == DATAGRAM {
                self.seal();
            }
        }
    }

    /// The end of an access unit (a sound): it's sent whole, the last datagram even if fewer
    /// than seven packets, so it doesn't wait for the next one.
    pub fn flush(&mut self) {
        self.seal();
        self.push(false);
    }

    /// [`Self::flush`] at the end of a picture, timing how long its last datagram waits.
    pub fn end_frame(&mut self) {
        self.seal();
        self.push(true);
    }

    /// The packets waiting, as a datagram of the unit.
    fn seal(&mut self) {
        if !self.pending.is_empty() {
            let datagram = std::mem::replace(&mut self.pending, Vec::with_capacity(DATAGRAM));
            self.unit.push(Bytes::from(datagram));
        }
    }

    /// How long pictures waited to be sent since the last call.
    pub fn take_send_times(&self) -> Vec<Duration> {
        std::mem::take(&mut *self.counters.send_times.lock())
    }

    /// The unit written into the queue, whole, or dropped whole: a queue with room for only
    /// part of it would send a picture's start without its end (a keyframe no one can decode,
    /// and the pictures after it neither).
    fn push(&mut self, frame_end: bool) {
        let unit = std::mem::take(&mut self.unit);
        let n = unit.len();
        if n == 0 {
            return;
        }
        // the only sender: the room seen here only grows until the sends below
        if !self.counters.connected.load(Ordering::Relaxed) || self.tx.capacity() < n {
            // away, or full: drop, and start again from a keyframe
            self.waiting_key = true;
            self.counters.dropped.fetch_add(n as u64, Ordering::Relaxed);
            self.counters.keyframe.store(true, Ordering::Relaxed);
            return;
        }
        let queued = Instant::now();
        for (i, datagram) in unit.into_iter().enumerate() {
            let last = frame_end && i + 1 == n;
            if self.tx.try_send((datagram, last.then_some(queued))).is_err() {
                // the connection's thread is gone
                self.waiting_key = true;
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Datagrams waiting to go out.
    pub fn queued(&self) -> usize {
        QUEUE - self.tx.capacity()
    }

    pub fn stats(&self) -> SenderStats {
        let c = &self.counters;
        SenderStats {
            connected: c.connected.load(Ordering::Relaxed),
            sent_bytes: c.sent_bytes.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            error: c.error.lock().clone(),
            retry_in: c.retry_at.lock().map(|t| t.saturating_duration_since(Instant::now())),
        }
    }

    /// Whether the encoder should make a keyframe now (connected again, or data was dropped).
    pub fn take_keyframe_request(&self) -> bool {
        self.counters.keyframe.swap(false, Ordering::Relaxed)
    }
}

impl Drop for SrtSender {
    fn drop(&mut self) {
        self.counters.stop.store(true, Ordering::Relaxed);
    }
}

async fn run(target: SrtTarget, mut rx: mpsc::Receiver<(Bytes, Option<Instant>)>, c: Arc<Counters>) {
    let stream_id = target.stream_id();
    let mut backoff = crate::backoff::Backoff::default();
    let wait = |backoff: &mut crate::backoff::Backoff| {
        let d = backoff.next(Instant::now());
        *c.retry_at.lock() = Some(Instant::now() + d);
        d
    };
    while !c.stop.load(Ordering::Relaxed) {
        let mut builder = SrtSocket::builder().latency(target.latency);
        if let Some(b) = &target.bind {
            match crate::net::udp_for(&target.address, Some(b)).and_then(tokio::net::UdpSocket::from_std) {
                Ok(s) => builder = builder.socket(s),
                Err(e) => {
                    *c.error.lock() = Some(format!("that network isn't there: {e}"));
                    tokio::time::sleep(wait(&mut backoff)).await;
                    continue;
                }
            }
        }
        if !target.passphrase.is_empty() {
            builder = builder.encryption(16, target.passphrase.clone());
        }
        let connecting = tokio::time::timeout(
            Duration::from_secs(3),
            builder.call(target.address, (!stream_id.is_empty()).then_some(stream_id.as_str())),
        )
        .await;
        let mut socket = match connecting {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                *c.error.lock() = Some(e.to_string());
                tokio::time::sleep(wait(&mut backoff)).await;
                continue;
            }
            Err(_) => {
                *c.error.lock() = Some(format!("{} didn't answer", target.address));
                tokio::time::sleep(wait(&mut backoff)).await;
                continue;
            }
        };
        // anything queued while away is stale
        while rx.try_recv().is_ok() {}
        *c.error.lock() = None;
        *c.retry_at.lock() = None;
        backoff.connected(Instant::now());
        c.connections.fetch_add(1, Ordering::Release);
        c.connected.store(true, Ordering::Relaxed);
        c.keyframe.store(true, Ordering::Relaxed);
        tracing::debug!(address = %target.address, "SRT connected");
        loop {
            if c.stop.load(Ordering::Relaxed) {
                let _ = socket.close().await;
                c.connected.store(false, Ordering::Relaxed);
                return;
            }
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some((datagram, frame_end))) => {
                    let n = datagram.len() as u64;
                    if let Err(e) = socket.send((Instant::now(), datagram)).await {
                        *c.error.lock() = Some(e.to_string());
                        break;
                    }
                    c.sent_bytes.fetch_add(n, Ordering::Relaxed);
                    if let Some(queued) = frame_end {
                        let mut times = c.send_times.lock();
                        if times.len() < 1024 {
                            times.push(queued.elapsed());
                        }
                    }
                }
                Ok(None) => {
                    let _ = socket.close().await;
                    c.connected.store(false, Ordering::Relaxed);
                    return;
                }
                Err(_) => {}
            }
        }
        c.connected.store(false, Ordering::Relaxed);
        tokio::time::sleep(wait(&mut backoff)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sender whose queue nothing reads (a network that doesn't keep up), connected.
    fn stalled() -> (SrtSender, mpsc::Receiver<(Bytes, Option<Instant>)>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let counters = Arc::new(Counters::default());
        counters.connected.store(true, Ordering::Relaxed);
        let sender = SrtSender {
            tx,
            counters,
            pending: Vec::with_capacity(DATAGRAM),
            unit: Vec::new(),
            seen: 0,
            waiting_key: true,
        };
        (sender, rx)
    }

    /// One picture of `datagrams` datagrams, each packet carrying the picture's number.
    fn picture(s: &mut SrtSender, n: u8, keyframe: bool, datagrams: usize) -> bool {
        if !s.begin_picture(keyframe) {
            return false;
        }
        let mut p = [0u8; PACKET];
        p[0] = 0x47;
        p[1] = n;
        s.write(&p.repeat(datagrams * 7));
        s.end_frame();
        true
    }

    /// A full queue drops whole pictures, then waits for a keyframe: never a keyframe's start
    /// without its end (it decodes as garbage, and so does every picture after it until the
    /// next keyframe). The failure it guards against: the first 200 datagrams of the third
    /// picture queued, the rest dropped, and the stream going on from there.
    #[test]
    fn a_full_queue_drops_whole_pictures() {
        let (mut s, mut rx) = stalled();
        for n in 0..6u8 {
            picture(&mut s, n, n % 3 == 0, 250);
        }
        let mut got = std::collections::BTreeMap::<u8, usize>::new();
        while let Ok((d, _)) = rx.try_recv() {
            for p in d.chunks(PACKET) {
                *got.entry(p[1]).or_default() += 1;
            }
        }
        // 0 and 1 fit (500 of 700); 2 doesn't, and 3 (a keyframe) neither; 4 and 5 wait for
        // a keyframe
        assert_eq!(got, [(0, 1750), (1, 1750)].into(), "{got:?}");
        assert!(s.waiting_for_keyframe());
        assert!(s.take_keyframe_request());
        assert_eq!(s.stats().dropped, 500);
        // the queue drained: the next keyframe goes whole
        assert!(picture(&mut s, 6, true, 250));
        let mut n = 0;
        while let Ok((d, at)) = rx.try_recv() {
            assert!(d.chunks(PACKET).all(|p| p[1] == 6));
            n += 1;
            assert_eq!(at.is_some(), n == 250, "the picture's last datagram is timed");
        }
        assert_eq!(n, 250);
        // a sound is whole too: a short last datagram, sent at once
        s.write(&[0x47; PACKET * 3]);
        s.flush();
        assert_eq!(rx.try_recv().unwrap().0.len(), PACKET * 3);
    }
}
