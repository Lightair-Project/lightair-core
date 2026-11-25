//! Clips: the last minute of each stream encode kept as encoded packets (at most 30 MB each), and a
//! clip of the newest seconds written from a keyframe to an MP4 with the stream's sound, without
//! encoding again.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

/// How long and how much a ring keeps.
const KEEP_NS: i64 = 60_000_000_000;
const KEEP_BYTES: usize = 30 << 20;

#[derive(Clone)]
pub struct Packet {
    pub pts_ns: i64,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// 4-byte lengths (VideoToolbox) rather than start codes (MediaCodec).
    pub length_prefixed: bool,
}

#[derive(Default)]
pub struct Ring {
    packets: Mutex<(VecDeque<Packet>, usize)>,
}

impl Ring {
    pub fn push(&self, p: Packet) {
        let mut r = self.packets.lock();
        r.1 += p.data.len();
        let newest = p.pts_ns;
        r.0.push_back(p);
        while r.1 > KEEP_BYTES || r.0.front().is_some_and(|f| newest - f.pts_ns > KEEP_NS) {
            let Some(old) = r.0.pop_front() else { break };
            r.1 -= old.data.len();
        }
    }

    pub fn snapshot(&self) -> Vec<Packet> {
        self.packets.lock().0.iter().cloned().collect()
    }
}

/// NAL units of a packet, in either form.
fn nals(p: &Packet) -> Vec<&[u8]> {
    let format = if p.length_prefixed {
        lightair_feed::NalFormat::LengthPrefixed
    } else {
        lightair_feed::NalFormat::AnnexB
    };
    lightair_feed::ts::split_nals(&p.data, format)
}

