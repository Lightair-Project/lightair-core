//! A workspace's recordings index (docs/plans/screen-studio.md, "Library"): every recording, clip,
//! multitrack set, studio_take and export the workspace made, at `<workspace>/recordings/index.json`.
//!
//! The media stays where it was recorded; the index points at it. A file that has moved shows
//! as missing until it's located again ([`Index::relink`], [`Index::locate_in`]). Nothing here
//! deletes media: taking an item away moves its files to the Trash ([`Index::trash`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use lightair_model::Id;
use serde::{Deserialize, Serialize};

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The program, one file per canvas.
    Recording,
    /// A studio take: raw tracks and source data (docs/plans/screen-studio.md, 4.2).
    #[serde(alias = "take")]
    StudioTake,
    /// A replay clip.
    Clip,
    /// Multitrack audio files.
    Multitrack,
    /// Made in the Editor.
    Export,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Recording, Kind::StudioTake, Kind::Clip, Kind::Multitrack, Kind::Export];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Recording => "Recording",
            Kind::StudioTake => "Studio take",
            Kind::Clip => "Clip",
            Kind::Multitrack => "Multitrack audio",
            Kind::Export => "Export",
        }
    }
}

/// One file of an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct File {
    pub path: PathBuf,
    /// The canvas it holds ("Landscape", "Vertical"), for a recording of several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "format")]
    pub canvas: Option<String>,
    /// What a studio_take recorded into it: "Screen", "Camera"…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
}

impl File {
    pub fn new(path: PathBuf) -> File {
        File { path, canvas: None, track: None }
    }
}

/// A marker: when (from the start, ms) and what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub at_ms: u64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: Id,
    pub kind: Kind,
    pub title: String,
    pub files: Vec<File>,
    /// When it started, Unix time in ms.
    pub started_ms: i64,
    /// Its length, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<Marker>,
    /// A still, in the workspace's `recordings/thumbnails`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<PathBuf>,
    /// For an export: the item it was made from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Id>,
}

impl Item {
    pub fn new(kind: Kind, title: impl Into<String>, files: Vec<File>, started_ms: i64) -> Self {
        Item {
            id: Id::new(),
            kind,
            title: title.into(),
            files,
            started_ms,
            duration_ms: None,
            markers: Vec::new(),
            thumbnail: None,
            from: None,
        }
    }

    /// Its files that aren't where the index says.
    pub fn missing(&self) -> Vec<&Path> {
        self.files.iter().map(|f| f.path.as_path()).filter(|p| !p.exists()).collect()
    }

    /// Bytes on disk (files that exist).
    pub fn bytes(&self) -> u64 {
        self.files.iter().filter_map(|f| fs::metadata(&f.path).ok()).map(|m| m.len()).sum()
    }

    /// Whether `query` is in its title, a file name or a marker (any case).
    pub fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        q.is_empty()
            || self.title.to_lowercase().contains(&q)
            || self.markers.iter().any(|m| m.label.to_lowercase().contains(&q))
            || self.files.iter().any(|f| f.path.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().contains(&q)))
    }
}

/// A workspace's index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub items: Vec<Item>,
}

/// The index file of the workspace package at `workspace`.
pub fn index_path(workspace: &Path) -> PathBuf {
    workspace.join("recordings").join("index.json")
}

/// Where a workspace's thumbnails go.
pub fn thumbnails_dir(workspace: &Path) -> PathBuf {
    workspace.join("recordings").join("thumbnails")
}

/// Video and audio files Lightair can list (Add existing recordings).
pub const MEDIA_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv", "m4v", "flv", "ts", "wav", "m4a", "mp3", "aac", "flac"];

fn is_media(path: &Path) -> bool {
    path.extension().is_some_and(|e| MEDIA_EXTENSIONS.iter().any(|m| e.eq_ignore_ascii_case(m)))
}

impl Index {
    /// The workspace's index (empty when it has none yet).
    pub fn load(workspace: &Path) -> Index {
        fs::read(index_path(workspace)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Writes it (whole, through a temporary file, so a crash never leaves half an index).
    pub fn save(&self, workspace: &Path) -> io::Result<()> {
        let path = index_path(workspace);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
        fs::rename(tmp, path)
    }

    /// Adds an item to the workspace's index on disk (read, add, write: other windows may have
    /// added items since this one loaded).
    pub fn add_to(workspace: &Path, item: Item) -> io::Result<()> {
        let mut index = Index::load(workspace);
        index.items.retain(|i| i.id != item.id);
        index.items.push(item);
        index.save(workspace)
    }

    pub fn get(&self, id: Id) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn get_mut(&mut self, id: Id) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Newest first.
    pub fn newest_first(&self) -> Vec<&Item> {
        let mut v: Vec<&Item> = self.items.iter().collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.started_ms));
        v
    }

