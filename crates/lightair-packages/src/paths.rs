//! Paths inside packages: checking they stay inside, skipping system clutter, and small file
//! helpers (unique temporary names, writing a file in one step).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Names Windows can't give a file, whatever its extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `path` as a plain relative path inside a package (`/`-separated, no `.` or empty parts), or
/// why it isn't one: absolute, a drive, `..`, a control character, a name Windows can't store,
/// or too long or too deep for `max_len` and `max_depth`. Backslashes are taken as separators.
pub(crate) fn clean_path(path: &str, max_len: usize, max_depth: usize) -> Result<String, String> {
    if path.is_empty() {
        return Err("is empty".into());
    }
    let unified = path.replace('\\', "/");
    if unified.starts_with('/') {
        return Err("is absolute; paths in a package are relative to its folder".into());
    }
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err("names a drive; paths in a package are relative to its folder".into());
    }
    let mut parts = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err("goes outside the package".into()),
            _ => {}
        }
        if part.chars().any(|c| c.is_control()) {
            return Err("holds a control character".into());
        }
        if part.contains(':') || part.contains(['<', '>', '"', '|', '?', '*']) {
            return Err("holds a character some systems can't store (: < > \" | ? *)".into());
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err("has a name ending in a dot or a space, which Windows can't store".into());
        }
        let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Err(format!("uses the name {stem}, which Windows reserves"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err("is empty".into());
    }
    if parts.len() > max_depth {
        return Err(format!("is nested more than {max_depth} folders deep"));
    }
    let clean = parts.join("/");
    if clean.len() > max_len {
        return Err(format!("is longer than {max_len} characters"));
    }
    Ok(clean)
}

/// Whether a file or folder name is clutter that's never part of a package: hidden names
/// (starting with a dot: `.DS_Store`, `.git`, macOS's `._` files, the store's own install
/// record), macOS's `__MACOSX` folder in zips, and Windows' `Thumbs.db` and `desktop.ini`.
pub(crate) fn is_junk_name(name: &str) -> bool {
    name.starts_with('.')
        || name == "__MACOSX"
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

/// Whether any part of a clean relative path is clutter ([`is_junk_name`]).
pub(crate) fn is_junk_path(path: &str) -> bool {
    path.split('/').any(is_junk_name)
}

/// A name made safe for a package id part or a folder: lower-case ASCII letters and digits, with
/// a hyphen for each run of anything else, trimmed, at most 48 characters; "item" if nothing is
/// left.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "item".into() } else { out }
}

/// A name no other temporary file or folder made by this process has: `prefix`, the process
/// id, the time and a counter.
pub(crate) fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{}-{nanos:x}-{n}", std::process::id())
}

/// Writes `bytes` to `path` through a temporary file beside it and a rename, so a reader never
/// sees half a file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| Error::io("make the folder", dir, e))?;
    let tmp = dir.join(unique_name(".tmp-"));
    fs::write(&tmp, bytes).map_err(|e| Error::io("write", &tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io("write", path, e)
    })
}

/// `rel` (a clean `/`-separated path) under `root`.
pub(crate) fn join(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(rel.split('/'));
    p
}

/// The seconds since 1970 now.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
