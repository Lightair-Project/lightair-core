//! What widgets keep between runs (a counter's value, a subathon's end): one JSON file per
//! widget.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

fn file(dir: &Path, id: &str) -> PathBuf {
    // ids are Lightair's own (hex), but never a path
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    dir.join(format!("{safe}.json"))
}

pub fn load(dir: &Path, id: &str) -> Map<String, Value> {
    std::fs::read(file(dir, id)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Sets `key` (null removes it); everything stored now.
pub fn set(dir: &Path, id: &str, key: &str, value: Value) -> Map<String, Value> {
    let mut all = load(dir, id);
    if value.is_null() {
        all.remove(key);
    } else {
        all.insert(key.into(), value);
    }
    let _ = std::fs::create_dir_all(dir);
    let path = file(dir, id);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&all).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    all
}
