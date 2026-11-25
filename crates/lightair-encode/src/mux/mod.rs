//! Recording files from encoded packets, never encoded again.
//!
//! [`Recorder`] takes each track's packets as the encoders send them, starts the file at the
//! first video keyframe (sound that arrives earlier waits for it, up to a few seconds), and
//! writes:
//!
//! - **MP4 and MOV** as fragments every [`RecorderOptions::fragment_ms`] (each starting at a
//!   keyframe), so a file cut off by a crash or a full disk plays up to its last fragment.
//!   [`Recorder::finish`] then writes an ordinary index (`moov`) at the end, with the markers as
//!   a chapter track, and turns the fragments' headers into free space: the finished file is a
//!   plain progressive MP4 or MOV every editor reads.
//! - **Matroska** as clusters (each starting at a keyframe) in a segment of unknown size, so a
//!   file cut off plays up to its last cluster; finishing adds the cues, the chapters, the
//!   duration and the seek head.

mod bytes;
mod mkv;
mod mp4;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::format::{Packet, TrackFormat, VideoCodec};
use crate::nal::{self, NalLayout};
use crate::{AudioCodec, Error};

/// The file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    Mp4,
    /// QuickTime: the only one of the three that takes ProRes.
    Mov,
    Mkv,
}

impl Container {
    /// By a path's extension (`mp4`, `m4v`, `mov`, `mkv`).
    pub fn from_path(path: &Path) -> Option<Container> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "mp4" | "m4v" | "m4a" => Some(Container::Mp4),
            "mov" | "qt" => Some(Container::Mov),
            "mkv" | "mka" => Some(Container::Mkv),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mov => "mov",
            Container::Mkv => "mkv",
        }
    }

    /// Whether the container can carry `format`.
    pub fn accepts(self, format: &TrackFormat) -> bool {
        match (self, format) {
            (Container::Mp4, TrackFormat::Video(v)) => v.codec.is_nal(),
            (Container::Mov, TrackFormat::Audio(a)) => a.codec == AudioCodec::Aac,
            _ => true,
        }
    }
}

/// One track of a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSpec {
    pub format: TrackFormat,
    /// Shown by players and editors ("Microphone", "Game").
    pub name: Option<String>,
    /// ISO 639-2 (`eng`, `jpn`); `und` when unknown.
    pub language: String,
}

impl TrackSpec {
    pub fn new(format: TrackFormat) -> TrackSpec {
        TrackSpec { format, name: None, language: "und".into() }
    }

