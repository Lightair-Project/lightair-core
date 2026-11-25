//! Test material from ffmpeg: a few seconds of picture and tone, as a phone's encoders would
//! hand them over.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use lightair_feed::ts::{NalFormat, VideoCodec, split_nals};

pub fn ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ts-decodes");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

pub fn run(args: &[&str]) -> String {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Access units from an elementary stream with access unit delimiters.
/// A video access unit (length-prefixed) and whether it's a keyframe.
pub type Unit = (Vec<u8>, bool);

pub fn access_units(es: &[u8], codec: VideoCodec) -> Vec<Unit> {
    let is_aud = |n: &[u8]| match codec {
        VideoCodec::H264 => n[0] & 0x1F == 9,
        VideoCodec::Hevc => (n[0] >> 1) & 0x3F == 35,
    };
    let is_key = |n: &[u8]| match codec {
        VideoCodec::H264 => n[0] & 0x1F == 5,
        VideoCodec::Hevc => matches!((n[0] >> 1) & 0x3F, 16..=21),
    };
    let mut units: Vec<Unit> = vec![];
    for n in split_nals(es, NalFormat::AnnexB) {
        if is_aud(n) || units.is_empty() {
            units.push((vec![], false));
        }
        if is_aud(n) {
            continue;
        }
        let u = units.last_mut().unwrap();
        // as VideoToolbox gives them: 4-byte lengths
        u.0.extend_from_slice(&(n.len() as u32).to_be_bytes());
        u.0.extend_from_slice(n);
        u.1 |= is_key(n);
    }
    units.retain(|u| !u.0.is_empty());
    units
}

/// Raw AAC access units from ADTS.
pub fn aac_units(adts: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![];
    let mut i = 0;
    while i + 7 <= adts.len() {
        let len = (((adts[i + 3] & 3) as usize) << 11)
            | ((adts[i + 4] as usize) << 3)
            | ((adts[i + 5] >> 5) as usize);
        let header = if adts[i + 1] & 1 == 0 { 9 } else { 7 };
        out.push(adts[i + header..i + len].to_vec());
        i += len;
    }
    out
}

/// Two seconds of 30 fps test picture (access units, length-prefixed, and whether each is a
/// keyframe) and 48 kHz stereo tone (raw AAC access units).
pub fn material(codec: VideoCodec, name: &str) -> (Vec<Unit>, Vec<Vec<u8>>) {
    let (encoder, ext, aud) = match codec {
        VideoCodec::H264 => (
            "libx264",
            "h264",
            ["-x264-params", "aud=1:keyint=30:bframes=0"],
        ),
        VideoCodec::Hevc => (
            "libx265",
            "hevc",
            ["-x265-params", "aud=1:keyint=30:bframes=0:log-level=error"],
        ),
    };
    let video = scratch(&format!("{name}.{ext}"));
    let audio = scratch(&format!("{name}.aac"));
    run(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-t",
        "2",
        "-c:v",
        encoder,
        "-pix_fmt",
        "yuv420p",
        aud[0],
        aud[1],
        video.to_str().unwrap(),
    ]);
    run(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "2",
        "-ac",
        "2",
        "-c:a",
        "aac",
        "-f",
        "adts",
        audio.to_str().unwrap(),
    ]);
    (
        access_units(&std::fs::read(&video).unwrap(), codec),
        aac_units(&std::fs::read(&audio).unwrap()),
    )
}

/// Frames and their codec, as ffprobe counts them.
pub fn probe(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,nb_read_frames",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}
