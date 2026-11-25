//! Recording's files and the Studio index Lightair Editor reads.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The folder recordings go in: `<shared>/Lightair/Studio`.
pub fn folder(shared: &Path) -> PathBuf {
    shared.join("Lightair").join("Studio")
}

/// (y, m, d, H, M) for Unix seconds.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // the days-to-civil date algorithm
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, (rem / 60 % 60) as u32)
}

/// A new recording's file: `<yyyy-mm-dd HH.MM> <project>.mp4`, never an existing one.
pub fn file_name(dir: &Path, project: &str, unix_s: i64, utc_offset_min: i32) -> PathBuf {
    let (y, m, d, h, mi) = civil(unix_s + utc_offset_min as i64 * 60);
    let safe = lightair_model::file_safe(project);
    let base = format!("{y:04}-{m:02}-{d:02} {h:02}.{mi:02} {safe}");
    let mut p = dir.join(format!("{base}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} {n}.mp4"));
        n += 1;
    }
    p
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub name: String,
    pub created_ms: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    /// The file, inside `<shared>/Lightair/`.
    pub file: PathBuf,
}

/// Adds a finished recording to `<shared>/Lightair/Studio/index.json` (written to a temporary
/// file, then renamed). Returns the item's id.
pub fn add_to_index(shared: &Path, item: &Item) -> std::io::Result<String> {
    let index = folder(shared).join("index.json");
    fs::create_dir_all(folder(shared))?;
    let mut doc = read_index(&index)?;
    let id = push(&mut doc, shared, item);
    write_index(&index, &doc)?;
    Ok(id)
}

/// The index as it is: a fresh one only where there's none. One that can't be read is kept
/// beside it (`index.json.broken-<ms>`) and a fresh one started; one of a newer version than
/// this app writes is left alone (an error: nothing is added).
fn read_index(index: &Path) -> std::io::Result<Value> {
    let fresh = || json!({ "version": 1, "items": [] });
    let bytes = match fs::read(index) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(fresh()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) if v["version"] == 1 && v["items"].is_array() => Ok(v),
        Ok(v) if v["version"].as_u64().is_some_and(|n| n > 1) => Err(std::io::Error::other(
            "the Studio index is a newer version than this app writes",
        )),
        _ => {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            let aside = index.with_extension(format!("json.broken-{ms}"));
            tracing::warn!(aside = %aside.display(), "the Studio index couldn't be read: kept aside");
            fs::rename(index, &aside)?;
            Ok(fresh())
        }
    }
}

fn write_index(index: &Path, doc: &Value) -> std::io::Result<()> {
    let tmp = index.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(doc).unwrap_or_default())?;
    fs::rename(&tmp, index)
}

fn push(doc: &mut Value, shared: &Path, item: &Item) -> String {
    let root = shared.join("Lightair");
    let rel = item
        .file
        .strip_prefix(&root)
        .unwrap_or(&item.file)
        .to_string_lossy()
        .replace('\\', "/");
    let id = uuid::Uuid::new_v4().to_string();
    let entry = json!({
        "id": id,
        "kind": "recording",
        "name": item.name,
        "created_ms": item.created_ms,
        "duration_ms": item.duration_ms,
        "width": item.width,
        "height": item.height,
        "file": rel,
        "clips": 0,
    });
    match doc["items"].as_array_mut() {
        Some(items) => items.push(entry),
        None => doc["items"] = json!([entry]),
    }
    id
}

/// Recordings in the folder the index doesn't list (the app was ended mid-recording: iOS's
/// fragmented MP4 plays to its last fragment) go into it, by their file's name, with the size
/// their first video track says and no length (0). Run when the app starts, before any
/// recording. Returns how many were added.
pub fn recover(shared: &Path) -> std::io::Result<usize> {
    let dir = folder(shared);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(0);
    };
    let index = dir.join("index.json");
    let mut doc = read_index(&index)?;
    let listed: std::collections::HashSet<String> = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["file"].as_str().map(str::to_string))
        .collect();
    let root = shared.join("Lightair");
    let mut added = 0;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("mp4") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if listed.contains(&rel) || e.metadata().map_or(true, |m| m.len() == 0) {
            continue;
        }
        let created_ms = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64);
        let (width, height) = video_size(&path).unwrap_or((0, 0));
        let name = path
            .file_stem()
            .map_or(String::new(), |s| s.to_string_lossy().to_string());
        push(
            &mut doc,
            shared,
            &Item {
                name,
                created_ms,
                duration_ms: 0,
                width,
                height,
                file: path.clone(),
            },
        );
        added += 1;
    }
    if added > 0 {
        write_index(&index, &doc)?;
    }
    Ok(added)
}

