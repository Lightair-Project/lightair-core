//! Bonding: one SRT stream over several networks at once, to an SRTLA receiver the person runs
//! (BELABOX's `srtla_rec`, or a relay of theirs): a relay on 127.0.0.1 the SRT sender talks to,
//! one UDP link per network (bound with [`crate::net::Bind`]), each registered with the receiver
//! (REG1 → REG2 → REG2 per link → REG3), SRT data spread over the links by each one's window of
//! packets in flight (grown by the receiver's SRTLA acknowledgements, shrunk by SRT's loss
//! reports), keepalives every second, and a link that hears nothing for 10 s registered again.
//! What the receiver sends back (SRT's handshakes, ACKs, NAKs) goes to the SRT sender.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub const KEEPALIVE: u16 = 0x9000;
pub const ACK: u16 = 0x9100;
pub const REG1: u16 = 0x9200;
pub const REG2: u16 = 0x9201;
pub const REG3: u16 = 0x9202;
pub const REG_ERR: u16 = 0x9210;
pub const REG_NGP: u16 = 0x9211;
pub const REG_NAK: u16 = 0x9212;
pub const ID_LEN: usize = 256;
const SRT_NAK: u16 = 0x8003;

const WINDOW_START: f32 = 20.0;
const WINDOW_MIN: f32 = 1.0;
const WINDOW_MAX: f32 = 60.0;
/// A packet unacknowledged this long is taken as lost (it no longer counts as in flight).
const IN_FLIGHT_FOR: Duration = Duration::from_secs(2);
/// The receiver drops a connection after 10 s of silence; ours re-registers sooner.
const LINK_SILENT: Duration = Duration::from_secs(5);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(1);
const REGISTER_EVERY: Duration = Duration::from_millis(500);
const NOT_ANSWERED: &str = "the bonding receiver hasn't answered";

fn kind(b: &[u8]) -> Option<u16> {
    (b.len() >= 2).then(|| u16::from_be_bytes([b[0], b[1]]))
}

