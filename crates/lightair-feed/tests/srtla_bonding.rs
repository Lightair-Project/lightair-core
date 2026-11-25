//! SRTLA bonding over real SRT: the SRT sender talks to the SRTLA relay, the relay registers two
//! links with a receiver written here from the protocol (as `srtla_rec` behaves: one SRT
//! connection per group, SRTLA acknowledgements every 10 packets per link, keepalives echoed,
//! the SRT listener's packets sent back on the link heard from last, its ACKs on every link),
//! and an srt-tokio listener gets the stream. Everything arrives in order, over both links, and
//! keeps arriving when one link goes dark.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use lightair_feed::SrtTarget;
use lightair_feed::srtla::{ACK, ID_LEN, KEEPALIVE, REG1, REG2, REG3, Relay};
use parking_lot::Mutex;
use srt_tokio::SrtListener;

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn listen(port: u16, got: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (_server, mut incoming) = SrtListener::builder().bind(port).await.unwrap();
            while let Some(request) = incoming.incoming().next().await {
                let got = got.clone();
                tokio::spawn(async move {
                    let Ok(mut socket) = request.accept(None).await else {
                        return;
                    };
                    while let Some(Ok((_, data))) = socket.next().await {
                        got.lock().extend_from_slice(&data);
                    }
                });
            }
        });
    });
}

/// The receiver: packets per link it counted, and a switch that makes the first link go dark.
struct Receiver {
    addr: SocketAddr,
    per_link: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    dark: Arc<AtomicBool>,
}

fn receiver(srt: SocketAddr) -> Receiver {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = s.local_addr().unwrap();
    let up = UdpSocket::bind("127.0.0.1:0").unwrap();
    up.connect(srt).unwrap();
    up.set_nonblocking(true).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    let per_link: Arc<Mutex<HashMap<SocketAddr, usize>>> = Default::default();
    let dark = Arc::new(AtomicBool::new(false));
    let (counted, off) = (per_link.clone(), dark.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut group: Option<[u8; ID_LEN]> = None;
        let mut links: Vec<SocketAddr> = Vec::new();
        let mut last: Option<SocketAddr> = None;
        let mut unacked: HashMap<SocketAddr, Vec<u32>> = HashMap::new();
        loop {
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                let p = &buf[..n];
                // the first registered link goes dark: nothing in, nothing out
                if off.load(Ordering::Acquire) && links.first() == Some(&from) {
                    continue;
                }
                let kind = u16::from_be_bytes([p[0], p[1]]);
                match kind {
                    REG1 if n == 2 + ID_LEN => {
                        let mut id = [0u8; ID_LEN];
                        id.copy_from_slice(&p[2..]);
                        id[ID_LEN / 2..].fill(0x5A);
                        group = Some(id);
                        let mut r = REG2.to_be_bytes().to_vec();
                        r.extend_from_slice(&id);
                        let _ = s.send_to(&r, from);
                    }
                    REG2 if n == 2 + ID_LEN => {
                        if group.is_some_and(|g| g[..] == p[2..]) {
                            if !links.contains(&from) {
                                links.push(from);
                            }
                            let _ = s.send_to(&REG3.to_be_bytes(), from);
                        }
                    }
                    KEEPALIVE => {
                        let _ = s.send_to(p, from);
                    }
                    _ if links.contains(&from) => {
                        last = Some(from);
                        if p[0] & 0x80 == 0 && n >= 16 {
                            *counted.lock().entry(from).or_default() += 1;
                            let v = unacked.entry(from).or_default();
                            v.push(u32::from_be_bytes([p[0], p[1], p[2], p[3]]));
                            if v.len() == 10 {
                                let mut a = ACK.to_be_bytes().to_vec();
                                a.extend([0, 0]);
                                for sn in v.drain(..) {
                                    a.extend(sn.to_be_bytes());
                                }
                                let _ = s.send_to(&a, from);
                            }
                        }
                        let _ = up.send(p);
                    }
                    _ => {}
                }
            }
            // the listener's packets: its ACKs to every link, the rest to the last one heard
            while let Ok(n) = up.recv(&mut buf) {
                let p = &buf[..n];
                let is_ack = p[0] & 0x80 != 0 && u16::from_be_bytes([p[0], p[1]]) == 0x8002;
                let dark_link = off.load(Ordering::Acquire).then(|| links.first().copied()).flatten();
                for l in &links {
                    if Some(*l) == dark_link {
                        continue;
                    }
                    if is_ack || Some(*l) == last {
                        let _ = s.send_to(p, l);
                    }
                }
            }
        }
    });
    Receiver { addr, per_link, dark }
}

#[test]
fn a_bonded_stream_arrives_whole_over_two_links_and_survives_losing_one() {
    let port = free_port();
    let got: Arc<Mutex<Vec<u8>>> = Default::default();
    listen(port, got.clone());
    let rx = receiver(SocketAddr::from(([127, 0, 0, 1], port)));
    let relay = Relay::start(rx.addr, vec![("Wi-Fi".into(), None), ("Mobile data".into(), None)]).unwrap();
    let mut sender = lightair_feed::SrtSender::start(SrtTarget {
        address: relay.local(),
        source: "studio".into(),
        passphrase: String::new(),
        latency: Duration::from_millis(1000),
        stream_id: None,
        bind: None,
    })
    .unwrap();
    let start = Instant::now();
    while !sender.stats().connected {
        assert!(start.elapsed() < Duration::from_secs(8), "{:?} {:?}", sender.stats(), relay.links());
        std::thread::sleep(Duration::from_millis(20));
    }

    // three seconds of "transport stream" (counting bytes, so order is checked), 200 datagrams a
    // second; the first link goes dark after one
    let mut sent = Vec::new();
    let mut i: u32 = 0;
    let begin = Instant::now();
    while begin.elapsed() < Duration::from_secs(3) {
        if begin.elapsed() > Duration::from_secs(1) {
            rx.dark.store(true, Ordering::Release);
        }
        let mut packet = vec![0x47u8; 188];
        packet[4..8].copy_from_slice(&i.to_be_bytes());
        i += 1;
        assert!(sender.begin_picture(true));
        sender.write(&packet);
        sender.flush();
        sent.extend_from_slice(&packet);
        std::thread::sleep(Duration::from_millis(5));
    }
    let start = Instant::now();
    while got.lock().len() < sent.len() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{} of {} bytes; {:?}; {:?}",
            got.lock().len(),
            sent.len(),
            relay.links(),
            rx.per_link.lock()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(*got.lock() == sent, "in order and whole");
    let per_link = rx.per_link.lock().clone();
    assert_eq!(per_link.len(), 2, "both links carried data: {per_link:?}");
    let links = relay.links();
    assert!(links.iter().all(|l| l.sent_bytes > 0), "{links:?}");
}