/// A picture as MP4 wants it: 4-byte lengths, without parameter sets (they're in `avcC`).
fn avcc(p: &Packet) -> Vec<u8> {
    let mut out = Vec::with_capacity(p.data.len() + 16);
    for n in nals(p) {
        if matches!(n.first().map(|b| b & 0x1F), Some(7 | 8)) {
            continue;
        }
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

/// Writes the newest `seconds` of `video` (from the keyframe before them) with the `audio` of
/// the same time to `path`. `sets`: the encoder's SPS and PPS; `size`: its picture.
pub fn write(
    path: &Path,
    seconds: u32,
    video: &[Packet],
    sets: &[Vec<u8>],
    size: (u32, u32),
    audio: &[Packet],
) -> Result<(), String> {
    use mp4::{
        AacConfig, AudioObjectType, AvcConfig, ChannelConfig, MediaConfig, Mp4Config, Mp4Sample,
        Mp4Writer, SampleFreqIndex, TrackConfig, TrackType,
    };
    let newest = video.last().ok_or("nothing streamed yet")?.pts_ns;
    let from = newest - seconds as i64 * 1_000_000_000;
    let start = video
        .iter()
        .rposition(|p| p.keyframe && p.pts_ns <= from)
        .or_else(|| video.iter().position(|p| p.keyframe))
        .ok_or("no keyframe yet")?;
    let video = &video[start..];
    let t0 = video[0].pts_ns;
    let sps = sets
        .iter()
        .find(|n| n.first().map(|b| b & 0x1F) == Some(7))
        .cloned()
        .or_else(|| {
            nals(&video[0])
                .into_iter()
                .find(|n| n[0] & 0x1F == 7)
                .map(<[u8]>::to_vec)
        })
        .ok_or("no SPS")?;
    let pps = sets
        .iter()
        .find(|n| n.first().map(|b| b & 0x1F) == Some(8))
        .cloned()
        .or_else(|| {
            nals(&video[0])
                .into_iter()
                .find(|n| n[0] & 0x1F == 8)
                .map(<[u8]>::to_vec)
        })
        .ok_or("no PPS")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let e = |e: mp4::Error| e.to_string();
    let mut w = Mp4Writer::write_start(
        std::io::BufWriter::new(file),
        &Mp4Config {
            major_brand: str::parse("isom").expect("a brand"),
            minor_version: 512,
            compatible_brands: vec![
                str::parse("isom").expect("a brand"),
                str::parse("iso2").expect("a brand"),
                str::parse("avc1").expect("a brand"),
                str::parse("mp41").expect("a brand"),
            ],
            timescale: 1000,
        },
    )
    .map_err(e)?;
    w.add_track(&TrackConfig {
        track_type: TrackType::Video,
        timescale: 90_000,
        language: "und".into(),
        media_conf: MediaConfig::AvcConfig(AvcConfig {
            width: size.0 as u16,
            height: size.1 as u16,
            seq_param_set: sps,
            pic_param_set: pps,
        }),
    })
    .map_err(e)?;
    let audio: Vec<&Packet> = audio
        .iter()
        .filter(|p| p.pts_ns >= t0 && p.pts_ns <= newest)
        .collect();
    if !audio.is_empty() {
        w.add_track(&TrackConfig {
            track_type: TrackType::Audio,
            timescale: 48_000,
            language: "und".into(),
            media_conf: MediaConfig::AacConfig(AacConfig {
                bitrate: 160_000,
                profile: AudioObjectType::AacLowComplexity,
                freq_index: SampleFreqIndex::Freq48000,
                chan_conf: ChannelConfig::Stereo,
            }),
        })
        .map_err(e)?;
    }
    let ticks =
        |ns: i64, rate: i64| ((ns - t0).max(0) as i128 * rate as i128 / 1_000_000_000) as u64;
    for (i, p) in video.iter().enumerate() {
        let next = video.get(i + 1).map_or(p.pts_ns + 33_333_333, |n| n.pts_ns);
        w.write_sample(
            1,
            &Mp4Sample {
                start_time: ticks(p.pts_ns, 90_000),
                duration: (ticks(next, 90_000) - ticks(p.pts_ns, 90_000)).max(1) as u32,
                rendering_offset: 0,
                is_sync: p.keyframe,
                bytes: avcc(p).into(),
            },
        )
        .map_err(e)?;
    }
    for p in &audio {
        w.write_sample(
            2,
            &Mp4Sample {
                start_time: ticks(p.pts_ns, 48_000),
                duration: 1024,
                rendering_offset: 0,
                is_sync: true,
                bytes: p.data.clone().into(),
            },
        )
        .map_err(e)?;
    }
    w.write_end().map_err(e)?;
    Ok(())
}

/// Where clips go: beside the recordings, in `Clips`.
pub fn folder(shared: &Path) -> PathBuf {
    crate::record::folder(shared).join("Clips")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_a_minute_and_a_clip_starts_at_a_keyframe() {
        let ring = Ring::default();
        // 90 s of 30 fps, a keyframe every 2 s
        for i in 0..2700i64 {
            let key = i % 60 == 0;
            let nal = if key { 0x65 } else { 0x41 };
            ring.push(Packet {
                pts_ns: i * 33_333_333,
                keyframe: key,
                data: vec![0, 0, 0, 3, nal, 1, 2],
                length_prefixed: true,
            });
        }
        let video = ring.snapshot();
        assert!(
            video.len() <= 1801 && video.len() > 1700,
            "a minute: {}",
            video.len()
        );
        let audio: Vec<Packet> = (0..4300)
            .map(|i| Packet {
                pts_ns: i * 21_333_333,
                keyframe: true,
                data: vec![0x21, 0x10, 4],
                length_prefixed: false,
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        let sets = [
            vec![0x67, 0x42, 0xe0, 0x1f, 0xda],
            vec![0x68, 0xce, 0x3c, 0x80],
        ];
        write(&path, 10, &video, &sets, (640, 360), &audio).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mp4 =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(&bytes), bytes.len() as u64).unwrap();
        let v = mp4
            .tracks()
            .values()
            .find(|t| t.track_type().unwrap() == mp4::TrackType::Video)
            .unwrap();
        // from the keyframe at or before 10 s back: 10–12 s of pictures
        let n = v.sample_count();
        assert!((300..=361).contains(&n), "{n} pictures");
        assert_eq!(mp4.tracks().len(), 2, "with its sound");
    }
}
