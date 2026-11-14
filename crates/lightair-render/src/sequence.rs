//! A sequence (docs/plans/video-editor.md): the Editor's multi-track document. Media, video and
//! audio tracks, and clips placed on them. Times are milliseconds on the timeline unless a name
//! says `in`/`out` (the medium's own time).

use std::path::{Path, PathBuf};

use lightair_core::Id;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

/// A file the sequence uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub id: Id,
    pub name: String,
    pub kind: MediaKind,
    /// The picture or sound (a take: its screen video).
    pub path: PathBuf,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    /// Its own sound (videos), or the files that play with it (a take's stems).
    #[serde(default)]
    pub audio: Vec<PathBuf>,
    /// A Studio take's folder: drawn with its screen recording look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<PathBuf>,
    /// The Library item it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Id>,
}

impl Media {
    /// A file as a medium: what it holds, its size and length (from FFmpeg).
    pub fn from_file(path: &Path) -> Result<Media, crate::media::MediaError> {
        let c = crate::media::contents(path)?;
        let kind = if c.still {
            MediaKind::Image
        } else if c.video {
            MediaKind::Video
        } else {
            MediaKind::Audio
        };
        Ok(Media {
            id: Id::new(),
            name: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            kind,
            path: path.to_path_buf(),
            duration_ms: c.duration_ms,
            width: c.width,
            height: c.height,
            audio: if c.audio && kind == MediaKind::Video { vec![path.to_path_buf()] } else { Vec::new() },
            take: None,
            item: None,
        })
    }

    /// A Studio take folder as a medium: its screen, drawn with its look, and its stems.
    pub fn from_take(folder: &Path) -> Option<Media> {
        let source = crate::export::Source::of_take(folder)?;
        let c = crate::media::contents(&source.video).ok()?;
        Some(Media {
            id: Id::new(),
            name: folder.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            kind: MediaKind::Video,
            path: source.video,
            duration_ms: c.duration_ms,
            width: c.width,
            height: c.height,
            audio: source.audio,
            take: Some(folder.to_path_buf()),
            item: None,
        })
    }
}

/// Where a picture sits: its centre's offset from the frame's centre (result pixels), its scale
/// (1: as large as fits the frame), its turn (degrees) and opacity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub rotation: f32,
    pub opacity: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform { x: 0.0, y: 0.0, scale: 1.0, rotation: 0.0, opacity: 1.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: Id,
    pub media: Id,
    /// Where it starts on the timeline.
    pub start_ms: u64,
    /// The piece of the medium it plays.
    pub in_ms: u64,
    pub out_ms: u64,
    pub speed: f32,
    /// 1: as recorded.
    pub volume: f32,
    pub muted: bool,
    #[serde(default)]
    pub transform: Transform,
}

