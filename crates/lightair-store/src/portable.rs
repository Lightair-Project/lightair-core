//! A portable copy of a workspace (desktop SC-04; Lightair Live's project transfer, MS-01): the
//! workspace as it is now, its media collected into `assets/`, and nothing secret in it.
//!
//! ```text
//! <to>/
//! ├─ workspace.json, sources.json, audio.json, mixer.json, outputs.json, scenes/<id>.json
//! └─ assets/<file>     every file a setting pointed at, its setting now "assets/<file>"
//! ```
//!
//! Left out: the journal (replayed into the copy first), recordings, edits and anything else
//! in the workspace's folder. Stream keys already live in the system's credential store, never
//! in workspace files; on top of that, any setting named like a secret (`password`, `token`,
//! `secret`, `stream_key`, `key`, …) is removed, and so is any such parameter in a web
//! address's query.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use lightair_model::Workspace;
use serde_json::Value;

use crate::StoreError;

/// What [`export`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exported {
    /// Every file in the package, relative to it (`/` between folders), workspace files first.
    pub files: Vec<String>,
    /// How many media files were collected into `assets/`.
    pub assets: usize,
    /// How many secret settings were left out.
    pub secrets_removed: usize,
}

/// The workspace in `dir` as it is now: its snapshot and the journal not in it yet. Writes
/// nothing, so it's safe while the workspace is open elsewhere in the app (a command still
/// being written at the journal's end is left out).
pub fn load_current(dir: &Path) -> Result<Workspace, StoreError> {
    let raw: Value = crate::read_json(&crate::manifest(dir))?;
    let included = raw.get("journal").and_then(Value::as_u64);
    let mut workspace = crate::load_snapshot(dir)?;
    let journal_dir = dir.join("journal");
    for (n, path) in crate::rotated_logs(&journal_dir) {
        if included.is_none_or(|i| n > i) {
            crate::replay(&path, &mut workspace)?;
        }
    }
    let current = journal_dir.join("current.log");
    if current.exists() {
        crate::replay(&current, &mut workspace)?;
    }
    lightair_model::kinds::migrate(&mut workspace);
    Ok(workspace)
}

/// Writes a portable copy of the workspace in `dir` into `to` (made; must not hold another
/// package). Media is hard-linked where it can be (same disk), else copied.
pub fn export(dir: &Path, to: &Path) -> Result<Exported, StoreError> {
    let workspace = load_current(dir)?;
    crate::save(to, &workspace)?;
    let mut out = Exported::default();
    let mut collected: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    let assets = to.join("assets");
    let mut files = vec!["workspace.json".to_string()];
    for name in ["sources.json", "audio.json", "mixer.json", "outputs.json"] {
        files.push(name.into());
    }
    let mut scenes: Vec<String> = fs::read_dir(to.join("scenes"))
        .map_err(crate::io(&to.join("scenes")))?
        .flatten()
        .map(|e| format!("scenes/{}", e.file_name().to_string_lossy()))
        .collect();
    scenes.sort();
    files.extend(scenes);
    for name in &files {
        let path = to.join(name);
        let mut v: Value = crate::read_json(&path)?;
        let mut changed = false;
        rewrite(
            &mut v,
            dir,
            &mut |found: &Path| {
                if let Some(name) = collected.get(found) {
                    return Some(name.clone());
                }
                let name = unique_name(found, &mut taken);
                fs::create_dir_all(&assets).ok()?;
                let target = assets.join(&name);
                if fs::hard_link(found, &target).is_err() {
                    fs::copy(found, &target).ok()?;
                }
                collected.insert(found.to_path_buf(), name.clone());
                Some(name)
            },
            &mut out.secrets_removed,
            &mut changed,
        );
        if changed {
            crate::write_json(&path, &v)?;
        }
    }
    out.assets = collected.len();
    files.extend(collected.values().map(|n| format!("assets/{n}")));
    out.files = files;
    Ok(out)
}

/// A setting's name that holds a secret.
fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "key"
        || [
            "password",
            "passwd",
            "passphrase",
            "secret",
            "token",
            "stream_key",
            "streamkey",
            "api_key",
            "apikey",
            "auth",
        ]
        .iter()
        .any(|s| n.contains(s))
}