    /// Whether some item has this file.
    pub fn has_file(&self, path: &Path) -> bool {
        self.items.iter().any(|i| i.files.iter().any(|f| f.path == path))
    }

    /// Points an item's file at where it is now. Returns whether the item had it.
    pub fn relink(&mut self, id: Id, old: &Path, new: &Path) -> bool {
        let Some(item) = self.get_mut(id) else { return false };
        let Some(file) = item.files.iter_mut().find(|f| f.path == old) else { return false };
        file.path = new.to_path_buf();
        true
    }

    /// Finds missing files in `folder` (and its subfolders, two levels) by name, and points the
    /// index at them. Returns how many were found.
    pub fn locate_in(&mut self, folder: &Path) -> usize {
        let mut found = std::collections::HashMap::new();
        collect(folder, 2, &mut found);
        let mut n = 0;
        for item in &mut self.items {
            for file in &mut item.files {
                if file.path.exists() {
                    continue;
                }
                if let Some(name) = file.path.file_name()
                    && let Some(there) = found.get(name)
                {
                    file.path = there.clone();
                    n += 1;
                }
            }
        }
        n
    }

    /// The media in `folder` (not in subfolders) that no item has, as items: one per file,
    /// started when the file was last written. Their lengths are read from MP4 and MOV files.
    pub fn existing_in(&self, folder: &Path) -> Vec<Item> {
        let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            // a take (or a studio_take package from someone else): all its files, as one item
            if path.is_dir() && path.extension().is_some_and(|e| e == "lightair-take") {
                if let Some(item) = studio_take_item(&path).filter(|i| !i.files.iter().any(|f| self.has_file(&f.path))) {
                    out.push(item);
                }
                continue;
            }
            if !path.is_file() || !is_media(&path) || self.has_file(&path) {
                continue;
            }
            let started_ms = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let audio = path.extension().is_some_and(|e| {
                ["wav", "m4a", "mp3", "aac", "flac"].iter().any(|a| e.eq_ignore_ascii_case(a))
            });
            let kind = if audio { Kind::Multitrack } else { Kind::Recording };
            let mut item = Item::new(kind, title, vec![File::new(path.clone())], started_ms);
            item.duration_ms = mp4_duration_ms(&path);
            out.push(item);
        }
        out.sort_by_key(|i| i.started_ms);
        out
    }

    /// Takes an item away: its files go to the Trash through `trash` (they can be put back),
    /// then it leaves the index. If any file can't go to the Trash, the item stays (with the
    /// files that did go marked missing) and the error is returned: nothing is deleted outright.
    pub fn trash(&mut self, id: Id, trash: impl Fn(&Path) -> io::Result<()>) -> io::Result<()> {
        let Some(item) = self.get(id) else { return Ok(()) };
        for file in &item.files {
            if file.path.exists() {
                trash(&file.path)?;
            }
        }
        // its thumbnail is Lightair's own
        if let Some(t) = &item.thumbnail {
            let _ = fs::remove_file(t);
        }
        self.items.retain(|i| i.id != id);
        Ok(())
    }

    /// A copy of an item, its files copied beside the originals ("… copy.mp4"). Returns the
    /// new item (also added).
    pub fn duplicate(&mut self, id: Id) -> io::Result<Item> {
        let item = self.get(id).cloned().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut copy = item.clone();
        copy.id = Id::new();
        copy.title = format!("{} copy", item.title);
        copy.thumbnail = None;
        for file in &mut copy.files {
            let to = free_name(&file.path, "copy");
            fs::copy(&file.path, &to)?;
            file.path = to;
        }
        if let Some(t) = &item.thumbnail {
            let to = t.with_file_name(format!("{}.png", copy.id));
            if fs::copy(t, &to).is_ok() {
                copy.thumbnail = Some(to);
            }
        }
        self.items.push(copy.clone());
        Ok(copy)
    }

    /// Bytes on disk, all items.
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(Item::bytes).sum()
    }
}

/// A studio_take folder as an item: its files, from its studio_take.json.
pub fn studio_take_item(folder: &Path) -> Option<Item> {
    let studio_take: serde_json::Value = serde_json::from_slice(&fs::read(folder.join("take.json")).ok()?).ok()?;
    let mut files: Vec<File> = studio_take["video"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| Some(File { track: v["name"].as_str().map(String::from), ..File::new(folder.join(v["file"].as_str()?)) }))
        .collect();
    files.extend(studio_take["audio"].as_array().into_iter().flatten().filter_map(|a| Some(File::new(folder.join(a.as_str()?)))));
    for extra in ["input.bin", "take.json", "edit.json"] {
        if folder.join(extra).exists() {
            files.push(File::new(folder.join(extra)));
        }
    }
    let title = folder.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut item = Item::new(Kind::StudioTake, title, files, studio_take["started_ms"].as_i64().unwrap_or(0));
    item.duration_ms = studio_take["duration_ms"].as_u64();
    Some(item)
}

