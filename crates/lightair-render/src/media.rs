//! Decoding and encoding through FFmpeg, run as a helper program (`ffmpeg`, `ffprobe`), frames
//! over pipes as raw RGBA. The same on macOS and Windows, with each platform's hardware codecs
//! (VideoToolbox; NVENC, AMF, Quick Sync) when FFmpeg has them.
//!
//! The helpers are found in `LIGHTAIR_FFMPEG_DIR`, next to Lightair, or on the PATH.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::edit::Clip;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("FFmpeg isn't installed (Lightair looks next to itself, in LIGHTAIR_FFMPEG_DIR and on the PATH)")]
    NoFfmpeg,
    #[error("couldn't read {0}")]
    Unreadable(PathBuf),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("FFmpeg stopped: {0}")]
    Failed(String),
}

fn find(tool: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{tool}.exe") } else { tool.to_string() };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("LIGHTAIR_FFMPEG_DIR") {
        dirs.push(d.into());
    }
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(d);
    }
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    // where Homebrew puts it, for apps that don't inherit a shell's PATH
    if cfg!(target_os = "macos") {
        dirs.push("/opt/homebrew/bin".into());
        dirs.push("/usr/local/bin".into());
    }
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

pub fn ffmpeg() -> Result<PathBuf, MediaError> {
    find("ffmpeg").ok_or(MediaError::NoFfmpeg)
}

/// A video's size, frame rate and length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_ms: u64,
}

pub fn probe(path: &Path) -> Result<Probe, MediaError> {
    let ffprobe = find("ffprobe").ok_or(MediaError::NoFfmpeg)?;
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,avg_frame_rate:format=duration"])
        .args(["-of", "json"])
        .arg(path)
        .output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| MediaError::Unreadable(path.into()))?;
    let s = &v["streams"][0];
    let fps = s["avg_frame_rate"].as_str().and_then(|r| {
        let (n, d) = r.split_once('/')?;
        let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
        (d > 0.0).then_some(n / d)
    });
    let duration = v["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok());
    match (s["width"].as_u64(), s["height"].as_u64(), duration) {
        (Some(w), Some(h), Some(d)) => Ok(Probe { width: w as u32, height: h as u32, fps: fps.unwrap_or(30.0), duration_ms: (d * 1000.0) as u64 }),
        _ => Err(MediaError::Unreadable(path.into())),
    }
}

/// A video read in time order, at one size: the frame for a time, reading ahead from where it
/// is and starting again only for a jump (back, or far ahead).
pub struct Stream {
    path: PathBuf,
    size: (u32, u32),
    fps: f64,
    dec: Option<Decoder>,
    current: Vec<u8>,
    /// The time of the frame in `current`.
    at: Option<u64>,
}

