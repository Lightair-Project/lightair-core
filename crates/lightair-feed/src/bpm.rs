//! Broadcast performance metrics: the three SEI messages a multitrack stream to Amazon IVS
//! (Twitch's Enhanced Broadcasting) carries in front of every IDR on every track. Each is an
//! H.264 or HEVC "user data unregistered" SEI with its own UUID: a timestamp (TS), the session's
//! frame counters (SM) and the rendition's (ERM), the counters counted since the track's last
//! IDR, the time in RFC 3339 (UTC, milliseconds).

use std::time::{SystemTime, UNIX_EPOCH};

/// The timestamp message's UUID.
const UUID_TS: [u8; 16] = uuid(0x0aec_ffe7_5272_4e2f_a62f_d19c_d61a_93b5);
/// The session metrics message's UUID.
const UUID_SM: [u8; 16] = uuid(0xca60_e71c_6a8b_4388_a377_151d_f7bf_8ac2);
/// The encoded rendition metrics message's UUID.
const UUID_ERM: [u8; 16] = uuid(0xf1fb_c1d5_101e_4fb5_a61e_b8ce_3c07_b8c0);

const fn uuid(n: u128) -> [u8; 16] {
    n.to_be_bytes()
}

/// The timestamp is of the packet being handed to the connection (the only event the
/// service reads for now).
const EVENT_PACKET_INTERLEAVED: u8 = 4;
/// A timestamp written as an RFC 3339 string.
const TIMESTAMP_RFC3339: u8 = 1;

/// The whole sending side's frames since the track's last IDR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCounts {
    /// Frames the compositor drew.
    pub rendered: u32,
    /// Frames the compositor was late with.
    pub lagged: u32,
    /// Frames the network dropped.
    pub dropped: u32,
    /// Frames every rendition's encoder made, together.
    pub output: u32,
}

/// One rendition's frames since its last IDR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenditionCounts {
    /// Frames given to its encoder.
    pub input: u32,
    /// Frames its encoder didn't take.
    pub skipped: u32,
    /// Frames it encoded.
    pub output: u32,
}

/// The three messages as NAL units, each with a 4-byte length in front (to put before the
/// IDR's own NAL units in its access unit). `hevc`: HEVC's prefix SEI, else H.264's SEI.
pub fn sei_nals(hevc: bool, at: SystemTime, session: SessionCounts, rendition: RenditionCounts) -> Vec<u8> {
    let time = rfc3339(at);
    let mut stamp = vec![0x00, TIMESTAMP_RFC3339, EVENT_PACKET_INTERLEAVED];
    stamp.extend(time.as_bytes());
    stamp.push(0);
    let counters = |list: &[(u8, u32)]| {
        let mut out = vec![(list.len() as u8 - 1) & 0x0F];
        for (tag, value) in list {
            out.push(*tag);
            out.extend(value.to_be_bytes());
        }
        out
    };
    let ts = [&UUID_TS[..], &stamp].concat();
    let sm = [
        &UUID_SM[..],
        &stamp,
        &counters(&[(1, session.rendered), (2, session.lagged), (3, session.dropped), (4, session.output)]),
    ]
    .concat();
    let erm =
        [&UUID_ERM[..], &stamp, &counters(&[(1, rendition.input), (2, rendition.skipped), (3, rendition.output)])]
            .concat();
    let mut out = Vec::new();
    for payload in [ts, sm, erm] {
        let nal = sei_nal(hevc, &payload);
        out.extend((nal.len() as u32).to_be_bytes());
        out.extend(nal);
    }
    out
}

/// One SEI NAL unit holding one "user data unregistered" message (payload type 5).
fn sei_nal(hevc: bool, payload: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(payload.len() + 8);
    let coded = |mut n: usize, out: &mut Vec<u8>| {
        while n >= 255 {
            out.push(0xFF);
            n -= 255;
        }
        out.push(n as u8);
    };
    coded(5, &mut rbsp);
    coded(payload.len(), &mut rbsp);
    rbsp.extend(payload);
    // the RBSP's stop bit
    rbsp.push(0x80);
    let mut nal = if hevc { vec![39 << 1, 1] } else { vec![0x06] };
    // emulation prevention: no 00 00 0x (x ≤ 3) inside the NAL unit
    let mut zeros = 0;
    for b in rbsp {
        if zeros >= 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        nal.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    nal
}

/// `at` as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn rfc3339(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (days, day_secs) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        day_secs / 3600,
        day_secs / 60 % 60,
        day_secs % 60,
        since.subsec_millis()
    )
}

/// The calendar date `days` after 1970-01-01 (proleptic Gregorian).
fn civil(days: i64) -> (i64, u32, u32) {
    // counted from 0000-03-01, in 400-year eras of 146 097 days, so leap days fall at a year's end
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