/// Walks `v`: secret settings out, file paths (absolute, or relative to `base`) to files that
/// exist collected by `collect` and pointed at `assets/<name>`.
fn rewrite(
    v: &mut Value,
    base: &Path,
    collect: &mut dyn FnMut(&Path) -> Option<String>,
    removed: &mut usize,
    changed: &mut bool,
) {
    match v {
        Value::Object(map) => {
            let before = map.len();
            map.retain(|k, v| !(secret_name(k) && (v.is_string() || v.is_number())));
            if map.len() != before {
                *removed += before - map.len();
                *changed = true;
            }
            for (_, v) in map.iter_mut() {
                rewrite(v, base, collect, removed, changed);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite(v, base, collect, removed, changed)),
        Value::String(s) => {
            if s.starts_with("http://") || s.starts_with("https://") {
                if let Some((clean, n)) = strip_query_secrets(s) {
                    *s = clean;
                    *removed += n;
                    *changed = true;
                }
            } else if let Some(file) = file_of(s, base)
                && let Some(name) = collect(&file)
            {
                *s = format!("assets/{name}");
                *changed = true;
            }
        }
        _ => {}
    }
}

/// The file a setting's text names, if it names one that exists.
fn file_of(s: &str, base: &Path) -> Option<PathBuf> {
    if s.is_empty() || s.len() > 4096 || s.contains('\n') || s.contains("://") {
        return None;
    }
    let p = Path::new(s);
    let path = if p.is_absolute() {
        p.to_path_buf()
    } else if s.contains('/') || s.contains('\\') {
        // (a bare word isn't taken for a file in the workspace's folder)
        base.join(p)
    } else {
        return None;
    };
    // the workspace's own files and folders aren't media
    if path.starts_with(base) && !path.starts_with(base.join("assets")) {
        return None;
    }
    fs::metadata(&path).ok().filter(|m| m.is_file()).map(|_| path)
}

/// `url` without query parameters named like secrets, and how many went (`None`: none).
fn strip_query_secrets(url: &str) -> Option<(String, usize)> {
    let (head, rest) = url.split_once('?')?;
    let (query, fragment) = match rest.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (rest, None),
    };
    let parts: Vec<&str> = query.split('&').collect();
    let kept: Vec<&str> = parts.iter().copied().filter(|p| !secret_name(p.split('=').next().unwrap_or(""))).collect();
    let gone = parts.len() - kept.len();
    if gone == 0 {
        return None;
    }
    let mut out = head.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some((out, gone))
}

/// `path`'s file name, made unique among `taken` ("logo.png", "logo 2.png", …).
fn unique_name(path: &Path, taken: &mut HashSet<String>) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let name: String = name.chars().map(|c| if c == '/' || c == '\\' || c == '\0' { '_' } else { c }).collect();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    let mut n = 1;
    loop {
        let candidate = if n == 1 { name.clone() } else { format!("{stem} {n}{ext}") };
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Removes exports in `dir` (one folder each) older than `age`.
pub fn remove_old(dir: &Path, age: std::time::Duration) -> io::Result<()> {
    for entry in fs::read_dir(dir)?.flatten() {
        let old =
            entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e > age);
        if old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightair_model::{Command, Source};
    use serde_json::json;

    #[test]
    fn exports_current_state_with_media_and_without_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("Show.lightair");
        let media = tmp.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("logo.png"), b"png").unwrap();
        fs::create_dir_all(ws_dir.join("assets")).unwrap();
        fs::write(ws_dir.join("assets").join("logo.png"), b"other png").unwrap();

        let mut ws = Workspace::new("Show");
        let source = |name: &str, settings: Value| Source {
            id: lightair_model::Id::new(),
            name: name.into(),
            kind: "image_source".into(),
            settings,
            effects: vec![],
        };
        let a = source("Logo", json!({"file": media.join("logo.png").to_string_lossy()}));
        let b = source("Inside", json!({"file": "assets/logo.png", "missing": "/no/such/file.png"}));
        let c = source(
            "Alerts",
            json!({"url": "https://example.com/alerts?theme=dark&token=abc123", "password": "hunter2"}),
        );
        ws.sources.insert(a.id, std::sync::Arc::new(a.clone()));
        ws.sources.insert(b.id, std::sync::Arc::new(b));
        crate::save(&ws_dir, &ws).unwrap();
        // a change only in the journal (not compacted yet)
        let mut opened = crate::Package::open(&ws_dir, || unreachable!()).unwrap();
        opened.package.record(&Command::AddSource { source: c.clone() }).unwrap();
        opened.package.flush().unwrap();
        opened.package.wait();

        let to = tmp.path().join("export");
        let done = export(&ws_dir, &to).unwrap();
        assert_eq!(done.assets, 2);
        assert_eq!(done.secrets_removed, 2);
        assert!(done.files.contains(&"assets/logo.png".to_string()));
        assert!(done.files.contains(&"assets/logo 2.png".to_string()));
        assert!(!to.join("journal").exists());

        let back = crate::load_snapshot(&to).unwrap();
        let settings = |id| back.sources.get(&id).unwrap().settings.clone();
        assert!(settings(a.id)["file"].as_str().unwrap().starts_with("assets/logo"));
        assert_eq!(settings(c.id), json!({"url": "https://example.com/alerts?theme=dark"}));
        let text = fs::read_to_string(to.join("sources.json")).unwrap();
        assert!(!text.contains("hunter2") && !text.contains("abc123"));
        assert!(text.contains("/no/such/file.png"));
        for f in &done.files {
            assert!(to.join(f).is_file(), "{f}");
        }
    }

    #[test]
    fn secret_names_and_query() {
        assert!(secret_name("stream_key") && secret_name("Token") && secret_name("key") && secret_name("auth_header"));
        assert!(!secret_name("keyframe_interval") && !secret_name("monkey") && !secret_name("url"));
        assert_eq!(strip_query_secrets("https://a/b?x=1&key=2#f"), Some(("https://a/b?x=1#f".into(), 1)));
        assert_eq!(strip_query_secrets("https://a/b?token=2"), Some(("https://a/b".into(), 1)));
        assert_eq!(strip_query_secrets("https://a/b?x=1"), None);
    }
}