impl Clip {
    /// Its length on the timeline.
    pub fn len_ms(&self) -> u64 {
        ((self.out_ms.saturating_sub(self.in_ms)) as f64 / self.speed.max(0.05) as f64).round() as u64
    }

    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.len_ms()
    }

    /// The medium's time at timeline time `t` (`None` outside the clip).
    pub fn source_at(&self, t: u64) -> Option<u64> {
        (t >= self.start_ms && t < self.end_ms())
            .then(|| (self.in_ms + ((t - self.start_ms) as f64 * self.speed.max(0.05) as f64) as u64).min(self.out_ms.saturating_sub(1)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Id,
    pub kind: TrackKind,
    /// Clips in time order, never overlapping.
    pub clips: Vec<Clip>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub locked: bool,
}

impl Track {
    fn new(kind: TrackKind) -> Track {
        Track { id: Id::new(), kind, clips: Vec::new(), muted: false, hidden: false, locked: false }
    }

    /// Whether `start..end` is free on this track (ignoring `except`).
    pub fn free(&self, start: u64, end: u64, except: Option<Id>) -> bool {
        self.clips.iter().filter(|c| Some(c.id) != except).all(|c| end <= c.start_ms || start >= c.end_ms())
    }

    fn sort(&mut self) {
        self.clips.sort_by_key(|c| c.start_ms);
    }
}

/// The sequence. Tracks are in display order: video tracks first, the top one in front; then
/// audio tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub version: u32,
    pub size: (u32, u32),
    pub fps: u32,
    pub media: Vec<Media>,
    pub tracks: Vec<Track>,
}

impl Sequence {
    /// Empty: one video track and one audio track.
    pub fn new(size: (u32, u32), fps: u32) -> Sequence {
        Sequence { version: 1, size, fps, media: Vec::new(), tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)] }
    }

    pub fn load(path: &Path) -> std::io::Result<Sequence> {
        serde_json::from_slice(&std::fs::read(path)?).map_err(std::io::Error::other)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn duration_ms(&self) -> u64 {
        self.tracks.iter().flat_map(|t| t.clips.iter().map(Clip::end_ms)).max().unwrap_or(0)
    }

    pub fn media(&self, id: Id) -> Option<&Media> {
        self.media.iter().find(|m| m.id == id)
    }

    /// Adds a medium (once per file); its id.
    pub fn add_media(&mut self, m: Media) -> Id {
        if let Some(e) = self.media.iter().find(|e| e.path == m.path) {
            return e.id;
        }
        let id = m.id;
        self.media.push(m);
        id
    }

    /// (track, clip) indices of a clip.
    pub fn find(&self, clip: Id) -> Option<(usize, usize)> {
        self.tracks.iter().enumerate().find_map(|(t, tr)| tr.clips.iter().position(|c| c.id == clip).map(|c| (t, c)))
    }

    pub fn clip(&self, id: Id) -> Option<&Clip> {
        self.find(id).map(|(t, c)| &self.tracks[t].clips[c])
    }

    pub fn clip_mut(&mut self, id: Id) -> Option<&mut Clip> {
        self.find(id).map(|(t, c)| &mut self.tracks[t].clips[c])
    }

    fn kind_for(m: &Media) -> TrackKind {
        if m.kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video }
    }

    /// A track of `kind` free over `start..end`, the preferred one first; a new one if none is
    /// (video tracks go above the others, audio tracks below).
    fn free_track(&mut self, kind: TrackKind, preferred: Option<usize>, start: u64, end: u64) -> usize {
        if let Some(p) = preferred.filter(|p| self.tracks.get(*p).is_some_and(|t| t.kind == kind && t.free(start, end, None))) {
            return p;
        }
        // the bottom-most free video track (the main one), the top-most free audio track
        let found = match kind {
            TrackKind::Video => self.tracks.iter().rposition(|t| t.kind == kind && t.free(start, end, None)),
            TrackKind::Audio => self.tracks.iter().position(|t| t.kind == kind && t.free(start, end, None)),
        };
        if let Some(i) = found {
            return i;
        }
        match kind {
            TrackKind::Video => {
                self.tracks.insert(0, Track::new(TrackKind::Video));
                0
            }
            TrackKind::Audio => {
                self.tracks.push(Track::new(TrackKind::Audio));
                self.tracks.len() - 1
            }
        }
    }

    /// Places all of a medium at `start` (on `track` if it's free there); the new clip's id.
    pub fn place(&mut self, media: Id, start: u64, track: Option<usize>) -> Option<Id> {
        let m = self.media(media)?.clone();
        let len = if m.kind == MediaKind::Image { 5_000 } else { m.duration_ms.max(1) };
        let clip = Clip {
            id: Id::new(),
            media,
            start_ms: start,
            in_ms: 0,
            out_ms: len,
            speed: 1.0,
            volume: 1.0,
            muted: false,
            transform: Transform::default(),
        };
        let t = self.free_track(Self::kind_for(&m), track, start, start + len);
        let id = clip.id;
        self.tracks[t].clips.push(clip);
        self.tracks[t].sort();
        Some(id)
    }

    /// Places a medium at the end of the main track (the bottom video track, or the first audio
    /// track for sound).
    pub fn append(&mut self, media: Id) -> Option<Id> {
        let kind = Self::kind_for(self.media(media)?);
        let t = match kind {
            TrackKind::Video => self.tracks.iter().rposition(|t| t.kind == kind),
            TrackKind::Audio => self.tracks.iter().position(|t| t.kind == kind),
        };
        let end = t.and_then(|t| self.tracks[t].clips.last().map(Clip::end_ms)).unwrap_or(0);
        self.place(media, end, t)
    }

    /// Moves a clip to `start` on `track` (same kind). False (and nothing moves) if it would
    /// overlap another clip there.
    pub fn move_clip(&mut self, clip: Id, track: usize, start: u64) -> bool {
        let Some((t, c)) = self.find(clip) else { return false };
        let Some(dest) = self.tracks.get(track) else { return false };
        let len = self.tracks[t].clips[c].len_ms();
        if dest.kind != self.tracks[t].kind || dest.locked || !dest.free(start, start + len, Some(clip)) {
            return false;
        }
        let mut moved = self.tracks[t].clips.remove(c);
        moved.start_ms = start;
        self.tracks[track].clips.push(moved);
        self.tracks[track].sort();
        true
    }

    /// Moves a clip's left edge to timeline time `at` (its in point follows), within its medium
    /// and its neighbours.
    pub fn trim_start(&mut self, clip: Id, at: u64) {
        let Some((t, c)) = self.find(clip) else { return };
        let prev_end = if c > 0 { self.tracks[t].clips[c - 1].end_ms() } else { 0 };
        let cl = &mut self.tracks[t].clips[c];
        let speed = cl.speed.max(0.05) as f64;
        // the earliest the edge can go: the medium's start, the clip before
        let earliest = cl.start_ms.saturating_sub((cl.in_ms as f64 / speed) as u64).max(prev_end);
        let at = at.clamp(earliest, cl.end_ms().saturating_sub(100));
        let in_ms = cl.in_ms as i64 + ((at as i64 - cl.start_ms as i64) as f64 * speed) as i64;
        cl.in_ms = in_ms.max(0) as u64;
        cl.start_ms = at;
    }

    /// Moves a clip's right edge to timeline time `at` (its out point follows), within its medium
    /// (images stretch freely) and the clip after.
    pub fn trim_end(&mut self, clip: Id, at: u64) {
        let Some((t, c)) = self.find(clip) else { return };
        let next_start = self.tracks[t].clips.get(c + 1).map_or(u64::MAX, |n| n.start_ms);
        let media = self.tracks[t].clips[c].media;
        let (limit, image) = self.media(media).map_or((u64::MAX, false), |m| (m.duration_ms, m.kind == MediaKind::Image));
        let cl = &mut self.tracks[t].clips[c];
        let speed = cl.speed.max(0.05) as f64;
        let at = at.clamp(cl.start_ms + 100, next_start);
        let out = cl.in_ms + ((at - cl.start_ms) as f64 * speed) as u64;
        cl.out_ms = if image { out } else { out.min(limit) };
    }

    /// Cuts a clip at timeline time `at`: the second half becomes a clip of its own (its id).
    pub fn split(&mut self, clip: Id, at: u64) -> Option<Id> {
        let (t, c) = self.find(clip)?;
        let cl = self.tracks[t].clips[c].clone();
        let s = cl.source_at(at).filter(|_| at > cl.start_ms)?;
        self.tracks[t].clips[c].out_ms = s;
        let second = Clip { id: Id::new(), start_ms: at, in_ms: s, ..cl };
        let id = second.id;
        self.tracks[t].clips.insert(c + 1, second);
        Some(id)
    }

    /// Cuts every unlocked clip under the playhead (CapCut's split with nothing selected).
    pub fn split_all(&mut self, at: u64) -> Vec<Id> {
        let under: Vec<Id> = self
            .tracks
            .iter()
            .filter(|t| !t.locked)
            .flat_map(|t| t.clips.iter().filter(|c| c.start_ms < at && at < c.end_ms()).map(|c| c.id))
            .collect();
        under.into_iter().filter_map(|c| self.split(c, at)).collect()
    }

    pub fn remove(&mut self, clip: Id) {
        if let Some((t, c)) = self.find(clip) {
            self.tracks[t].clips.remove(c);
        }
    }

    /// Removes empty tracks beyond one of each kind.
    pub fn tidy(&mut self) {
        for kind in [TrackKind::Video, TrackKind::Audio] {
            while self.tracks.iter().filter(|t| t.kind == kind).count() > 1 {
                let Some(i) = self.tracks.iter().position(|t| t.kind == kind && t.clips.is_empty()) else { break };
                self.tracks.remove(i);
            }
        }
    }

    /// Times things snap to: every clip's edges, and the start.
    pub fn snap_points(&self, except: Option<Id>) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().filter(|c| Some(c.id) != except).flat_map(|c| [c.start_ms, c.end_ms()]))
            .collect();
        v.push(0);
        v.sort();
        v.dedup();
        v
    }

    /// The video clips showing at `t`, back to front, with their track.
    pub fn video_at(&self, t: u64) -> Vec<(usize, &Clip)> {
        let mut out: Vec<(usize, &Clip)> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Video && !tr.hidden)
            .filter_map(|(i, tr)| tr.clips.iter().find(|c| c.start_ms <= t && t < c.end_ms()).map(|c| (i, c)))
            .collect();
        // the top track (index 0) is in front: drawn last
        out.reverse();
        out
    }
}

