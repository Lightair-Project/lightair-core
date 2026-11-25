//! MP4 and MOV, written as fragments while recording and finished as a progressive file.
//!
//! The file while recording: `ftyp`, `moov` (the tracks, no samples, `mvex`), then a `moof` and
//! an `mdat` per fragment. A reader of fragmented files plays it up to the last whole
//! fragment. Finishing appends the chapter samples and a complete `moov` (sample tables
//! pointing into the fragments' `mdat`s), then renames the first `moov` and every `moof` to
//! `free`: what remains is an ordinary MP4 or MOV.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use super::bytes::{Put, bx, fbx, packed_language, ticks, unity_matrix};
use super::{Marker, RecorderOptions, Sample, TrackSpec, Writer};
use crate::Error;
use crate::audio_config;
use crate::format::{AudioCodec, AudioFormat, Color, TrackFormat, VideoCodec, VideoFormat};

const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_TIMESCALE: u32 = 90_000;
/// Sample flags: a sync sample, and one that depends on others.
const SYNC: u32 = 0x0200_0000;
const NON_SYNC: u32 = 0x0101_0000;

struct Pending {
    data: Vec<u8>,
    /// Decode time and composition offset, in the track's timescale.
    d: i64,
    c: i32,
    sync: bool,
}

struct Entry {
    size: u32,
    duration: u32,
    c: i32,
    sync: bool,
}

struct Track {
    spec: TrackSpec,
    id: u32,
    timescale: u32,
    /// A sample's nominal length, for the last one.
    nominal: i64,
    /// Sound: frames the decoder skips at the start.
    priming: i64,
    is_video: bool,
    /// The first packet's (presentation, decode) times.
    first: Option<(i64, i64)>,
    last_d: Option<i64>,
    pending: Option<Pending>,
    // the fragment being gathered
    frag: Vec<Entry>,
    frag_data: Vec<u8>,
    frag_base: Option<i64>,
    // the index the finished file gets
    stts: Vec<(u32, u32)>,
    ctts: Vec<(u32, i32)>,
    sizes: Vec<u32>,
    sync_samples: Vec<u32>,
    chunks: Vec<(u64, u32)>,
    all_sync: bool,
    any_offset: bool,
    first_d: Option<i64>,
    end_d: i64,
}

impl Track {
    fn new(spec: &TrackSpec, id: u32) -> Track {
        let (timescale, nominal, priming, is_video) = match &spec.format {
            TrackFormat::Video(v) => (VIDEO_TIMESCALE, ticks(v.frame_ns(), VIDEO_TIMESCALE).max(1), 0, true),
            TrackFormat::Audio(a) => {
                (a.sample_rate.max(1), a.frames_per_packet() as i64, a.priming_frames as i64, false)
            }
        };
        Track {
            spec: spec.clone(),
            id,
            timescale,
            nominal,
            priming,
            is_video,
            first: None,
            last_d: None,
            pending: None,
            frag: Vec::new(),
            frag_data: Vec::new(),
            frag_base: None,
            stts: Vec::new(),
            ctts: Vec::new(),
            sizes: Vec::new(),
            sync_samples: Vec::new(),
            chunks: Vec::new(),
            all_sync: true,
            any_offset: false,
            first_d: None,
            end_d: 0,
        }
    }

    /// The pending sample, now `duration` long, into the fragment and the index.
    fn commit(&mut self, duration: i64) {
        let Some(p) = self.pending.take() else { return };
        let duration = duration.clamp(1, u32::MAX as i64) as u32;
        let size = p.data.len() as u32;
        self.frag_base.get_or_insert(p.d);
        self.frag.push(Entry { size, duration, c: p.c, sync: p.sync });
        self.frag_data.extend_from_slice(&p.data);
        match self.stts.last_mut() {
            Some((n, d)) if *d == duration => *n += 1,
            _ => self.stts.push((1, duration)),
        }
        match self.ctts.last_mut() {
            Some((n, c)) if *c == p.c => *n += 1,
            _ => self.ctts.push((1, p.c)),
        }
        self.sizes.push(size);
        if p.sync {
            self.sync_samples.push(self.sizes.len() as u32);
        }
        self.all_sync &= p.sync;
        self.any_offset |= p.c != 0;
        self.first_d.get_or_insert(p.d);
        self.end_d = p.d + duration as i64;
    }
}