    pub fn named(mut self, name: impl Into<String>) -> TrackSpec {
        self.name = Some(name.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderOptions {
    /// How often the file is made playable up to now (a fragment or a cluster ends at the
    /// first keyframe this long after the last). 2000 by default.
    pub fragment_ms: u32,
    /// Flush each fragment to the disk (`fsync`), so a power cut loses at most one.
    pub sync_fragments: bool,
    /// The file's title, if any.
    pub title: Option<String>,
    /// The writing application's name, recorded in the file.
    pub writing_app: String,
}

impl Default for RecorderOptions {
    fn default() -> Self {
        RecorderOptions { fragment_ms: 2000, sync_fragments: true, title: None, writing_app: "Lightair".into() }
    }
}

/// A finished recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub path: PathBuf,
    pub duration_ns: i64,
    pub bytes: u64,
}

/// A marker: a chapter in the finished file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    /// From the file's start.
    pub at_ns: i64,
    pub title: String,
}

/// A packet as the container writers take it: the sample's bytes as stored, times from the
/// file's start.
pub(crate) struct Sample {
    pub track: usize,
    pub data: Vec<u8>,
    pub pts_ns: i64,
    pub dts_ns: i64,
    pub keyframe: bool,
}

pub(crate) trait Writer: Send {
    fn write(&mut self, sample: Sample) -> Result<(), Error>;
    /// Writes what's left, the index and the markers; returns the file's length.
    fn finish(&mut self, markers: &[Marker], duration_ns: i64) -> Result<u64, Error>;
}

/// How long sound may wait for the first keyframe before the oldest is dropped.
const PREROLL_NS: i64 = 5_000_000_000;

struct Held {
    track: usize,
    data: Vec<u8>,
    layout: NalLayout,
    pts_ns: i64,
    dts_ns: i64,
    keyframe: bool,
}

/// One recording file being written.
pub struct Recorder {
    path: PathBuf,
    tracks: Vec<TrackSpec>,
    writer: Box<dyn Writer>,
    /// The program time the file starts at: the first keyframe of the first video track (or
    /// the first sound packet of a sound-only file).
    origin: Option<i64>,
    /// Packets waiting for the origin.
    held: VecDeque<Held>,
    /// Per track: it has had its first keyframe (pictures before it can't be decoded).
    started: Vec<bool>,
    /// Per track: the end of its last packet, from the file's start.
    ends: Vec<i64>,
    markers: Vec<(i64, String)>,
    primary_video: Option<usize>,
}

impl Recorder {
    /// Creates `path` for `tracks` in `container`. Fails when a track doesn't fit the
    /// container (ProRes outside MOV and Matroska, Opus in MOV) or the file can't be made.
    pub fn create(
        path: impl AsRef<Path>,
        container: Container,
        tracks: Vec<TrackSpec>,
        options: RecorderOptions,
    ) -> Result<Recorder, Error> {
        let path = path.as_ref().to_path_buf();
        if tracks.is_empty() {
            return Err(Error::Mux("a recording needs at least one track".into()));
        }
        for t in &tracks {
            if !container.accepts(&t.format) {
                return Err(Error::Mux(format!(
                    "{} can't carry {}",
                    container.extension().to_uppercase(),
                    match &t.format {
                        TrackFormat::Video(v) => v.codec.name(),
                        TrackFormat::Audio(a) => a.codec.name(),
                    }
                )));
            }
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let writer: Box<dyn Writer> = match container {
            Container::Mp4 | Container::Mov => {
                Box::new(mp4::Mp4Writer::create(&path, container == Container::Mov, &tracks, &options)?)
            }
            Container::Mkv => Box::new(mkv::MkvWriter::create(&path, &tracks, &options)?),
        };
        let primary_video = tracks.iter().position(|t| matches!(t.format, TrackFormat::Video(_)));
        let n = tracks.len();
        Ok(Recorder {
            path,
            tracks,
            writer,
            origin: None,
            held: VecDeque::new(),
            started: vec![false; n],
            ends: vec![0; n],
            markers: Vec::new(),
            primary_video,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn tracks(&self) -> &[TrackSpec] {
        &self.tracks
    }

    /// The program time the file starts at, once known.
    pub fn origin_ns(&self) -> Option<i64> {
        self.origin
    }

    /// One packet of track `track` (its index in the `tracks` given to [`Recorder::create`]).
    /// Packets of one track come in decode order; tracks may interleave as they like.
    pub fn write(&mut self, track: usize, packet: Packet<'_>) -> Result<(), Error> {
        if track >= self.tracks.len() {
            return Err(Error::Mux(format!("no track {track}")));
        }
        if self.origin.is_some() {
            return self.emit(track, packet);
        }
        let starts = match self.primary_video {
            Some(v) => track == v && packet.keyframe,
            None => true,
        };
        if !starts {
            self.hold(track, packet);
            return Ok(());
        }
        self.origin = Some(packet.pts_ns);
        self.emit(track, packet)?;
        // what waited: sound from the origin on (pictures before a keyframe are useless)
        let held = std::mem::take(&mut self.held);
        for h in held {
            let p =
                Packet { data: &h.data, layout: h.layout, pts_ns: h.pts_ns, dts_ns: h.dts_ns, keyframe: h.keyframe };
            self.emit(h.track, p)?;
        }
        Ok(())
    }

    fn hold(&mut self, track: usize, packet: Packet<'_>) {
        if !matches!(self.tracks[track].format, TrackFormat::Audio(_)) {
            return;
        }
        self.held.push_back(Held {
            track,
            data: packet.data.to_vec(),
            layout: packet.layout,
            pts_ns: packet.pts_ns,
            dts_ns: packet.dts_ns,
            keyframe: packet.keyframe,
        });
        while self.held.front().is_some_and(|f| packet.pts_ns - f.pts_ns > PREROLL_NS) {
            self.held.pop_front();
        }
    }

    fn emit(&mut self, track: usize, packet: Packet<'_>) -> Result<(), Error> {
        let origin = self.origin.unwrap_or(packet.pts_ns);
        let pts = packet.pts_ns - origin;
        let (data, length) = match &self.tracks[track].format {
            TrackFormat::Video(v) => {
                if !self.started[track] {
                    if !packet.keyframe {
                        return Ok(());
                    }
                    self.started[track] = true;
                }
                if pts < 0 {
                    return Ok(());
                }
                let data = match v.codec {
                    VideoCodec::H264 => nal::to_sample(packet.data, packet.layout, false),
                    VideoCodec::Hevc { .. } => nal::to_sample(packet.data, packet.layout, true),
                    VideoCodec::ProRes(_) => packet.data.to_vec(),
                };
                (data, v.frame_ns())
            }
            TrackFormat::Audio(a) => {
                if pts < 0 {
                    return Ok(());
                }
                self.started[track] = true;
                (packet.data.to_vec(), a.packet_ns())
            }
        };
        if data.is_empty() {
            return Ok(());
        }
        self.ends[track] = self.ends[track].max(pts + length);
        self.writer.write(Sample {
            track,
            data,
            pts_ns: pts,
            dts_ns: packet.dts_ns - origin,
            keyframe: packet.keyframe,
        })
    }

    /// A marker at program time `at_ns`: a chapter starting there in the finished file
    /// (markers before the file's start are put at its start).
    pub fn add_marker(&mut self, at_ns: i64, title: impl Into<String>) {
        self.markers.push((at_ns, title.into()));
    }

    /// The length so far, from the first keyframe to the end of the last packet.
    pub fn duration_ns(&self) -> i64 {
        self.ends.iter().copied().max().unwrap_or(0)
    }

    /// Writes what's left and the index; the file is complete.
    pub fn finish(mut self) -> Result<Finished, Error> {
        let origin = self.origin.unwrap_or(0);
        let duration = self.duration_ns();
        let mut markers: Vec<Marker> = self
            .markers
            .iter()
            .map(|(at, title)| Marker { at_ns: (at - origin).clamp(0, duration.max(0)), title: title.clone() })
            .collect();
        markers.sort_by_key(|m| m.at_ns);
        let bytes = self.writer.finish(&markers, duration)?;
        Ok(Finished { path: self.path.clone(), duration_ns: duration, bytes })
    }
}

/// Random 64-bit identifiers (Matroska's UIDs) without a random number crate.
pub(crate) fn uid() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish().max(1)
}
