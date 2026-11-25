//! Matroska, written cluster by cluster in a segment of unknown size.
//!
//! While recording the file is: the EBML header, the segment (size unknown), space kept for
//! the seek head, `Info` (with a duration of 0), `Tracks`, then one cluster per keyframe
//! interval, each written whole. A reader plays it up to the last whole cluster. Finishing
//! appends the cues and chapters, writes the seek head into the space kept for it, and fills in
//! the duration and the segment's size.

use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use super::{Marker, RecorderOptions, Sample, TrackSpec, Writer, uid};
use crate::Error;
use crate::format::{AudioCodec, TrackFormat, VideoCodec};

// element ids
const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const CUES: u32 = 0x1C53_BB6B;
const CHAPTERS: u32 = 0x1043_A770;
const VOID: u32 = 0xEC;

/// Timestamps in milliseconds.
const TIMESTAMP_SCALE: u64 = 1_000_000;
/// The space kept for the seek head.
const SEEK_HEAD_SPACE: usize = 128;
/// How long a block may wait for the other tracks, to keep the clusters in time order.
const INTERLEAVE_NS: i64 = 1_000_000_000;

fn vint_len(v: u64) -> usize {
    (1..=8).find(|&n| v < (1u64 << (7 * n)) - 1).unwrap_or(8)
}

fn put_id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().position(|&b| b != 0).unwrap_or(3);
    out.extend_from_slice(&bytes[skip..]);
}

fn put_size(out: &mut Vec<u8>, size: u64) {
    let n = vint_len(size);
    let v = size | (1u64 << (7 * n));
    out.extend_from_slice(&v.to_be_bytes()[8 - n..]);
}

/// An 8-byte size (fixed width, for sizes filled in later).
fn size8(size: u64) -> [u8; 8] {
    (size | (1u64 << 56)).to_be_bytes()
}