impl Stream {
    pub fn new(path: &Path, size: (u32, u32), fps: f64) -> Stream {
        Stream { path: path.to_path_buf(), size, fps, dec: None, current: Vec::new(), at: None }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The frame at `ms` of the video.
    pub fn frame(&mut self, ms: u64) -> Option<&[u8]> {
        let frame = (1000.0 / self.fps) as u64;
        let behind = self.at.is_some_and(|a| ms + frame / 2 < a);
        let far = self.dec.as_ref().is_some_and(|d| ms > d.next_ms() + 2_000);
        if self.dec.is_none() || behind || far {
            self.dec = Decoder::open(&self.path, ms, self.fps, self.size).ok();
            self.at = None;
        }
        let dec = self.dec.as_mut()?;
        while self.at.is_none() || dec.next_ms() <= ms {
            let t = dec.next_ms();
            if !dec.next(&mut self.current) {
                break;
            }
            self.at = Some(t);
        }
        self.at.map(|_| self.current.as_slice())
    }
}

/// What a file holds: video (with or without sound), sound only, or a still; its size and length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contents {
    pub video: bool,
    pub audio: bool,
    pub still: bool,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

pub fn contents(path: &Path) -> Result<Contents, MediaError> {
    let ffprobe = find("ffprobe").ok_or(MediaError::NoFfmpeg)?;
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "stream=codec_type,width,height,nb_frames:format=duration,format_name", "-of", "json"])
        .arg(path)
        .output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| MediaError::Unreadable(path.into()))?;
    let streams = v["streams"].as_array().ok_or_else(|| MediaError::Unreadable(path.into()))?;
    let video = streams.iter().find(|s| s["codec_type"] == "video");
    let audio = streams.iter().any(|s| s["codec_type"] == "audio");
    let format = v["format"]["format_name"].as_str().unwrap_or("");
    // a picture: an image format, or one frame and no length
    let still = video.is_some()
        && (format.contains("image2") || format.contains("_pipe") || ["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "heic"].iter().any(|e| path.extension().is_some_and(|x| x.eq_ignore_ascii_case(e))));
    let duration_ms = v["format"]["duration"].as_str().and_then(|d| d.parse::<f64>().ok()).map_or(0, |d| (d * 1000.0) as u64);
    if video.is_none() && !audio {
        return Err(MediaError::Unreadable(path.into()));
    }
    Ok(Contents {
        video: video.is_some(),
        audio,
        still,
        width: video.and_then(|s| s["width"].as_u64()).unwrap_or(0) as u32,
        height: video.and_then(|s| s["height"].as_u64()).unwrap_or(0) as u32,
        duration_ms,
    })
}

fn scale_filter(fps: f64, size: (u32, u32)) -> String {
    format!("fps={fps},scale={}:{}:flags=bicubic", size.0, size.1)
}

/// One frame at `t_ms`, as RGBA at `size` (scrubbing).
pub fn frame_at(path: &Path, t_ms: u64, size: (u32, u32)) -> Result<Vec<u8>, MediaError> {
    let out = Command::new(ffmpeg()?)
        .args(["-v", "error", "-ss", &format!("{:.3}", t_ms as f64 / 1000.0), "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale={}:{}", size.0, size.1), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .output()?;
    let want = (size.0 * size.1 * 4) as usize;
    if out.stdout.len() < want {
        return Err(MediaError::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(out.stdout[..want].to_vec())
}

/// A video read frame by frame from `start_ms`, at `fps`, scaled to `size`.
pub struct Decoder {
    child: Child,
    out: ChildStdout,
    size: (u32, u32),
    fps: f64,
    start_ms: u64,
    /// Frames read so far.
    pub read: u64,
}

impl Decoder {
    pub fn open(path: &Path, start_ms: u64, fps: f64, size: (u32, u32)) -> Result<Decoder, MediaError> {
        let mut child = Command::new(ffmpeg()?)
            .args(["-v", "error", "-hwaccel", "auto", "-ss", &format!("{:.3}", start_ms as f64 / 1000.0), "-i"])
            .arg(path)
            .args(["-an", "-vf", &scale_filter(fps, size), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out = child.stdout.take().ok_or(MediaError::NoFfmpeg)?;
        Ok(Decoder { child, out, size, fps, start_ms, read: 0 })
    }

    /// The source time of the next frame.
    pub fn next_ms(&self) -> u64 {
        self.start_ms + (self.read as f64 * 1000.0 / self.fps) as u64
    }

    /// The next frame into `buf` (RGBA, `size`); false at the end.
    pub fn next(&mut self, buf: &mut Vec<u8>) -> bool {
        buf.resize((self.size.0 * self.size.1 * 4) as usize, 0);
        let ok = self.out.read_exact(buf).is_ok();
        if ok {
            self.read += 1;
        }
        ok
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the result is encoded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 in MP4: hardware when there is one.
    H264,
    /// ProRes 422 in MOV (VideoToolbox's on a Mac, FFmpeg's own elsewhere).
    ProRes,
    /// An animated GIF: 15 frames a second, at most 960 wide, no sound.
    Gif,
}

/// FFmpeg's encoders, once.
fn encoders() -> &'static Vec<String> {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let Ok(ff) = ffmpeg() else { return Vec::new() };
        let Ok(out) = Command::new(ff).args(["-hide_banner", "-encoders"]).output() else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_whitespace().nth(1).map(String::from)).collect()
    })
}

/// The FFmpeg encoder and its options for a codec, best first.
fn video_args(codec: Codec, size: (u32, u32), fps: f64) -> Vec<String> {
    let has = |e: &str| encoders().iter().any(|x| x == e);
    let kbps = ((size.0 as f64 * size.1 as f64 * fps) / 30_000.0).clamp(4_000.0, 60_000.0) as u32;
    let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match codec {
        Codec::ProRes if has("prores_videotoolbox") => v(&["-c:v", "prores_videotoolbox", "-profile:v", "2", "-pix_fmt", "p210le"]),
        Codec::ProRes => v(&["-c:v", "prores_ks", "-profile:v", "2", "-pix_fmt", "yuv422p10le"]),
        Codec::Gif => v(&["-loop", "0"]),
        Codec::H264 => {
            let enc = ["h264_videotoolbox", "h264_nvenc", "h264_amf", "h264_qsv"].into_iter().find(|e| has(e)).unwrap_or("libx264");
            let mut a = v(&["-c:v", enc, "-b:v", &format!("{kbps}k"), "-pix_fmt", "yuv420p"]);
            if enc == "libx264" {
                a.extend(v(&["-preset", "medium"]));
            }
            a
        }
    }
}

/// The audio filter that plays `inputs` audio streams (inputs 1..) through the clips: each cut
/// and sped like the video, then mixed.
fn audio_filter(inputs: usize, clips: &[Clip]) -> Option<String> {
    if inputs == 0 || clips.is_empty() {
        return None;
    }
    let mut f = String::new();
    for i in 0..inputs {
        for (k, c) in clips.iter().enumerate() {
            // atempo takes 0.5..100; Lightair's speeds stay within 0.25..16 (two stages)
            let s = c.speed.clamp(0.25, 16.0);
            let tempo = if s < 0.5 { format!("atempo={},atempo=0.5", s / 0.5) } else { format!("atempo={s}") };
            f.push_str(&format!(
                "[{}:a]atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS,{tempo}[a{i}c{k}];",
                i + 1,
                c.start_ms as f64 / 1000.0,
                c.end_ms as f64 / 1000.0
            ));
        }
        for k in 0..clips.len() {
            f.push_str(&format!("[a{i}c{k}]"));
        }
        f.push_str(&format!("concat=n={}:v=0:a=1[a{i}];", clips.len()));
    }
    for i in 0..inputs {
        f.push_str(&format!("[a{i}]"));
    }
    f.push_str(&format!("amix=inputs={inputs}:normalize=0:dropout_transition=0[aout]"));
    Some(f)
}

/// Writes frames (RGBA, `size`, at `fps`) and the audio of `audio` through `clips` into `out`.
pub struct Encoder {
    child: Child,
    input: Option<ChildStdin>,
}

impl Encoder {
    /// `subtitles`: a SubRip file, added as a subtitle track (MP4 and MOV).
    pub fn start(
        out: &Path,
        size: (u32, u32),
        fps: f64,
        codec: Codec,
        audio: &[PathBuf],
        clips: &[Clip],
        subtitles: Option<&Path>,
    ) -> Result<Encoder, MediaError> {
        Self::start_with(out, size, fps, codec, audio, clips, subtitles, None)
    }

    /// As `start`, with the sound made by `mix` (an FFmpeg filter over inputs 1.., ending in
    /// `[aout]`) instead of the edit's cuts.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with(
        out: &Path,
        size: (u32, u32),
        fps: f64,
        codec: Codec,
        audio: &[PathBuf],
        clips: &[Clip],
        subtitles: Option<&Path>,
        mix: Option<&str>,
    ) -> Result<Encoder, MediaError> {
        let mut cmd = Command::new(ffmpeg()?);
        cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{}x{}", size.0, size.1)])
            .args(["-r", &format!("{fps}"), "-i", "-"]);
        if codec == Codec::Gif {
            // one palette for the whole clip, made from its own colours
            cmd.args([
                "-filter_complex",
                "[0:v]fps=15,scale='min(960,iw)':-2:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4",
            ]);
        } else {
            for a in audio {
                cmd.arg("-i").arg(a);
            }
            if let Some(subs) = subtitles {
                cmd.arg("-i").arg(subs);
            }
            match mix.map(String::from).or_else(|| audio_filter(audio.len(), clips)) {
                Some(f) => cmd.args(["-filter_complex", &f, "-map", "0:v", "-map", "[aout]", "-c:a", "aac", "-b:a", "192k"]),
                None => cmd.args(["-map", "0:v"]),
            };
            if subtitles.is_some() {
                cmd.args(["-map", &format!("{}:s", audio.len() + 1), "-c:s", "mov_text"]);
            }
        }
        cmd.args(video_args(codec, size, fps));
        if codec == Codec::H264 {
            cmd.args(["-movflags", "+faststart"]);
        }
        let mut child = cmd.arg(out).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
        let input = child.stdin.take();
        Ok(Encoder { child, input })
    }

    pub fn write(&mut self, rgba: &[u8]) -> Result<(), MediaError> {
        let Some(i) = &mut self.input else { return Ok(()) };
        i.write_all(rgba).map_err(|e| MediaError::Failed(format!("the encoder stopped taking frames ({e})")))
    }

    /// Ends the file; FFmpeg's complaint if it failed.
    pub fn finish(mut self) -> Result<(), MediaError> {
        drop(self.input.take());
        let out = self.child.wait_with_output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(MediaError::Failed(String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("").to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_follows_the_cuts_and_speed() {
        let clips = [Clip { start_ms: 0, end_ms: 4_000, speed: 1.0 }, Clip { start_ms: 6_000, end_ms: 10_000, speed: 2.0 }];
        let f = audio_filter(2, &clips).unwrap();
        assert!(f.contains("[1:a]atrim=start=0.000:end=4.000"));
        assert!(f.contains("[2:a]atrim=start=6.000:end=10.000,asetpts=PTS-STARTPTS,atempo=2"));
        assert!(f.contains("concat=n=2:v=0:a=1[a1]"));
        assert!(f.ends_with("amix=inputs=2:normalize=0:dropout_transition=0[aout]"));
        assert!(audio_filter(0, &clips).is_none());
    }
}