/// Copies a studio_take, with its edit, into `to` as a package (a `.lightair-studio_take` folder) to give to
/// someone else or keep. Returns the package.
pub fn export_package(item: &Item, edit: Option<&Path>, to: &Path) -> io::Result<PathBuf> {
    let name = format!("{}.lightair-take", item.title.replace(['/', ':', '\\'], "-"));
    let package = (1..)
        .map(|n| if n == 1 { to.join(&name) } else { to.join(name.replace(".lightair-take", &format!(" {n}.lightair-take"))) })
        .find(|p| !p.exists())
        .unwrap_or_else(|| to.join(&name));
    fs::create_dir_all(&package)?;
    for f in item.files.iter().filter(|f| f.path.exists()) {
        fs::copy(&f.path, package.join(f.path.file_name().unwrap_or_default()))?;
    }
    if let Some(e) = edit.filter(|e| e.exists()) {
        fs::copy(e, package.join("edit.json"))?;
    }
    Ok(package)
}

/// Moves an item from one workspace's index to another's (its thumbnail too). Returns whether
/// `from` had it.
pub fn move_item(from: &Path, to: &Path, id: Id) -> io::Result<bool> {
    let mut source = Index::load(from);
    let Some(pos) = source.items.iter().position(|i| i.id == id) else { return Ok(false) };
    let mut item = source.items.remove(pos);
    if let Some(t) = item.thumbnail.take() {
        let dir = thumbnails_dir(to);
        fs::create_dir_all(&dir)?;
        let new = dir.join(t.file_name().unwrap_or_default());
        if fs::rename(&t, &new).is_ok() || fs::copy(&t, &new).is_ok() {
            item.thumbnail = Some(new);
        }
    }
    Index::add_to(to, item)?;
    source.save(from)?;
    Ok(true)
}

/// "name copy.ext", or "name copy 2.ext"… whichever doesn't exist yet.
fn free_name(path: &Path, word: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|n| {
            let name = if n == 1 { format!("{stem} {word}{ext}") } else { format!("{stem} {word} {n}{ext}") };
            path.with_file_name(name)
        })
        .find(|p| !p.exists())
        .unwrap_or_else(|| path.to_path_buf())
}

fn collect(dir: &Path, depth: u32, out: &mut std::collections::HashMap<std::ffi::OsString, PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect(&path, depth - 1, out);
            }
        } else if let Some(name) = path.file_name() {
            out.entry(name.to_os_string()).or_insert(path);
        }
    }
}

/// The length of an MP4 or MOV file, from its movie header (`moov/mvhd`).
pub fn mp4_duration_ms(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    // top-level boxes until moov
    let mut at = 0u64;
    let moov = loop {
        if at + 8 > len {
            return None;
        }
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut head = [0u8; 16];
        f.read_exact(&mut head[..8]).ok()?;
        let mut size = u32::from_be_bytes(head[..4].try_into().ok()?) as u64;
        let mut header = 8;
        if size == 1 {
            f.read_exact(&mut head[8..16]).ok()?;
            size = u64::from_be_bytes(head[8..16].try_into().ok()?);
            header = 16;
        } else if size == 0 {
            size = len - at;
        }
        if size < header {
            return None;
        }
        if &head[4..8] == b"moov" {
            break (at + header, at + size);
        }
        at += size;
    };
    // mvhd inside moov
    let mut at = moov.0;
    while at + 8 <= moov.1 {
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut head = [0u8; 8];
        f.read_exact(&mut head).ok()?;
        let size = u32::from_be_bytes(head[..4].try_into().ok()?) as u64;
        if size < 8 {
            return None;
        }
        if &head[4..8] == b"mvhd" {
            let mut body = [0u8; 32];
            f.read_exact(&mut body).ok()?;
            let (scale, duration) = if body[0] == 1 {
                // version 1: 64-bit times
                (u32::from_be_bytes(body[20..24].try_into().ok()?) as u64, u64::from_be_bytes(body[24..32].try_into().ok()?))
            } else {
                (u32::from_be_bytes(body[12..16].try_into().ok()?) as u64, u32::from_be_bytes(body[16..20].try_into().ok()?) as u64)
            };
            return (scale > 0).then(|| duration * 1000 / scale);
        }
        at += size;
    }
    None
}