fn element(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    put_id(out, id);
    put_size(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn master(out: &mut Vec<u8>, id: u32, f: impl FnOnce(&mut Vec<u8>)) {
    let mut body = Vec::new();
    f(&mut body);
    element(out, id, &body);
}

fn uint(out: &mut Vec<u8>, id: u32, v: u64) {
    let n = (1..=8).find(|&n| n == 8 || v < (1u64 << (8 * n))).unwrap_or(8);
    element(out, id, &v.to_be_bytes()[8 - n..]);
}

fn float(out: &mut Vec<u8>, id: u32, v: f64) {
    element(out, id, &v.to_be_bytes());
}

fn string(out: &mut Vec<u8>, id: u32, v: &str) {
    element(out, id, v.as_bytes());
}

/// A Void element exactly `n` bytes long (`n` ≥ 2).
fn void(out: &mut Vec<u8>, n: usize) {
    put_id(out, VOID);
    if n - 2 < 127 {
        put_size(out, (n - 2) as u64);
        out.resize(out.len() + n - 2, 0);
    } else {
        out.extend_from_slice(&size8((n - 9) as u64));
        out.resize(out.len() + n - 9, 0);
    }
}

/// A block waiting for its cluster, ordered by decode time.
struct Waiting {
    dts_ms: i64,
    seq: u64,
    sample: Sample,
}

impl PartialEq for Waiting {
    fn eq(&self, o: &Self) -> bool {
        (self.dts_ms, self.seq) == (o.dts_ms, o.seq)
    }
}
impl Eq for Waiting {}
impl PartialOrd for Waiting {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Waiting {
    // the heap pops the smallest first
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (o.dts_ms, o.seq).cmp(&(self.dts_ms, self.seq))
    }
}

pub(crate) struct MkvWriter {
    file: File,
    pos: u64,
    options: RecorderOptions,
    /// Where the segment's size, its data, the seek head's space and the duration are.
    segment_size_at: u64,
    segment_data_at: u64,
    seek_head_at: u64,
    duration_at: u64,
    info_at: u64,
    tracks_at: u64,
    waiting: BinaryHeap<Waiting>,
    seq: u64,
    /// Per track: the newest decode time handed in.
    newest: Vec<Option<i64>>,
    cluster: Vec<u8>,
    cluster_ms: Option<i64>,
    cluster_at_keyframe: bool,
    /// Keyframes that start clusters: (time ms, cluster position in the segment).
    cues: Vec<(i64, u64)>,
    primary: usize,
}

impl MkvWriter {
    pub fn create(path: &Path, tracks: &[TrackSpec], options: &RecorderOptions) -> Result<MkvWriter, Error> {
        let mut head = Vec::new();
        master(&mut head, EBML, |o| {
            uint(o, 0x4286, 1); // EBMLVersion
            uint(o, 0x42F7, 1); // EBMLReadVersion
            uint(o, 0x42F2, 4); // EBMLMaxIDLength
            uint(o, 0x42F3, 8); // EBMLMaxSizeLength
            string(o, 0x4282, "matroska"); // DocType
            uint(o, 0x4287, 4); // DocTypeVersion
            uint(o, 0x4285, 2); // DocTypeReadVersion
        });
        put_id(&mut head, SEGMENT);
        let segment_size_at = head.len() as u64;
        // unknown size until finished
        head.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let segment_data_at = head.len() as u64;
        let seek_head_at = head.len() as u64;
        void(&mut head, SEEK_HEAD_SPACE);
        let info_at = head.len() as u64;
        let duration_at = {
            let mut info = Vec::new();
            uint(&mut info, 0x2AD7B1, TIMESTAMP_SCALE);
            string(&mut info, 0x4D80, &options.writing_app); // MuxingApp
            string(&mut info, 0x5741, &options.writing_app); // WritingApp
            if let Some(title) = &options.title {
                string(&mut info, 0x7BA9, title);
            }
            let mut segment_uid = uid().to_be_bytes().to_vec();
            segment_uid.extend_from_slice(&uid().wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
            element(&mut info, 0x73A4, &segment_uid); // SegmentUUID
            let before = info.len();
            float(&mut info, 0x4489, 0.0); // Duration, filled in when finished
            let duration_in_info = before + 3; // id (2) + size (1)
            put_id(&mut head, INFO);
            put_size(&mut head, info.len() as u64);
            let at = head.len() as u64 + duration_in_info as u64;
            head.extend_from_slice(&info);
            at
        };
        let tracks_at = head.len() as u64;
        master(&mut head, TRACKS, |o| {
            for (i, t) in tracks.iter().enumerate() {
                master(o, 0xAE, |o| track_entry(o, i, t));
            }
        });
        let mut file = File::create(path)?;
        file.write_all(&head)?;
        if options.sync_fragments {
            file.sync_data()?;
        }
        let primary = tracks.iter().position(|t| matches!(t.format, TrackFormat::Video(_))).unwrap_or(0);
        Ok(MkvWriter {
            file,
            pos: head.len() as u64,
            options: options.clone(),
            segment_size_at,
            segment_data_at,
            seek_head_at,
            duration_at,
            info_at,
            tracks_at,
            waiting: BinaryHeap::new(),
            seq: 0,
            newest: vec![None; tracks.len()],
            cluster: Vec::new(),
            cluster_ms: None,
            cluster_at_keyframe: false,
            cues: Vec::new(),
            primary,
        })
    }

    /// Hands blocks on in decode order once every track has caught up with them (or they
    /// waited long enough).
    fn release(&mut self, all: bool) -> Result<(), Error> {
        let caught_up = self.newest.iter().flatten().copied().min();
        let latest = self.newest.iter().flatten().copied().max().unwrap_or(0);
        while let Some(w) = self.waiting.peek() {
            let due = all
                || caught_up.is_some_and(|c| w.dts_ms * 1_000_000 <= c)
                || latest - w.dts_ms * 1_000_000 >= INTERLEAVE_NS;
            if !due {
                break;
            }
            let w = self.waiting.pop().expect("peeked");
            self.block(w.sample)?;
        }
        Ok(())
    }

    fn block(&mut self, s: Sample) -> Result<(), Error> {
        let ms = s.pts_ns.div_euclid(1_000_000);
        let interval = self.options.fragment_ms.max(100) as i64;
        let starts_cluster = match self.cluster_ms {
            None => true,
            Some(c) => {
                let rel = ms - c;
                !(-32_768..=32_767).contains(&rel)
                    || (s.track == self.primary && s.keyframe && rel >= interval)
                    || self.cluster.len() > 64 << 20
            }
        };
        if starts_cluster {
            self.flush_cluster()?;
            self.cluster_ms = Some(ms);
            self.cluster_at_keyframe = s.track == self.primary && s.keyframe;
            uint(&mut self.cluster, 0xE7, ms.max(0) as u64);
        }
        let rel = (ms - self.cluster_ms.unwrap_or(ms)) as i16;
        let mut head = Vec::with_capacity(4);
        put_size(&mut head, s.track as u64 + 1);
        head.extend_from_slice(&rel.to_be_bytes());
        head.push(if s.keyframe { 0x80 } else { 0 });
        put_id(&mut self.cluster, 0xA3);
        put_size(&mut self.cluster, (head.len() + s.data.len()) as u64);
        self.cluster.extend_from_slice(&head);
        self.cluster.extend_from_slice(&s.data);
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<(), Error> {
        if self.cluster.is_empty() {
            return Ok(());
        }
        let at = self.pos;
        if self.cluster_at_keyframe {
            self.cues.push((self.cluster_ms.unwrap_or(0), at - self.segment_data_at));
        }
        let mut out = Vec::with_capacity(self.cluster.len() + 12);
        element(&mut out, CLUSTER, &self.cluster);
        self.cluster.clear();
        self.put(&out)?;
        if self.options.sync_fragments {
            self.file.sync_data()?;
        }
        Ok(())
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<(), Error> {
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(bytes)?;
        Ok(())
    }
}

fn track_entry(o: &mut Vec<u8>, i: usize, t: &TrackSpec) {
    uint(o, 0xD7, i as u64 + 1); // TrackNumber
    uint(o, 0x73C5, uid()); // TrackUID
    uint(o, 0x9C, 0); // FlagLacing
    string(o, 0x22B59C, &t.language); // Language
    if let Some(name) = &t.name {
        string(o, 0x536E, name);
    }
    match &t.format {
        TrackFormat::Video(v) => {
            uint(o, 0x83, 1);
            let (codec, private): (&str, Vec<u8>) = match v.codec {
                VideoCodec::H264 => ("V_MPEG4/ISO/AVC", v.config.clone()),
                VideoCodec::Hevc { .. } => ("V_MPEGH/ISO/HEVC", v.config.clone()),
                VideoCodec::ProRes(p) => ("V_PRORES", p.fourcc().to_vec()),
            };
            string(o, 0x86, codec);
            element(o, 0x63A2, &private);
            uint(o, 0x23E383, v.frame_ns().max(1) as u64); // DefaultDuration
            master(o, 0xE0, |o| {
                uint(o, 0xB0, v.width as u64);
                uint(o, 0xBA, v.height as u64);
                master(o, 0x55B0, |o| {
                    uint(o, 0x55B1, v.color.matrix as u64);
                    uint(o, 0x55B9, if v.color.full_range { 2 } else { 1 });
                    uint(o, 0x55BA, v.color.transfer as u64);
                    uint(o, 0x55BB, v.color.primaries as u64);
                });
            });
        }
        TrackFormat::Audio(a) => {
            uint(o, 0x83, 2);
            string(
                o,
                0x86,
                match a.codec {
                    AudioCodec::Aac => "A_AAC",
                    AudioCodec::Opus => "A_OPUS",
                },
            );
            element(o, 0x63A2, &a.config);
            // the encoder's priming: subtracted from every block's time
            uint(o, 0x56AA, a.priming_ns().max(0) as u64);
            if a.codec == AudioCodec::Opus {
                uint(o, 0x56BB, 80_000_000); // SeekPreRoll
            }
            master(o, 0xE1, |o| {
                float(o, 0xB5, a.sample_rate as f64);
                uint(o, 0x9F, a.channels as u64);
            });
        }
    }
}

impl Writer for MkvWriter {
    fn write(&mut self, s: Sample) -> Result<(), Error> {
        let track = s.track;
        let dts = s.dts_ns;
        self.newest[track] = Some(self.newest[track].map_or(dts, |n| n.max(dts)));
        self.seq += 1;
        self.waiting.push(Waiting { dts_ms: dts.div_euclid(1_000_000), seq: self.seq, sample: s });
        self.release(false)
    }

    fn finish(&mut self, markers: &[Marker], duration_ns: i64) -> Result<u64, Error> {
        self.release(true)?;
        self.flush_cluster()?;
        let cues_at = self.pos;
        let mut tail = Vec::new();
        if !self.cues.is_empty() {
            let primary = self.primary as u64 + 1;
            master(&mut tail, CUES, |o| {
                for &(ms, at) in &self.cues {
                    master(o, 0xBB, |o| {
                        uint(o, 0xB3, ms.max(0) as u64);
                        master(o, 0xB7, |o| {
                            uint(o, 0xF7, primary);
                            uint(o, 0xF1, at);
                        });
                    });
                }
            });
        }
        let chapters_at = self.pos + tail.len() as u64;
        if !markers.is_empty() {
            master(&mut tail, CHAPTERS, |o| {
                master(o, 0x45B9, |o| {
                    uint(o, 0x45BC, uid()); // EditionUID
                    for (i, m) in markers.iter().enumerate() {
                        master(o, 0xB6, |o| {
                            uint(o, 0x73C4, uid().wrapping_add(i as u64).max(1));
                            uint(o, 0x91, m.at_ns.max(0) as u64);
                            let end = markers.get(i + 1).map_or(duration_ns, |n| n.at_ns);
                            uint(o, 0x92, end.max(m.at_ns).max(0) as u64);
                            master(o, 0x80, |o| {
                                string(o, 0x85, &m.title);
                                string(o, 0x437C, "und");
                            });
                        });
                    }
                });
            });
        }
        self.put(&tail)?;
        // the seek head, in the space kept for it
        let data_at = self.segment_data_at;
        let mut seeks: Vec<(u32, u64)> = vec![(INFO, self.info_at), (TRACKS, self.tracks_at)];
        if !self.cues.is_empty() {
            seeks.push((CUES, cues_at));
        }
        if !markers.is_empty() {
            seeks.push((CHAPTERS, chapters_at));
        }
        let mut head = Vec::new();
        master(&mut head, SEEK_HEAD, |o| {
            for (id, at) in seeks {
                master(o, SEEK, |o| {
                    let mut idb = Vec::new();
                    put_id(&mut idb, id);
                    element(o, SEEK_ID, &idb);
                    uint(o, SEEK_POSITION, at - data_at);
                });
            }
        });
        if head.len() + 2 <= SEEK_HEAD_SPACE {
            let rest = SEEK_HEAD_SPACE - head.len();
            void(&mut head, rest);
            let at = self.seek_head_at;
            self.patch(at, &head)?;
        }
        let duration_ms = duration_ns as f64 / TIMESTAMP_SCALE as f64;
        let at = self.duration_at;
        self.patch(at, &duration_ms.to_be_bytes())?;
        let size = self.pos - self.segment_data_at;
        let at = self.segment_size_at;
        self.patch(at, &size8(size))?;
        self.file.seek(SeekFrom::Start(self.pos))?;
        self.file.sync_all()?;
        Ok(self.pos)
    }
}
