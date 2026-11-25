//! An MPEG transport stream muxer for one camera: H.264 or HEVC video and
//! AAC-LC audio, 188-byte packets, PAT and PMT before every keyframe, PCR on the video.
//!
//! Encoders hand over access units either in Annex B (start codes) or length-prefixed (AVCC /
//! HVCC, 4-byte lengths, as VideoToolbox gives them); parameter sets given once with
//! [`Muxer::parameter_sets`] are put in front of every keyframe that lacks them, so a receiver
//! joining late can start at the next keyframe. Audio is raw AAC access units; the muxer adds
//! the ADTS header.

/// The video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// How an encoder delimits NAL units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalFormat {
    /// Start codes (MediaCodec).
    AnnexB,
    /// 4-byte big-endian lengths (VideoToolbox).
    LengthPrefixed,
}

/// AAC-LC as the phone's microphone gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u8,
}

const PAT_PID: u16 = 0;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x100;
const AUDIO_PID: u16 = 0x101;
/// PTS and DTS run this far ahead of the PCR, so a decoder has the frame before it's due.
const DELAY_90K: u64 = 9_000; // 100 ms
pub const PACKET: usize = 188;

pub struct Muxer {
    video: VideoCodec,
    audio: Option<AudioConfig>,
    parameter_sets: Vec<Vec<u8>>,
    cc: [u8; 4],
    /// When the tables were last sent, on the 90 kHz clock.
    tables_at: Option<u64>,
}

fn us_to_90k(us: u64) -> u64 {
    us * 9 / 100
}

impl Muxer {
    pub fn new(video: VideoCodec, audio: Option<AudioConfig>) -> Muxer {
        Muxer {
            video,
            audio,
            parameter_sets: vec![],
            cc: [0; 4],
            tables_at: None,
        }
    }

    /// SPS and PPS (and VPS for HEVC), each without a start code or length.
    pub fn parameter_sets(&mut self, nals: Vec<Vec<u8>>) {
        self.parameter_sets = nals;
    }

