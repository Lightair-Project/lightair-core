//! The widget run's pages (design §4.4): each widget of a package written as the app composes it
//! (`lightair_widgets::page::build`: its fields' defaults filled in, the bridge and the shims), with
//! its files beside it and the bridge at the root, plus `widgets.json` for the runner: each
//! widget's id, size, buttons, fields, the hosts the package declared, and sample events. The
//! check workflow's widget runner (lightair-checks, headless Chromium) serves the folder, plays
//! Lightair's side of the bridge and records every request.

use std::path::Path;

use lightair_packages::{Kind, Limits, Package};
use serde_json::{Value, json};

/// Writes the pages into `out`. Returns how many widgets there were.
pub fn write_pages(path: &Path, out: &Path) -> Result<usize, String> {
    let package = Package::open_with(path, Limits::every_tier()).map_err(|e| e.to_string())?;
    let temp = std::env::temp_dir().join(format!("lightair-check-widgets-{}", std::process::id()));
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        let _ = std::fs::remove_dir_all(&temp);
        package.extract_to(&temp).map_err(|e| e.to_string())?;
        temp.clone()
    };
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    for (name, text) in [
        ("lightair.js", lightair_widgets::page::BRIDGE),
        ("se-shim.js", lightair_widgets::page::SE_SHIM),
        ("sl-shim.js", lightair_widgets::page::SL_SHIM),
    ] {
        std::fs::write(out.join(name), text).map_err(|e| e.to_string())?;
    }
    let hosts: Vec<String> = package
        .manifest
        .permissions
        .iter()
        .filter_map(|p| p.strip_prefix("network:"))
        .filter(|h| !h.starts_with("localhost"))
        .map(str::to_string)
        .collect();
    let mut widgets = Vec::new();
    for (n, content) in package.manifest.contents.iter().filter(|c| c.kind == Kind::Widget).enumerate() {
        let folder = dir.join(&content.path);
        let custom = lightair_widgets::import_folder(&folder).map_err(|e| format!("{}: {e}", content.path))?;
        let fields = lightair_widgets::fields::parse(&custom.fields);
        let defaults = lightair_widgets::fields::values(&fields, &Value::Object(Default::default()));
        let buttons: Vec<&str> = fields.iter().filter(|f| f.kind == "button").map(|f| f.key.as_str()).collect();
        let instance = lightair_widgets::Instance::new("custom", Some(custom));
        let id = format!("widget{n}");
        let page_dir = out.join("w").join(&id);
        copy_dir(&folder, &page_dir)?;
        std::fs::write(page_dir.join("index.html"), lightair_widgets::page::build(&instance))
            .map_err(|e| e.to_string())?;
        let size = read_size(&folder).unwrap_or((800, 600));
        widgets.push(json!({
            "id": id,
            "path": content.path,
            "width": size.0,
            "height": size.1,
            "buttons": buttons,
            "fields": defaults,
        }));
    }
    let events: Vec<Value> = ["follow", "sub", "paid", "cheer", "raid", "message", "message", "message"]
        .iter()
        .map(|k| serde_json::to_value(lightair_widgets::test_events::make(k, "twitch")).unwrap_or(Value::Null))
        .collect();
    let manifest = json!({ "id": package.manifest.id.as_str(), "hosts": hosts, "widgets": widgets, "events": events });
    std::fs::write(out.join("widgets.json"), serde_json::to_vec_pretty(&manifest).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    if !path.is_dir() {
        let _ = std::fs::remove_dir_all(&temp);
    }
    Ok(manifest["widgets"].as_array().map_or(0, Vec::len))
}

fn read_size(folder: &Path) -> Option<(u64, u64)> {
    let m: Value = serde_json::from_slice(&std::fs::read(folder.join("manifest.json")).ok()?).ok()?;
    let s = m["size"].as_array()?;
    Some((s.first()?.as_u64()?, s.get(1)?.as_u64()?))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()))?;
        } else {
            std::fs::copy(&p, to.join(e.file_name())).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