/// The first track header with a picture size in an MP4's first megabyte (`tkhd`'s last 8
/// bytes: width and height, 16.16 fixed point).
fn video_size(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut head = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(1 << 20)
        .read_to_end(&mut head)
        .ok()?;
    let mut at = 0;
    while let Some(i) = head[at..].windows(4).position(|w| w == b"tkhd") {
        let Some(start) = (at + i).checked_sub(4) else {
            at += i + 4;
            continue;
        };
        let size = u32::from_be_bytes(head.get(start..start + 4)?.try_into().ok()?) as usize;
        if size >= 16
            && let Some(b) = head.get(start + size - 8..start + size)
        {
            let w = u32::from_be_bytes(b[0..4].try_into().ok()?) >> 16;
            let h = u32::from_be_bytes(b[4..8].try_into().ok()?) >> 16;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        at += i + 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_index_is_kept_a_newer_one_left_and_lost_recordings_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = folder(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let index = dir.join("index.json");
        let item = |f: &str| Item {
            name: f.into(),
            created_ms: 1,
            duration_ms: 2,
            width: 9,
            height: 16,
            file: dir.join(f),
        };
        // unreadable: kept aside, a fresh one started
        fs::write(&index, b"{ not json").unwrap();
        add_to_index(tmp.path(), &item("a.mp4")).unwrap();
        let aside = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("broken"))
            .count();
        assert_eq!(aside, 1);
        // a newer version: left alone
        fs::write(&index, br#"{"version": 2, "items": [{"id": "x"}]}"#).unwrap();
        assert!(add_to_index(tmp.path(), &item("b.mp4")).is_err());
        assert!(
            fs::read_to_string(&index)
                .unwrap()
                .contains("\"version\": 2")
        );
        // an MP4 the index doesn't list (an app ended mid-recording) is put back, with its size
        fs::remove_file(&index).unwrap();
        add_to_index(tmp.path(), &item("listed.mp4")).unwrap();
        fs::write(dir.join("listed.mp4"), b"x").unwrap();
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&[0, 0, 0, 20]);
        mp4.extend_from_slice(b"ftyp");
        mp4.extend_from_slice(&[0; 12]);
        let tkhd_len = 92u32;
        mp4.extend_from_slice(&tkhd_len.to_be_bytes());
        mp4.extend_from_slice(b"tkhd");
        mp4.extend_from_slice(&vec![0; (tkhd_len - 16) as usize]);
        mp4.extend_from_slice(&(1080u32 << 16).to_be_bytes());
        mp4.extend_from_slice(&(1920u32 << 16).to_be_bytes());
        fs::write(dir.join("2026-11-21 10.00 Lost.mp4"), &mp4).unwrap();
        assert_eq!(recover(tmp.path()).unwrap(), 1);
        assert_eq!(recover(tmp.path()).unwrap(), 0, "once");
        let doc: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        let lost = &doc["items"][1];
        assert_eq!(
            (
                lost["name"].as_str(),
                lost["width"].as_u64(),
                lost["height"].as_u64()
            ),
            (Some("2026-11-21 10.00 Lost"), Some(1080), Some(1920))
        );
    }

    #[test]
    fn names_and_the_index() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0));
        assert_eq!(civil(1_795_100_000), (2026, 11, 19, 14, 53));
        let tmp = tempfile::tempdir().unwrap();
        let dir = folder(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let f = file_name(&dir, "Tokyo: walk", 1_795_100_000, 9 * 60);
        assert!(
            f.to_string_lossy().contains("2026-11-19 23.53 Tokyo"),
            "{}",
            f.display()
        );
        fs::write(&f, b"x").unwrap();
        assert_ne!(file_name(&dir, "Tokyo: walk", 1_795_100_000, 9 * 60), f);
        let item = Item {
            name: "Tokyo walk".into(),
            created_ms: 1,
            duration_ms: 2,
            width: 1080,
            height: 1920,
            file: f,
        };
        add_to_index(tmp.path(), &item).unwrap();
        add_to_index(tmp.path(), &item).unwrap();
        let doc: Value =
            serde_json::from_slice(&fs::read(dir.join("index.json")).unwrap()).unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["items"].as_array().unwrap().len(), 2);
        assert!(
            doc["items"][0]["file"]
                .as_str()
                .unwrap()
                .starts_with("Studio/2026-11-19 23.53")
        );
    }
}