    /// One video access unit; returns whole TS packets. `dts_us` defaults to `pts_us` (no
    /// B-frames, as the phones encode for latency).
    pub fn video(
        &mut self,
        data: &[u8],
        format: NalFormat,
        pts_us: u64,
        dts_us: Option<u64>,
        keyframe: bool,
    ) -> Vec<u8> {
        let nals = split_nals(data, format);
        let pts = us_to_90k(pts_us);
        let dts = dts_us.map_or(pts, us_to_90k);
        let mut es = Vec::with_capacity(data.len() + 64);
        // an access unit delimiter first, as TS decoders expect
        match self.video {
            VideoCodec::H264 => es.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]),
            VideoCodec::Hevc => es.extend_from_slice(&[0, 0, 0, 1, 0x46, 0x01, 0x50]),
        }
        let has_sets = nals.iter().any(|n| self.is_parameter_set(n));
        if keyframe && !has_sets {
            for p in &self.parameter_sets {
                es.extend_from_slice(&[0, 0, 0, 1]);
                es.extend_from_slice(p);
            }
        }
        for n in nals.iter().filter(|n| !self.is_delimiter(n)) {
            es.extend_from_slice(&[0, 0, 0, 1]);
            es.extend_from_slice(n);
        }
        let mut out = vec![];
        // the tables before every keyframe, and at least every 100 ms
        if keyframe
            || self
                .tables_at
                .is_none_or(|t| dts.saturating_sub(t) >= 9_000)
        {
            self.tables_at = Some(dts);
            out.extend(self.pat());
            out.extend(self.pmt());
        }
        let pes = pes(0xE0, &es, pts + DELAY_90K, Some(dts + DELAY_90K));
        out.extend(self.packets(VIDEO_PID, &pes, Some(dts * 300), keyframe));
        out
    }

    /// One raw AAC access unit.
    pub fn audio(&mut self, data: &[u8], pts_us: u64) -> Vec<u8> {
        let Some(cfg) = self.audio else { return vec![] };
        let mut es = adts_header(cfg, data.len());
        es.extend_from_slice(data);
        let pes = pes(0xC0, &es, us_to_90k(pts_us) + DELAY_90K, None);
        self.packets(AUDIO_PID, &pes, None, false)
    }

    fn is_parameter_set(&self, nal: &[u8]) -> bool {
        let Some(&h) = nal.first() else { return false };
        match self.video {
            VideoCodec::H264 => matches!(h & 0x1F, 7 | 8),
            VideoCodec::Hevc => matches!((h >> 1) & 0x3F, 32..=34),
        }
    }

    fn is_delimiter(&self, nal: &[u8]) -> bool {
        let Some(&h) = nal.first() else { return true };
        match self.video {
            VideoCodec::H264 => h & 0x1F == 9,
            VideoCodec::Hevc => (h >> 1) & 0x3F == 35,
        }
    }

    fn pat(&mut self) -> Vec<u8> {
        // program 1 → the PMT
        let mut s = vec![0x00, 0xB0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01];
        s.push(0xE0 | (PMT_PID >> 8) as u8);
        s.push(PMT_PID as u8);
        self.section(PAT_PID, s)
    }

    fn pmt(&mut self) -> Vec<u8> {
        let mut s = vec![0x02, 0xB0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00];
        s.push(0xE0 | (VIDEO_PID >> 8) as u8); // PCR on the video
        s.push(VIDEO_PID as u8);
        s.extend_from_slice(&[0xF0, 0x00]); // no program descriptors
        let video_type = match self.video {
            VideoCodec::H264 => 0x1B,
            VideoCodec::Hevc => 0x24,
        };
        let mut streams = vec![(video_type, VIDEO_PID)];
        if self.audio.is_some() {
            streams.push((0x0F, AUDIO_PID)); // AAC in ADTS
        }
        for (t, pid) in streams {
            s.extend_from_slice(&[t, 0xE0 | (pid >> 8) as u8, pid as u8, 0xF0, 0x00]);
        }
        self.section(PMT_PID, s)
    }

    /// A PSI section (its length filled in, CRC appended) in one packet.
    fn section(&mut self, pid: u16, mut s: Vec<u8>) -> Vec<u8> {
        let len = s.len() - 3 + 4;
        s[1] = 0xB0 | ((len >> 8) as u8 & 0x0F);
        s[2] = len as u8;
        let crc = crc32(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        let mut p = vec![
            0x47,
            0x40 | (pid >> 8) as u8,
            pid as u8,
            0x10 | self.next_cc(pid),
            0x00,
        ];
        p.extend_from_slice(&s);
        p.resize(PACKET, 0xFF);
        p
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let i = match pid {
            PAT_PID => 0,
            PMT_PID => 1,
            VIDEO_PID => 2,
            _ => 3,
        };
        let c = self.cc[i];
        self.cc[i] = (c + 1) & 0x0F;
        c
    }

    /// A PES split into packets: the first carries the start flag (and the PCR, and the random
    /// access flag on keyframes); the last is padded with adaptation-field stuffing.
    fn packets(&mut self, pid: u16, pes: &[u8], pcr: Option<u64>, random_access: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity((pes.len() / 184 + 2) * PACKET);
        let mut rest = pes;
        let mut first = true;
        while !rest.is_empty() {
            let mut af: Vec<u8> = vec![];
            if first && (pcr.is_some() || random_access) {
                let mut flags = 0u8;
                if random_access {
                    flags |= 0x40;
                }
                af.push(0); // length, below
                if let Some(pcr) = pcr {
                    flags |= 0x10;
                    af.push(flags);
                    let base = pcr / 300;
                    let ext = pcr % 300;
                    af.extend_from_slice(&[
                        (base >> 25) as u8,
                        (base >> 17) as u8,
                        (base >> 9) as u8,
                        (base >> 1) as u8,
                        ((base & 1) << 7) as u8 | 0x7E | ((ext >> 8) as u8 & 1),
                        ext as u8,
                    ]);
                } else {
                    af.push(flags);
                }
            }
            let room = 184 - af.len();
            let take = rest.len().min(room);
            // stuffing: grow (or start) the adaptation field so the packet is exactly full
            if take < room {
                let pad = room - take;
                if af.is_empty() {
                    if pad == 1 {
                        af.push(0);
                    } else {
                        af.push(0);
                        af.push(0);
                        af.extend(std::iter::repeat_n(0xFF, pad - 2));
                    }
                } else {
                    af.extend(std::iter::repeat_n(0xFF, pad));
                }
            }
            if !af.is_empty() {
                af[0] = (af.len() - 1) as u8;
            }
            let control = if af.is_empty() { 0x10 } else { 0x30 };
            out.push(0x47);
            out.push(if first { 0x40 } else { 0 } | (pid >> 8) as u8);
            out.push(pid as u8);
            out.push(control | self.next_cc(pid));
            out.extend_from_slice(&af);
            out.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            first = false;
        }
        out
    }
}

/// A PES packet with its length, so the desktop knows a picture is complete as soon as its last
/// byte arrives instead of when the next one starts (about a frame sooner). A picture over 64 KiB
/// (a big keyframe) keeps the length 0, unbounded, which MPEG-TS allows for video only.
fn pes(stream_id: u8, es: &[u8], pts: u64, dts: Option<u64>) -> Vec<u8> {
    let header_len = if dts.is_some() { 10 } else { 5 };
    let mut p = vec![
        0,
        0,
        1,
        stream_id,
        0,
        0,
        0x80,
        if dts.is_some() { 0xC0 } else { 0x80 },
        header_len,
    ];
    let ts = |marker: u8, t: u64| {
        let t = t & 0x1_FFFF_FFFF;
        [
            (marker << 4) | (((t >> 30) as u8 & 0x07) << 1) | 1,
            (t >> 22) as u8,
            (((t >> 15) as u8) << 1) | 1,
            (t >> 7) as u8,
            ((t as u8) << 1) | 1,
        ]
    };
    p.extend_from_slice(&ts(if dts.is_some() { 3 } else { 2 }, pts));
    if let Some(d) = dts {
        p.extend_from_slice(&ts(1, d));
    }
    p.extend_from_slice(es);
    let len = p.len() - 6;
    if len <= 0xFFFF {
        p[4] = (len >> 8) as u8;
        p[5] = len as u8;
    }
    p
}