/// An Editor project (docs/plans/video-editor.md): a named sequence, one file in the workspace's
/// `edits` folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditProject {
    pub id: Id,
    pub name: String,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub sequence: Sequence,
}

/// A project as the list shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectInfo {
    pub id: Id,
    pub name: String,
    pub modified_ms: i64,
    pub duration_ms: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl EditProject {
    pub fn new(name: &str, size: (u32, u32), fps: u32) -> EditProject {
        let now = now_ms();
        EditProject { id: Id::new(), name: name.into(), created_ms: now, modified_ms: now, sequence: Sequence::new(size, fps) }
    }

    pub fn dir(workspace: &Path) -> PathBuf {
        workspace.join("edits")
    }

    fn path(workspace: &Path, id: Id) -> PathBuf {
        Self::dir(workspace).join(format!("{id}.json"))
    }

    /// The workspace's projects, the latest changed first.
    pub fn list(workspace: &Path) -> Vec<ProjectInfo> {
        let mut out: Vec<ProjectInfo> = std::fs::read_dir(Self::dir(workspace))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice::<EditProject>(&std::fs::read(e.path()).ok()?).ok())
            .map(|p| ProjectInfo { id: p.id, name: p.name.clone(), modified_ms: p.modified_ms, duration_ms: p.sequence.duration_ms() })
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.modified_ms));
        out
    }

    pub fn load(workspace: &Path, id: Id) -> std::io::Result<EditProject> {
        serde_json::from_slice(&std::fs::read(Self::path(workspace, id))?).map_err(std::io::Error::other)
    }

    /// Saves it (whole, through a temporary file), marked changed now.
    pub fn save(&mut self, workspace: &Path) -> std::io::Result<()> {
        self.modified_ms = now_ms();
        let path = Self::path(workspace, self.id);
        std::fs::create_dir_all(Self::dir(workspace))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Removes the project (its file; the media it uses stay).
    pub fn delete(workspace: &Path, id: Id) -> std::io::Result<()> {
        std::fs::remove_file(Self::path(workspace, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_files_in_the_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let mut a = EditProject::new("Intro", (1920, 1080), 30);
        a.save(ws.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut b = EditProject::new("Tutorial", (1080, 1920), 60);
        b.save(ws.path()).unwrap();
        let list = EditProject::list(ws.path());
        assert_eq!(list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Tutorial", "Intro"], "latest first");
        assert_eq!(EditProject::load(ws.path(), a.id).unwrap().name, "Intro");
        EditProject::delete(ws.path(), a.id).unwrap();
        assert_eq!(EditProject::list(ws.path()).len(), 1);
    }

    fn video(seq: &mut Sequence, name: &str, ms: u64) -> Id {
        seq.add_media(Media {
            id: Id::new(),
            name: name.into(),
            kind: MediaKind::Video,
            path: PathBuf::from(format!("/m/{name}.mp4")),
            duration_ms: ms,
            width: 1920,
            height: 1080,
            audio: vec![PathBuf::from(format!("/m/{name}.mp4"))],
            take: None,
            item: None,
        })
    }

    #[test]
    fn clips_go_end_to_end_and_overlaps_go_up_a_track() {
        let mut s = Sequence::new((1920, 1080), 30);
        let a = video(&mut s, "a", 10_000);
        let b = video(&mut s, "b", 4_000);
        let ca = s.append(a).unwrap();
        let cb = s.append(b).unwrap();
        assert_eq!(s.clip(cb).unwrap().start_ms, 10_000, "after the first");
        assert_eq!(s.duration_ms(), 14_000);
        // placed over the first clip: a new track above it
        let over = s.place(b, 2_000, None).unwrap();
        let (t_over, _) = s.find(over).unwrap();
        let (t_main, _) = s.find(ca).unwrap();
        assert!(t_over < t_main, "above (in front)");
        let shown: Vec<Id> = s.video_at(3_000).iter().map(|(_, c)| c.id).collect();
        assert_eq!(shown, vec![ca, over], "back to front");
        // a move onto an occupied place is refused
        assert!(!s.move_clip(over, t_main, 1_000));
        assert!(s.move_clip(over, t_main, 14_000));
        s.tidy();
        assert_eq!(s.tracks.iter().filter(|t| t.kind == TrackKind::Video).count(), 1);
    }

    #[test]
    fn trims_and_splits_keep_the_medium_in_step() {
        let mut s = Sequence::new((1920, 1080), 30);
        let a = video(&mut s, "a", 10_000);
        let c = s.append(a).unwrap();
        s.trim_start(c, 2_000);
        let cl = s.clip(c).unwrap().clone();
        assert_eq!((cl.start_ms, cl.in_ms, cl.out_ms), (2_000, 2_000, 10_000));
        s.trim_end(c, 50_000);
        assert_eq!(s.clip(c).unwrap().out_ms, 10_000, "not past the medium's end");
        s.clip_mut(c).unwrap().speed = 2.0;
        assert_eq!(s.clip(c).unwrap().len_ms(), 4_000);
        let second = s.split(c, 4_000).unwrap();
        let (first, second) = (s.clip(c).unwrap().clone(), s.clip(second).unwrap().clone());
        assert_eq!((first.in_ms, first.out_ms), (2_000, 6_000));
        assert_eq!((second.start_ms, second.in_ms, second.out_ms), (4_000, 6_000, 10_000));
        assert_eq!(second.source_at(5_000), Some(8_000));
        assert!(s.snap_points(None).contains(&4_000));
    }
}
