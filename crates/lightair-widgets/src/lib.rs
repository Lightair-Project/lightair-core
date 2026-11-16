//! Lightair's widgets (docs/plans/live-integrations.md, 2.5): overlay pages Lightair serves on
//! this machine and shows on the canvas like any web page, fed by the event bus: chat box,
//! alert box, event list, goal, label, counter, subathon timer, and StreamElements custom widgets
//! imported as they are.
//!
//! A widget lives in its web page object's settings (`"widget": {…}`, see [`Instance`]), so it
//! is saved, undone and copied with the workspace. The app tells the server which widgets exist
//! ([`Widgets::set_instances`]); pages reach Lightair only through the bridge (`/lightair.js`).

pub mod catalog;
pub mod fields;
mod http;
pub mod page;
mod server;
pub mod session;
mod store;
pub mod test_events;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use server::{Icons, PORT, Widgets};

/// The scheme a widget's web page object loads (`lightair-widget://<id>?key=<key>`): resolved to
/// the server's address when the page opens (the port can differ between runs).
pub const SCHEME: &str = "lightair-widget://";

/// A widget: what its web page object's settings keep under `"widget"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    /// A built-in widget's kind ("alert-box", …) or "custom".
    pub kind: String,
    /// Only its own page and feed open with it.
    pub key: String,
    /// The settings changed from their defaults.
    #[serde(default)]
    pub fields: Value,
    /// An imported widget's code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<Custom>,
}

/// An imported widget (StreamElements' custom widget tabs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Custom {
    #[serde(default)]
    pub html: String,
    #[serde(default)]
    pub css: String,
    #[serde(default)]
    pub js: String,
    /// Its `fields.json`, as written (its order is the Inspector's).
    #[serde(default)]
    pub fields: String,
}

impl Instance {
    pub fn from_settings(settings: &Value) -> Option<Instance> {
        serde_json::from_value(settings.get("widget")?.clone()).ok()
    }

    /// The fields' schema.
    pub fn schema(&self) -> &str {
        match &self.custom {
            Some(c) => &c.fields,
            None => catalog::by_kind(&self.kind).map_or("{}", |b| b.fields),
        }
    }

    /// A new widget of `kind`.
    pub fn new(kind: &str, custom: Option<Custom>) -> Instance {
        Instance { id: random_hex(8), kind: kind.into(), key: random_hex(16), fields: Value::Object(Default::default()), custom }
    }

    /// The address its web page object loads.
    pub fn address(&self) -> String {
        format!("{SCHEME}{}?key={}", self.id, self.key)
    }
}

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `lightair-widget://<id>?key=…` → the server's address on `port`; anything else as it is.
pub fn resolve(url: &str, port: u16) -> String {
    match url.strip_prefix(SCHEME) {
        Some(rest) => {
            let (id, q) = rest.split_once('?').unwrap_or((rest, ""));
            format!("http://127.0.0.1:{port}/w/{id}/?{q}")
        }
        None => url.to_string(),
    }
}

/// A widget from a folder, or a `.lwidget` (or `.zip`) package of the same files.
pub fn import(path: &std::path::Path) -> Result<Custom, String> {
    if path.is_dir() {
        return import_folder(path);
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| "that isn't a .lwidget (zip) package".to_string())?;
    let mut c = Custom { html: String::new(), css: String::new(), js: String::new(), fields: String::new() };
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().rsplit('/').next().unwrap_or("").to_ascii_lowercase();
        let mut text = String::new();
        if std::io::Read::read_to_string(&mut f, &mut text).is_err() {
            continue;
        }
        if name.ends_with(".html") || name.ends_with(".htm") {
            c.html = text;
        } else if name.ends_with(".css") {
            c.css = text;
        } else if name.ends_with(".js") {
            c.js = text;
        } else if name.contains("field") && name.ends_with(".json") {
            c.fields = text;
        }
    }
    if c.html.is_empty() && c.js.is_empty() {
        return Err("no HTML or JS in that package".into());
    }
    Ok(c)
}

/// Writes `c` as a `.lwidget` package (its manifest names it `name`, `size`).
pub fn export(c: &Custom, name: &str, size: (u32, u32), path: &std::path::Path) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let manifest = serde_json::json!({"name": name, "version": "1.0.0", "size": [size.0, size.1], "format": "lightair-widget/1"});
    for (file, text) in [
        ("manifest.json", serde_json::to_string_pretty(&manifest).unwrap_or_default()),
        ("index.html", c.html.clone()),
        ("style.css", c.css.clone()),
        ("main.js", c.js.clone()),
        ("fields.json", c.fields.clone()),
    ] {
        zip.start_file(file, opts).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut zip, text.as_bytes()).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// An imported StreamElements widget from its folder's files (`html`, `css`, `js`, `fields`:
/// whichever exist, by extension or name).
pub fn import_folder(dir: &std::path::Path) -> Result<Custom, String> {
    let mut c = Custom { html: String::new(), css: String::new(), js: String::new(), fields: String::new() };
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if name.ends_with(".html") || name.ends_with(".htm") {
            c.html = text;
        } else if name.ends_with(".css") {
            c.css = text;
        } else if name.ends_with(".js") {
            c.js = text;
        } else if name.contains("field") && name.ends_with(".json") {
            c.fields = text;
        }
    }
    if c.html.is_empty() && c.js.is_empty() {
        return Err("no HTML or JS file there".into());
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_round_trip() {
        let c = Custom { html: "<div>{{title}}</div>".into(), css: "div{}".into(), js: "1".into(), fields: r#"{"title":{"type":"text","value":"x"}}"#.into() };
        let path = std::env::temp_dir().join(format!("lightair-{}.lwidget", std::process::id()));
        export(&c, "Test", (400, 200), &path).unwrap();
        assert_eq!(import(&path).unwrap(), c);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn addresses_resolve_and_settings_round_trip() {
        let i = Instance::new("alert-box", None);
        let url = resolve(&i.address(), 4460);
        assert_eq!(url, format!("http://127.0.0.1:4460/w/{}/?key={}", i.id, i.key));
        assert_eq!(resolve("https://example.com", 4460), "https://example.com");
        let settings = serde_json::json!({"url": i.address(), "widget": i});
        assert_eq!(Instance::from_settings(&settings), Some(i));
    }
}