fn adts_header(cfg: AudioConfig, payload: usize) -> Vec<u8> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let index = RATES
        .iter()
        .position(|&r| r == cfg.sample_rate)
        .unwrap_or(4) as u8; // 44.1 kHz otherwise
    let len = payload + 7;
    let ch = cfg.channels & 0x07;
    vec![
        0xFF,
        0xF1,                                // MPEG-4, no CRC
        (1 << 6) | (index << 2) | (ch >> 2), // profile LC (object type 2 − 1)
        ((ch & 3) << 6) | ((len >> 11) as u8 & 0x03),
        (len >> 3) as u8,
        (((len & 7) as u8) << 5) | 0x1F,
        0xFC,
    ]
}

/// The NAL units in an access unit, without start codes or lengths.
pub fn split_nals(data: &[u8], format: NalFormat) -> Vec<&[u8]> {
    match format {
        NalFormat::LengthPrefixed => {
            let mut out = vec![];
            let mut rest = data;
            while rest.len() >= 4 {
                let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                if n == 0 || 4 + n > rest.len() {
                    break;
                }
                out.push(&rest[4..4 + n]);
                rest = &rest[4 + n..];
            }
            out
        }
        NalFormat::AnnexB => {
            let mut starts = vec![];
            let mut i = 0;
            while i + 3 <= data.len() {
                if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                    starts.push(i + 3);
                    i += 3;
                } else {
                    i += 1;
                }
            }
            starts
                .iter()
                .enumerate()
                .map(|(k, &s)| {
                    let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
                    // a 4-byte start code's leading zero belongs to it, not the NAL before
                    while end > s && data[end - 1] == 0 {
                        end -= 1;
                    }
                    &data[s..end]
                })
                .filter(|n| !n.is_empty())
                .collect()
        }
    }
}

/// CRC-32/MPEG-2.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_standard() {
        // CRC-32/MPEG-2 of "123456789"
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn splits_both_nal_formats() {
        let annex = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        assert_eq!(
            split_nals(&annex, NalFormat::AnnexB),
            vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]
        );
        let avcc = [0, 0, 0, 2, 0x65, 9, 0, 0, 0, 1, 0x06];
        assert_eq!(
            split_nals(&avcc, NalFormat::LengthPrefixed),
            vec![&[0x65, 9][..], &[0x06]]
        );
    }

    #[test]
    fn packets_are_whole_and_counted() {
        let mut m = Muxer::new(
            VideoCodec::H264,
            Some(AudioConfig {
                sample_rate: 48000,
                channels: 2,
            }),
        );
        m.parameter_sets(vec![vec![0x67, 1, 2, 3], vec![0x68, 4]]);
        // a small keyframe, then a 1000-byte frame that spans several packets
        let nal = |len: usize| -> Vec<u8> {
            (len as u32)
                .to_be_bytes()
                .into_iter()
                .chain([0x65])
                .chain(std::iter::repeat_n(7, len - 1))
                .collect()
        };
        for (i, data) in [nal(5), nal(1000)].iter().enumerate() {
            let out = m.video(
                data,
                NalFormat::LengthPrefixed,
                1_000_000 + i as u64 * 33_333,
                None,
                i == 0,
            );
            assert_eq!(out.len() % PACKET, 0);
            assert!(out.chunks(PACKET).all(|p| p[0] == 0x47));
        }
        let a = m.audio(&[1, 2, 3, 4], 1_000_000);
        assert_eq!(a.len(), PACKET);
        // a keyframe starts with the tables, and carries the parameter sets
        let out = m.video(
            &[0, 0, 0, 1, 0x65],
            NalFormat::LengthPrefixed,
            2_000_000,
            None,
            true,
        );
        let pids: Vec<u16> = out
            .chunks(PACKET)
            .map(|p| (((p[1] & 0x1F) as u16) << 8) | p[2] as u16)
            .collect();
        assert_eq!(&pids[..3], &[PAT_PID, PMT_PID, VIDEO_PID]);
        let video = &out[2 * PACKET..];
        assert!(video.windows(5).any(|w| w == [0, 0, 1, 0x67, 1]));
        // the random-access flag on the keyframe's first packet
        assert_eq!(video[3] & 0x30, 0x30);
        assert_ne!(video[5] & 0x40, 0);
    }

    #[test]
    fn video_pes_carries_its_length_when_it_fits() {
        let small = pes(0xE0, &[0u8; 1000], 9000, Some(9000));
        let len = u16::from_be_bytes([small[4], small[5]]) as usize;
        assert_eq!(len, small.len() - 6);
        let big = pes(0xE0, &vec![0u8; 70_000], 9000, Some(9000));
        assert_eq!((big[4], big[5]), (0, 0));
    }
}