pub(crate) struct Mp4Writer {
    file: File,
    pos: u64,
    mov: bool,
    options: RecorderOptions,
    tracks: Vec<Track>,
    init_moov_at: u64,
    moofs: Vec<u64>,
    sequence: u32,
    /// The time the fragment being gathered started at.
    frag_start_ns: Option<i64>,
    /// The track whose keyframes start fragments: the first video track, else the first.
    primary: usize,
}

impl Mp4Writer {
    pub fn create(path: &Path, mov: bool, specs: &[TrackSpec], options: &RecorderOptions) -> Result<Mp4Writer, Error> {
        let tracks: Vec<Track> = specs.iter().enumerate().map(|(i, s)| Track::new(s, i as u32 + 1)).collect();
        let primary = tracks.iter().position(|t| t.is_video).unwrap_or(0);
        let mut w = Mp4Writer {
            file: File::create(path)?,
            pos: 0,
            mov,
            options: options.clone(),
            tracks,
            init_moov_at: 0,
            moofs: Vec::new(),
            sequence: 0,
            frag_start_ns: None,
            primary,
        };
        let mut head = Vec::new();
        w.ftyp(&mut head);
        let moov_at = head.len() as u64;
        w.moov(&mut head, true, None);
        w.put(&head)?;
        w.init_moov_at = moov_at;
        if w.options.sync_fragments {
            w.file.sync_data()?;
        }
        Ok(w)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn ftyp(&self, out: &mut Vec<u8>) {
        bx(out, b"ftyp", |o| {
            if self.mov {
                o.bytes(b"qt  ");
                o.u32(0x2005_0300);
                o.bytes(b"qt  ");
            } else {
                o.bytes(b"isom");
                o.u32(0x200);
                o.bytes(b"isom");
                o.bytes(b"iso2");
                o.bytes(b"iso6");
                o.bytes(b"mp41");
                let h264 = self
                    .tracks
                    .iter()
                    .any(|t| matches!(&t.spec.format, TrackFormat::Video(v) if v.codec == VideoCodec::H264));
                if h264 {
                    o.bytes(b"avc1");
                }
            }
        });
    }

    /// Gathers the fragment so far into a `moof` and an `mdat`.
    fn flush_fragment(&mut self) -> Result<(), Error> {
        if self.tracks.iter().all(|t| t.frag.is_empty()) {
            return Ok(());
        }
        self.sequence += 1;
        let total: u64 = self.tracks.iter().map(|t| t.frag_data.len() as u64).sum();
        let mdat_header: u64 = if total + 8 > u32::MAX as u64 { 16 } else { 8 };
        let mut fields: Vec<(usize, usize)> = Vec::new();
        let mut moof = Vec::new();
        let sequence = self.sequence;
        let tracks = &self.tracks;
        bx(&mut moof, b"moof", |o| {
            fbx(o, b"mfhd", 0, 0, |o| o.u32(sequence));
            for (i, t) in tracks.iter().enumerate() {
                if t.frag.is_empty() {
                    continue;
                }
                bx(o, b"traf", |o| {
                    // default-base-is-moof
                    fbx(o, b"tfhd", 0, 0x02_0000, |o| o.u32(t.id));
                    fbx(o, b"tfdt", 1, 0, |o| o.u64(t.frag_base.unwrap_or(0).max(0) as u64));
                    let mut flags = 0x0001 | 0x0100 | 0x0200 | 0x0400;
                    if t.is_video {
                        flags |= 0x0800;
                    }
                    fbx(o, b"trun", t.is_video as u8, flags, |o| {
                        o.u32(t.frag.len() as u32);
                        fields.push((o.len(), i));
                        o.i32(0);
                        for e in &t.frag {
                            o.u32(e.duration);
                            o.u32(e.size);
                            o.u32(if e.sync { SYNC } else { NON_SYNC });
                            if t.is_video {
                                o.i32(e.c);
                            }
                        }
                    });
                });
            }
        });
        let moof_at = self.pos;
        let mut offset = moof.len() as u64 + mdat_header;
        for &(at, i) in &fields {
            moof[at..at + 4].copy_from_slice(&(offset as i32).to_be_bytes());
            let t = &mut self.tracks[i];
            t.chunks.push((moof_at + offset, t.frag.len() as u32));
            offset += t.frag_data.len() as u64;
        }
        let mut header = Vec::with_capacity(16);
        if mdat_header == 16 {
            header.u32(1);
            header.bytes(b"mdat");
            header.u64(total + 16);
        } else {
            header.u32((total + 8) as u32);
            header.bytes(b"mdat");
        }
        self.put(&moof)?;
        self.put(&header)?;
        for i in 0..self.tracks.len() {
            let data = std::mem::take(&mut self.tracks[i].frag_data);
            self.put(&data)?;
            let t = &mut self.tracks[i];
            t.frag.clear();
            t.frag_base = None;
        }
        self.moofs.push(moof_at);
        if self.options.sync_fragments {
            self.file.sync_data()?;
        }
        Ok(())
    }

    /// The `moov`: while recording (`fragmented`), the tracks without samples and `mvex`;
    /// finished, the full sample tables and the chapter track `chapters` (its id, duration in
    /// ms, and its samples' offset, sizes and durations).
    fn moov(&self, out: &mut Vec<u8>, fragmented: bool, chapters: Option<&Chapters>) {
        let durations: Vec<(i64, i64, i64)> = self
            .tracks
            .iter()
            .map(|t| {
                let media = (t.end_d - t.first_d.unwrap_or(0)).max(0);
                let empty = ticks_to_ms(t.first_d.unwrap_or(0).max(0), t.timescale);
                let shown = ticks_to_ms((media - t.priming).max(0), t.timescale);
                (media, empty, shown)
            })
            .collect();
        let movie = if fragmented {
            0
        } else {
            durations.iter().map(|(_, e, s)| e + s).chain(chapters.map(|c| c.total_ms)).max().unwrap_or(0)
        };
        let next_id = self.tracks.len() as u32 + 1 + chapters.is_some() as u32;
        bx(out, b"moov", |o| {
            mvhd(o, movie, next_id);
            for (i, t) in self.tracks.iter().enumerate() {
                let (media, empty, shown) = if fragmented { (0, 0, 0) } else { durations[i] };
                let chapter_ref = chapters.filter(|_| i == self.primary).map(|c| c.id);
                self.trak(o, t, fragmented, media, empty, shown, chapter_ref);
            }
            if let Some(c) = chapters {
                self.chapter_trak(o, c);
            }
            if fragmented {
                bx(o, b"mvex", |o| {
                    for t in &self.tracks {
                        fbx(o, b"trex", 0, 0, |o| {
                            o.u32(t.id);
                            o.u32(1);
                            o.u32(0);
                            o.u32(0);
                            o.u32(0);
                        });
                    }
                });
            }
            if let Some(title) = &self.options.title {
                bx(o, b"udta", |o| {
                    if self.mov {
                        // QuickTime's international text: length, language, text
                        bx(o, b"\xA9nam", |o| {
                            o.u16(title.len() as u16);
                            o.u16(0x55C4);
                            o.bytes(title.as_bytes());
                        });
                    } else {
                        bx(o, b"name", |o| o.bytes(title.as_bytes()));
                    }
                });
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn trak(
        &self,
        out: &mut Vec<u8>,
        t: &Track,
        fragmented: bool,
        media: i64,
        empty_ms: i64,
        shown_ms: i64,
        chapter_ref: Option<u32>,
    ) {
        bx(out, b"trak", |o| {
            let (w, h, volume, group) = match &t.spec.format {
                TrackFormat::Video(v) => (v.width, v.height, 0, 0),
                TrackFormat::Audio(_) => (0, 0, 0x0100, 1),
            };
            tkhd(o, t.id, 0x7, empty_ms + shown_ms, group, volume, w, h);
            // edits: the time before the track's first sample, then the media from its
            // priming on (while recording, the fragments' decode times place the samples)
            if fragmented {
                if t.priming > 0 {
                    bx(o, b"edts", |o| {
                        fbx(o, b"elst", 0, 0, |o| {
                            o.u32(1);
                            o.u32(0);
                            o.i32(t.priming as i32);
                            o.u32(0x0001_0000);
                        });
                    });
                }
            } else {
                bx(o, b"edts", |o| {
                    let entries = 1 + (empty_ms > 0) as u32;
                    fbx(o, b"elst", 0, 0, |o| {
                        o.u32(entries);
                        if empty_ms > 0 {
                            o.u32(empty_ms as u32);
                            o.i32(-1);
                            o.u32(0x0001_0000);
                        }
                        o.u32(shown_ms.clamp(0, u32::MAX as i64) as u32);
                        o.i32(t.priming as i32);
                        o.u32(0x0001_0000);
                    });
                });
            }
            if let Some(id) = chapter_ref {
                bx(o, b"tref", |o| bx(o, b"chap", |o| o.u32(id)));
            }
            if let Some(name) = &t.spec.name {
                bx(o, b"udta", |o| bx(o, b"name", |o| o.bytes(name.as_bytes())));
            }
            bx(o, b"mdia", |o| {
                mdhd(o, t.timescale, media, &t.spec.language);
                let (kind, name): (&[u8; 4], &str) =
                    if t.is_video { (b"vide", "VideoHandler") } else { (b"soun", "SoundHandler") };
                self.hdlr(o, b"mhlr", kind, name);
                bx(o, b"minf", |o| {
                    if t.is_video {
                        fbx(o, b"vmhd", 0, 1, |o| o.zeros(8));
                    } else {
                        fbx(o, b"smhd", 0, 0, |o| o.zeros(4));
                    }
                    if self.mov {
                        self.hdlr(o, b"dhlr", b"alis", "DataHandler");
                    }
                    self.dinf(o);
                    bx(o, b"stbl", |o| {
                        fbx(o, b"stsd", 0, 0, |o| {
                            o.u32(1);
                            match &t.spec.format {
                                TrackFormat::Video(v) => self.video_entry(o, v),
                                TrackFormat::Audio(a) => self.audio_entry(o, a, t.id),
                            }
                        });
                        if fragmented {
                            empty_tables(o);
                        } else {
                            tables(o, t);
                        }
                    });
                });
            });
        });
    }

    fn chapter_trak(&self, out: &mut Vec<u8>, c: &Chapters) {
        bx(out, b"trak", |o| {
            // in the movie, not enabled: players show it as chapters, not as text
            tkhd(o, c.id, 0x2, c.total_ms, 0, 0, 0, 0);
            bx(o, b"mdia", |o| {
                mdhd(o, MOVIE_TIMESCALE, c.total_ms, "und");
                self.hdlr(o, b"mhlr", b"text", "ChapterHandler");
                bx(o, b"minf", |o| {
                    if self.mov {
                        bx(o, b"gmhd", |o| {
                            fbx(o, b"gmin", 0, 0, |o| {
                                o.u16(0x40);
                                o.u16(0x8000);
                                o.u16(0x8000);
                                o.u16(0x8000);
                                o.i16(0);
                                o.u16(0);
                            });
                        });
                        self.hdlr(o, b"dhlr", b"alis", "DataHandler");
                    } else {
                        fbx(o, b"nmhd", 0, 0, |_| {});
                    }
                    self.dinf(o);
                    bx(o, b"stbl", |o| {
                        fbx(o, b"stsd", 0, 0, |o| {
                            o.u32(1);
                            tx3g_entry(o);
                        });
                        fbx(o, b"stts", 0, 0, |o| {
                            o.u32(c.durations.len() as u32);
                            for &d in &c.durations {
                                o.u32(1);
                                o.u32(d);
                            }
                        });
                        fbx(o, b"stsc", 0, 0, |o| {
                            o.u32(1);
                            o.u32(1);
                            o.u32(c.sizes.len() as u32);
                            o.u32(1);
                        });
                        fbx(o, b"stsz", 0, 0, |o| {
                            o.u32(0);
                            o.u32(c.sizes.len() as u32);
                            for &s in &c.sizes {
                                o.u32(s);
                            }
                        });
                        fbx(o, b"co64", 0, 0, |o| {
                            o.u32(1);
                            o.u64(c.offset);
                        });
                    });
                });
            });
        });
    }

    /// The handler: ISO's form in MP4, a QuickTime component in MOV.
    fn hdlr(&self, out: &mut Vec<u8>, component: &[u8; 4], kind: &[u8; 4], name: &str) {
        fbx(out, b"hdlr", 0, 0, |o| {
            if self.mov {
                o.bytes(component);
                o.bytes(kind);
                o.zeros(12);
                o.u8(name.len() as u8);
                o.bytes(name.as_bytes());
            } else {
                o.u32(0);
                o.bytes(kind);
                o.zeros(12);
                o.bytes(name.as_bytes());
                o.u8(0);
            }
        });
    }

    fn dinf(&self, out: &mut Vec<u8>) {
        bx(out, b"dinf", |o| {
            fbx(o, b"dref", 0, 0, |o| {
                o.u32(1);
                // the data is in this file
                fbx(o, if self.mov { b"alis" } else { b"url " }, 0, 1, |_| {});
            });
        });
    }

    fn video_entry(&self, out: &mut Vec<u8>, v: &VideoFormat) {
        let fourcc: [u8; 4] = match v.codec {
            VideoCodec::H264 => *b"avc1",
            VideoCodec::Hevc { .. } => *b"hvc1",
            VideoCodec::ProRes(p) => p.fourcc(),
        };
        bx(out, &fourcc, |o| {
            o.zeros(6);
            o.u16(1);
            o.u16(0);
            o.u16(0);
            if self.mov {
                o.bytes(b"appl");
                o.u32(0);
                o.u32(0x200);
            } else {
                o.zeros(12);
            }
            o.u16(v.width as u16);
            o.u16(v.height as u16);
            o.u32(0x0048_0000);
            o.u32(0x0048_0000);
            o.u32(0);
            o.u16(1);
            let name = v.codec.name().as_bytes();
            let n = name.len().min(31);
            o.u8(n as u8);
            o.bytes(&name[..n]);
            o.zeros(31 - n);
            o.u16(0x18);
            o.i16(-1);
            match v.codec {
                VideoCodec::H264 => bx(o, b"avcC", |o| o.bytes(&v.config)),
                VideoCodec::Hevc { .. } => bx(o, b"hvcC", |o| o.bytes(&v.config)),
                VideoCodec::ProRes(_) => {}
            }
            colr(o, &v.color, self.mov);
            bx(o, b"pasp", |o| {
                o.u32(1);
                o.u32(1);
            });
        });
    }

    fn audio_entry(&self, out: &mut Vec<u8>, a: &AudioFormat, track_id: u32) {
        let esds = |o: &mut Vec<u8>| {
            fbx(o, b"esds", 0, 0, |o| {
                o.bytes(&audio_config::es_descriptor(track_id as u16, &a.config, a.bitrate_kbps * 1000))
            });
        };
        let rate = (a.sample_rate.min(65_535)) << 16;
        match (a.codec, self.mov) {
            (AudioCodec::Aac, true) => bx(out, b"mp4a", |o| {
                // QuickTime's sound description, version 1, with the descriptor in `wave`
                o.zeros(6);
                o.u16(1);
                o.u16(1);
                o.u16(0);
                o.u32(0);
                o.u16(a.channels as u16);
                o.u16(16);
                o.i16(-2);
                o.u16(0);
                o.u32(rate);
                o.u32(a.frames_per_packet());
                o.u32(0);
                o.u32(0);
                o.u32(2);
                bx(o, b"wave", |o| {
                    bx(o, b"frma", |o| o.bytes(b"mp4a"));
                    bx(o, b"mp4a", |o| o.u32(0));
                    esds(o);
                    o.u32(8);
                    o.u32(0);
                });
            }),
            (codec, _) => {
                let fourcc = if codec == AudioCodec::Aac { b"mp4a" } else { b"Opus" };
                bx(out, fourcc, |o| {
                    o.zeros(6);
                    o.u16(1);
                    o.zeros(8);
                    o.u16(a.channels as u16);
                    o.u16(16);
                    o.u16(0);
                    o.u16(0);
                    o.u32(rate);
                    if codec == AudioCodec::Aac {
                        esds(o);
                    } else {
                        bx(o, b"dOps", |o| o.bytes(&audio_config::opus_dops(&a.config)));
                    }
                });
            }
        }
    }

    fn chapter_samples(&self, markers: &[Marker], duration_ns: i64) -> Option<(Vec<u8>, Chapters)> {
        if markers.is_empty() {
            return None;
        }
        let mut list: Vec<(i64, &str)> = Vec::new();
        if markers[0].at_ns > 0 {
            list.push((0, ""));
        }
        list.extend(markers.iter().map(|m| (m.at_ns, m.title.as_str())));
        let total_ms = (duration_ns / 1_000_000).max(1);
        let mut data = Vec::new();
        let mut sizes = Vec::new();
        let mut durations = Vec::new();
        for (i, (at, title)) in list.iter().enumerate() {
            let start = at / 1_000_000;
            let end = list.get(i + 1).map_or(total_ms, |(n, _)| n / 1_000_000).max(start + 1);
            let before = data.len();
            let text = &title.as_bytes()[..title.len().min(u16::MAX as usize)];
            data.u16(text.len() as u16);
            data.bytes(text);
            if self.mov {
                // the text is UTF-8
                data.u32(12);
                data.bytes(b"encd");
                data.u32(0x0000_0100);
            }
            sizes.push((data.len() - before) as u32);
            durations.push((end - start) as u32);
        }
        let total_ms = total_ms.max(durations.iter().map(|&d| d as i64).sum());
        Some((data, Chapters { id: self.tracks.len() as u32 + 1, total_ms, offset: 0, sizes, durations }))
    }
}

struct Chapters {
    id: u32,
    total_ms: i64,
    offset: u64,
    sizes: Vec<u32>,
    durations: Vec<u32>,
}

fn ticks_to_ms(t: i64, timescale: u32) -> i64 {
    (t as i128 * MOVIE_TIMESCALE as i128 / timescale.max(1) as i128) as i64
}

fn mvhd(out: &mut Vec<u8>, duration: i64, next_id: u32) {
    let v1 = duration > u32::MAX as i64;
    fbx(out, b"mvhd", v1 as u8, 0, |o| {
        if v1 {
            o.u64(0);
            o.u64(0);
            o.u32(MOVIE_TIMESCALE);
            o.u64(duration as u64);
        } else {
            o.u32(0);
            o.u32(0);
            o.u32(MOVIE_TIMESCALE);
            o.u32(duration as u32);
        }
        o.u32(0x0001_0000);
        o.u16(0x0100);
        o.zeros(10);
        unity_matrix(o);
        o.zeros(24);
        o.u32(next_id);
    });
}

#[allow(clippy::too_many_arguments)]
fn tkhd(out: &mut Vec<u8>, id: u32, flags: u32, duration: i64, group: u16, volume: u16, width: u32, height: u32) {
    let v1 = duration > u32::MAX as i64;
    fbx(out, b"tkhd", v1 as u8, flags, |o| {
        if v1 {
            o.u64(0);
            o.u64(0);
            o.u32(id);
            o.u32(0);
            o.u64(duration as u64);
        } else {
            o.u32(0);
            o.u32(0);
            o.u32(id);
            o.u32(0);
            o.u32(duration.max(0) as u32);
        }
        o.zeros(8);
        o.u16(0);
        o.u16(group);
        o.u16(volume);
        o.u16(0);
        unity_matrix(o);
        o.u32(width << 16);
        o.u32(height << 16);
    });
}

fn mdhd(out: &mut Vec<u8>, timescale: u32, duration: i64, language: &str) {
    let v1 = duration > u32::MAX as i64;
    fbx(out, b"mdhd", v1 as u8, 0, |o| {
        if v1 {
            o.u64(0);
            o.u64(0);
            o.u32(timescale);
            o.u64(duration as u64);
        } else {
            o.u32(0);
            o.u32(0);
            o.u32(timescale);
            o.u32(duration.max(0) as u32);
        }
        o.u16(packed_language(language));
        o.u16(0);
    });
}

/// Colour: `nclx` (ISO, with the range) in MP4, `nclc` in MOV.
fn colr(out: &mut Vec<u8>, c: &Color, mov: bool) {
    bx(out, b"colr", |o| {
        o.bytes(if mov { b"nclc" } else { b"nclx" });
        o.u16(c.primaries);
        o.u16(c.transfer);
        o.u16(c.matrix);
        if !mov {
            o.u8(if c.full_range { 0x80 } else { 0 });
        }
    });
}

/// A 3GPP timed text sample entry, as chapter tracks use.
fn tx3g_entry(out: &mut Vec<u8>) {
    bx(out, b"tx3g", |o| {
        o.zeros(6);
        o.u16(1);
        o.u32(0);
        o.u8(1);
        o.u8(0xFF);
        o.zeros(4);
        o.zeros(8);
        // style: characters 0–0, font 1, plain, 18 pt, white
        o.u16(0);
        o.u16(0);
        o.u16(1);
        o.u8(0);
        o.u8(18);
        o.bytes(&[0xFF; 4]);
        bx(o, b"ftab", |o| {
            o.u16(1);
            o.u16(1);
            let name = b"Sans-Serif";
            o.u8(name.len() as u8);
            o.bytes(name);
        });
    });
}

fn empty_tables(out: &mut Vec<u8>) {
    fbx(out, b"stts", 0, 0, |o| o.u32(0));
    fbx(out, b"stsc", 0, 0, |o| o.u32(0));
    fbx(out, b"stsz", 0, 0, |o| {
        o.u32(0);
        o.u32(0);
    });
    fbx(out, b"stco", 0, 0, |o| o.u32(0));
}

fn tables(out: &mut Vec<u8>, t: &Track) {
    fbx(out, b"stts", 0, 0, |o| {
        o.u32(t.stts.len() as u32);
        for &(n, d) in &t.stts {
            o.u32(n);
            o.u32(d);
        }
    });
    if t.any_offset {
        fbx(out, b"ctts", 1, 0, |o| {
            o.u32(t.ctts.len() as u32);
            for &(n, c) in &t.ctts {
                o.u32(n);
                o.i32(c);
            }
        });
    }
    if !t.all_sync {
        fbx(out, b"stss", 0, 0, |o| {
            o.u32(t.sync_samples.len() as u32);
            for &s in &t.sync_samples {
                o.u32(s);
            }
        });
    }
    fbx(out, b"stsc", 0, 0, |o| {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (i, &(_, n)) in t.chunks.iter().enumerate() {
            if runs.last().is_none_or(|&(_, last)| last != n) {
                runs.push((i as u32 + 1, n));
            }
        }
        o.u32(runs.len() as u32);
        for (first, n) in runs {
            o.u32(first);
            o.u32(n);
            o.u32(1);
        }
    });
    fbx(out, b"stsz", 0, 0, |o| {
        o.u32(0);
        o.u32(t.sizes.len() as u32);
        for &s in &t.sizes {
            o.u32(s);
        }
    });
    fbx(out, b"co64", 0, 0, |o| {
        o.u32(t.chunks.len() as u32);
        for &(at, _) in &t.chunks {
            o.u64(at);
        }
    });
}

impl Writer for Mp4Writer {
    fn write(&mut self, s: Sample) -> Result<(), Error> {
        let i = s.track;
        let (pts, dts) = {
            let t = &self.tracks[i];
            (ticks(s.pts_ns, t.timescale), ticks(s.dts_ns, t.timescale))
        };
        // a fragment ends before the primary track's first keyframe this long after its start
        // (or, with no keyframe for ten times as long, anywhere)
        let age = self.frag_start_ns.map_or(0, |f| s.pts_ns - f);
        let interval = self.options.fragment_ms.max(100) as i64 * 1_000_000;
        let cut = i == self.primary
            && self.frag_start_ns.is_some()
            && ((s.keyframe && age >= interval) || age >= interval * 10);
        let t = &mut self.tracks[i];
        let (fp, fd) = *t.first.get_or_insert((pts, dts));
        // decode times from the first packet's presentation time on, so every track lines up
        // on the file's clock; composition offsets make up the difference
        let mut d = dts - fd + fp;
        if let Some(last) = t.last_d {
            d = d.max(last + 1);
        }
        let duration = t.pending.as_ref().map(|p| d - p.d);
        if let Some(duration) = duration {
            t.commit(duration);
        }
        t.last_d = Some(d);
        t.pending = Some(Pending {
            data: s.data,
            d,
            c: (pts - d).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            sync: s.keyframe,
        });
        if cut {
            self.flush_fragment()?;
            self.frag_start_ns = Some(s.pts_ns);
        } else if self.frag_start_ns.is_none() && i == self.primary {
            self.frag_start_ns = Some(s.pts_ns);
        }
        Ok(())
    }

    fn finish(&mut self, markers: &[Marker], duration_ns: i64) -> Result<u64, Error> {
        for t in &mut self.tracks {
            let nominal = t.nominal;
            t.commit(nominal);
        }
        self.flush_fragment()?;
        let mut chapters = self.chapter_samples(markers, duration_ns);
        if let Some((data, c)) = &mut chapters {
            let mut header = Vec::new();
            header.u32(data.len() as u32 + 8);
            header.bytes(b"mdat");
            c.offset = self.pos + 8;
            self.put(&header)?;
            self.put(data)?;
        }
        let mut moov = Vec::new();
        self.moov(&mut moov, false, chapters.as_ref().map(|(_, c)| c));
        self.put(&moov)?;
        self.file.sync_data()?;
        // the recording-time headers become free space: the new index is the file's
        let moofs = std::mem::take(&mut self.moofs);
        for at in std::iter::once(self.init_moov_at).chain(moofs) {
            self.file.seek(SeekFrom::Start(at + 4))?;
            self.file.write_all(b"free")?;
        }
        self.file.seek(SeekFrom::Start(self.pos))?;
        self.file.sync_all()?;
        Ok(self.pos)
    }
}