/// An SRT data packet's sequence number (bit 31 clear: data).
fn srt_seq(b: &[u8]) -> Option<u32> {
    (b.len() >= 16 && b[0] & 0x80 == 0).then(|| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The sequence numbers an SRT NAK reports lost (single ones, and ranges: first with bit 31 set,
/// then last). Ranges are capped to keep a bad packet cheap.
fn nak_losses(b: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut words = b
        .get(16..)
        .unwrap_or(&[])
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| u32::from_be_bytes(*w));
    while let Some(w) = words.next() {
        if w & 0x8000_0000 != 0 {
            let first = w & 0x7FFF_FFFF;
            let last = words.next().unwrap_or(first) & 0x7FFF_FFFF;
            out.extend((first..=last).take(256));
        } else {
            out.push(w);
        }
    }
    out
}

/// One link's state.
pub struct Link {
    pub name: String,
    socket: UdpSocket,
    registered: bool,
    window: f32,
    in_flight: VecDeque<(u32, Instant)>,
    heard_at: Instant,
    keepalive_at: Instant,
    register_at: Option<Instant>,
    sent_bytes: u64,
    /// The bytes counted at the last rate reading, and the rate then.
    rate_at: (Instant, u64),
    kbps: u32,
}

impl Link {
    fn new(
        name: String,
        receiver: SocketAddr,
        bind: Option<&crate::net::Bind>,
    ) -> std::io::Result<Link> {
        let socket = crate::net::udp_for(&receiver, bind)?;
        socket.connect(receiver)?;
        socket.set_nonblocking(true)?;
        let now = Instant::now();
        Ok(Link {
            name,
            socket,
            registered: false,
            window: WINDOW_START,
            in_flight: VecDeque::new(),
            heard_at: now,
            keepalive_at: now,
            register_at: None,
            sent_bytes: 0,
            rate_at: (now, 0),
            kbps: 0,
        })
    }
}

/// How a link is doing (for health).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkStats {
    pub name: String,
    pub registered: bool,
    pub window: f32,
    pub in_flight: usize,
    pub sent_bytes: u64,
    /// What it carried over the last second or so.
    pub sent_kbps: u32,
}

struct Shared {
    links: Vec<Link>,
    /// The group's full id once the receiver gave it (REG2).
    id: Option<[u8; ID_LEN]>,
    /// Our half, sent in REG1.
    ours: [u8; ID_LEN],
    /// The SRT sender's address on 127.0.0.1.
    peer: Option<SocketAddr>,
    error: Option<String>,
}

/// The running relay. Dropping it stops it.
pub struct Relay {
    local: SocketAddr,
    receiver: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Relay {
    /// Starts relaying to the SRTLA receiver at `receiver` over `links` (a name and how to bind
    /// each, e.g. "Wi-Fi" and "Mobile data"; `None` binds nothing). The SRT sender then calls
    /// [`Relay::local`].
    pub fn start(
        receiver: SocketAddr,
        links: Vec<(String, Option<crate::net::Bind>)>,
    ) -> std::io::Result<Relay> {
        let local_socket = UdpSocket::bind("127.0.0.1:0")?;
        local_socket.set_read_timeout(Some(Duration::from_millis(2)))?;
        let local = local_socket.local_addr()?;
        let mut made = Vec::new();
        for (name, bind) in links {
            made.push(Link::new(name, receiver, bind.as_ref())?);
        }
        if made.is_empty() {
            return Err(std::io::Error::other("no network to bond"));
        }
        let mut ours = [0u8; ID_LEN];
        getrandom::fill(&mut ours).map_err(|e| std::io::Error::other(e.to_string()))?;
        let shared = Arc::new(Mutex::new(Shared {
            links: made,
            id: None,
            ours,
            peer: None,
            error: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, quit) = (shared.clone(), stop.clone());
        std::thread::Builder::new()
            .name("lightair-srtla".into())
            .spawn(move || run(local_socket, s, quit))?;
        Ok(Relay {
            local,
            receiver,
            shared,
            stop,
        })
    }

    /// Where the SRT sender sends (127.0.0.1).
    pub fn local(&self) -> SocketAddr {
        self.local
    }

    pub fn links(&self) -> Vec<LinkStats> {
        self.shared
            .lock()
            .links
            .iter()
            .map(|l| LinkStats {
                name: l.name.clone(),
                registered: l.registered,
                window: l.window,
                in_flight: l.in_flight.len(),
                sent_bytes: l.sent_bytes,
                sent_kbps: l.kbps,
            })
            .collect()
    }

    /// The networks changed: links whose names are gone stop, new ones register (the ones that
    /// stay keep going). A link that can't be made is left out and said in [`Self::error`].
    pub fn set_links(&self, links: Vec<(String, Option<crate::net::Bind>)>) {
        let mut s = self.shared.lock();
        s.links.retain(|l| links.iter().any(|(n, _)| *n == l.name));
        for (name, bind) in links {
            if s.links.iter().any(|l| l.name == name) {
                continue;
            }
            match Link::new(name.clone(), self.receiver, bind.as_ref()) {
                Ok(l) => s.links.push(l),
                Err(e) => s.error = Some(format!("{name} can't be used: {e}")),
            }
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.lock().error.clone()
    }
}

fn reg(kind: u16, id: &[u8; ID_LEN]) -> Vec<u8> {
    let mut b = kind.to_be_bytes().to_vec();
    b.extend_from_slice(id);
    b
}

fn run(local: UdpSocket, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        // what the SRT sender sends: spread over the links (a bounded batch, so replies keep up)
        for _ in 0..64 {
            let Ok((n, from)) = local.recv_from(&mut buf) else {
                break;
            };
            let mut s = shared.lock();
            s.peer = Some(from);
            send(&mut s, &buf[..n], now);
        }
        // what each link hears
        let mut s = shared.lock();
        let peer = s.peer;
        let mut to_peer: Vec<Vec<u8>> = Vec::new();
        let ours = s.ours;
        let mut new_id = None;
        let mut forget = false;
        for l in &mut s.links {
            while let Ok(n) = l.socket.recv(&mut buf) {
                let p = &buf[..n];
                l.heard_at = now;
                match kind(p) {
                    Some(REG2) if n == 2 + ID_LEN && !l.registered => {
                        // the group's id (ours, with the receiver's half)
                        let mut id = [0u8; ID_LEN];
                        id.copy_from_slice(&p[2..]);
                        if id[..ID_LEN / 2] == ours[..ID_LEN / 2] {
                            new_id = Some(id);
                        }
                    }
                    Some(REG3) => {
                        l.registered = true;
                        l.window = WINDOW_START;
                    }
                    Some(REG_NGP) => {
                        // the receiver doesn't know the group (it restarted): from REG1 again
                        l.registered = false;
                        forget = true;
                    }
                    Some(REG_ERR) | Some(REG_NAK) => {
                        l.registered = false;
                    }
                    Some(KEEPALIVE) => {}
                    Some(ACK) => {
                        for w in p.get(4..).unwrap_or(&[]).as_chunks::<4>().0 {
                            let sn = u32::from_be_bytes(*w);
                            if let Some(i) = l.in_flight.iter().position(|(s, _)| *s == sn) {
                                l.in_flight.remove(i);
                                if l.window < WINDOW_MAX {
                                    l.window += 1.0 / l.window.max(1.0) * 4.0;
                                }
                            }
                        }
                    }
                    _ => to_peer.push(p.to_vec()),
                }
            }
        }
        if forget {
            s.id = None;
        }
        if let Some(id) = new_id
            && s.id != Some(id)
        {
            s.id = Some(id);
            for l in &mut s.links {
                l.registered = false;
                l.register_at = None;
            }
        }
        // SRT's loss reports shrink the windows of the links that carried the losses
        for p in &to_peer {
            if kind(p) == Some(SRT_NAK) {
                for sn in nak_losses(p) {
                    for l in &mut s.links {
                        if let Some(i) = l.in_flight.iter().position(|(s, _)| *s == sn) {
                            l.in_flight.remove(i);
                            l.window = (l.window * 0.7).max(WINDOW_MIN);
                        }
                    }
                }
            }
        }
        // registering, keepalives, silent links, old packets
        let id = s.id;
        let mut any_registered = false;
        for l in &mut s.links {
            if now.duration_since(l.heard_at) > LINK_SILENT && l.registered {
                l.registered = false;
                l.register_at = None;
            }
            while l
                .in_flight
                .front()
                .is_some_and(|(_, t)| now.duration_since(*t) > IN_FLIGHT_FOR)
            {
                l.in_flight.pop_front();
            }
            if !l.registered
                && l.register_at
                    .is_none_or(|t| now.duration_since(t) >= REGISTER_EVERY)
            {
                l.register_at = Some(now);
                let _ = match id {
                    Some(id) => l.socket.send(&reg(REG2, &id)),
                    None => l.socket.send(&reg(REG1, &ours)),
                };
            }
            let since = now.duration_since(l.rate_at.0);
            if since >= Duration::from_secs(1) {
                l.kbps =
                    ((l.sent_bytes - l.rate_at.1) * 8 / since.as_millis().max(1) as u64) as u32;
                l.rate_at = (now, l.sent_bytes);
            }
            if now.duration_since(l.keepalive_at) >= KEEPALIVE_EVERY {
                l.keepalive_at = now;
                let _ = l.socket.send(&KEEPALIVE.to_be_bytes());
            }
            any_registered |= l.registered;
        }
        if !any_registered {
            s.error = Some(NOT_ANSWERED.into());
        } else if s.error.as_deref() == Some(NOT_ANSWERED) {
            s.error = None;
        }
        drop(s);
        if let Some(peer) = peer {
            for p in to_peer {
                let _ = local.send_to(&p, peer);
            }
        }
    }
}

/// An SRT packet onto the best link: the most room in its window. Control packets go on every
/// registered link (handshakes and ACKs must get through).
fn send(s: &mut Shared, p: &[u8], now: Instant) {
    let seq = srt_seq(p);
    if seq.is_none() {
        for l in s.links.iter_mut().filter(|l| l.registered) {
            if l.socket.send(p).is_ok() {
                l.sent_bytes += p.len() as u64;
            }
        }
        return;
    }
    let best = s.links.iter_mut().filter(|l| l.registered).max_by(|a, b| {
        let room = |l: &Link| l.window - l.in_flight.len() as f32;
        room(a).total_cmp(&room(b))
    });
    if let Some(l) = best
        && l.socket.send(p).is_ok()
    {
        l.sent_bytes += p.len() as u64;
        if let Some(sn) = seq {
            l.in_flight.push_back((sn, now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receiver in the test: registers links, echoes keepalives, acknowledges every 10 data
    /// packets per link, counts them, and answers the first data packet with an SRT control
    /// packet (to check it reaches the sender).
    type Seen = Arc<Mutex<Vec<(SocketAddr, u32)>>>;

    fn receiver() -> (SocketAddr, Seen) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = s.local_addr().unwrap();
        let got: Seen = Default::default();
        let seen = got.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut group: Option<[u8; ID_LEN]> = None;
            let mut per_link: std::collections::HashMap<SocketAddr, Vec<u32>> = Default::default();
            let mut answered = false;
            while let Ok((n, from)) = s.recv_from(&mut buf) {
                let p = &buf[..n];
                match kind(p) {
                    Some(REG1) => {
                        let mut id = [0u8; ID_LEN];
                        id.copy_from_slice(&p[2..]);
                        for b in &mut id[ID_LEN / 2..] {
                            *b = 7;
                        }
                        group = Some(id);
                        let _ = s.send_to(&reg(REG2, &id), from);
                    }
                    Some(REG2) if group.is_some_and(|g| g[..] == p[2..]) => {
                        let _ = s.send_to(&REG3.to_be_bytes(), from);
                    }
                    Some(KEEPALIVE) => {
                        let _ = s.send_to(p, from);
                    }
                    _ => {
                        if let Some(sn) = srt_seq(p) {
                            seen.lock().push((from, sn));
                            let v = per_link.entry(from).or_default();
                            v.push(sn);
                            if v.len() == 10 {
                                let mut ack = ACK.to_be_bytes().to_vec();
                                ack.extend([0, 0]);
                                for sn in v.drain(..) {
                                    ack.extend(sn.to_be_bytes());
                                }
                                let _ = s.send_to(&ack, from);
                            }
                            if !answered {
                                answered = true;
                                // an SRT ACK (control) back
                                let mut c = 0x8002u16.to_be_bytes().to_vec();
                                c.extend([0u8; 14]);
                                let _ = s.send_to(&c, from);
                            }
                        }
                    }
                }
            }
        });
        (addr, got)
    }

    #[test]
    fn two_links_register_share_the_packets_and_bring_replies_back() {
        let (addr, got) = receiver();
        let relay = Relay::start(
            addr,
            vec![("Wi-Fi".into(), None), ("Mobile data".into(), None)],
        )
        .unwrap();
        let start = Instant::now();
        while !relay.links().iter().all(|l| l.registered) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{:?}",
                relay.links()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // the "SRT sender": data packets to the relay
        let srt = UdpSocket::bind("127.0.0.1:0").unwrap();
        srt.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        for sn in 0..200u32 {
            let mut p = sn.to_be_bytes().to_vec();
            p.extend([0u8; 60]);
            srt.send_to(&p, relay.local()).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        // the receiver's SRT control packet comes back to the sender
        let mut buf = [0u8; 64];
        let (n, _) = srt.recv_from(&mut buf).unwrap();
        assert_eq!(kind(&buf[..n]), Some(0x8002));
        let start = Instant::now();
        while got.lock().len() < 200 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{}",
                got.lock().len()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let links: std::collections::HashSet<SocketAddr> =
            got.lock().iter().map(|(a, _)| *a).collect();
        assert_eq!(links.len(), 2, "both links carried data");
        assert!(relay.links().iter().all(|l| l.sent_bytes > 0));
    }

    #[test]
    fn links_come_and_go_with_the_networks() {
        let (addr, _) = receiver();
        let relay = Relay::start(addr, vec![("Wi-Fi".into(), None)]).unwrap();
        let all_up = |n: usize| {
            let start = Instant::now();
            loop {
                let l = relay.links();
                if l.len() == n && l.iter().all(|l| l.registered) {
                    return l;
                }
                assert!(start.elapsed() < Duration::from_secs(5), "{l:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        all_up(1);
        relay.set_links(vec![("Wi-Fi".into(), None), ("Mobile data".into(), None)]);
        all_up(2);
        relay.set_links(vec![("Mobile data".into(), None)]);
        let l = all_up(1);
        assert_eq!(l[0].name, "Mobile data");
        assert_eq!(relay.error(), None);
    }

    #[test]
    fn losses_are_read_from_nak_ranges() {
        let mut p = SRT_NAK.to_be_bytes().to_vec();
        p.extend([0u8; 14]);
        p.extend(5u32.to_be_bytes());
        p.extend((0x8000_0000u32 | 10).to_be_bytes());
        p.extend(12u32.to_be_bytes());
        assert_eq!(nak_losses(&p), [5, 10, 11, 12]);
    }
}
