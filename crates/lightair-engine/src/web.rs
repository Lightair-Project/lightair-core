//! Custom (imported) widgets and web pages as layers: the desktop's widget server
//! (lightair-widgets: the widget's page and its `/lightair.js` event feed, on 127.0.0.1) runs
//! here on the chat's hub; the host shows each page in a hidden web view and pushes its frames to
//! the layer's feed, like a camera's (iOS: snapshots at a few frames a second; Android: drawn to
//! a surface).

use std::path::Path;
use std::sync::OnceLock;

use lightair_model::Id;
use lightair_widgets::Widgets;
use serde_json::Value;

/// A web layer's page, for the host's web view.
#[derive(Debug, Clone, PartialEq)]
pub struct WebFeed {
    pub source: Id,
    /// Its frames go here.
    pub feed: u32,
    /// The page (the widget server's address for a widget).
    pub url: String,
    pub width: u32,
    pub height: u32,
    /// The frame rate it asks for (the host may draw fewer: iOS, 5 at most).
    pub fps: u32,
}

static SERVER: OnceLock<Option<Widgets>> = OnceLock::new();

/// The widget server (one for the app, on the chat's hub; started the first time it's needed).
pub fn server(store_dir: &Path) -> Option<&'static Widgets> {
    SERVER
        .get_or_init(|| {
            Widgets::start(crate::chat::hub(), store_dir.to_path_buf())
                .map_err(|e| tracing::warn!("the widget server didn't start: {e}"))
                .ok()
        })
        .as_ref()
}

/// The widget server if it has started (the render thread reads a timer's storage from it).
pub fn started() -> Option<&'static Widgets> {
    SERVER.get().and_then(Option::as_ref)
}

/// What a widget object's settings show as a web page, if not drawn natively: an imported
/// widget (or a built-in kind phones don't draw) at the widget server, or a plain page's address.
/// (url, width, height, fps)
pub fn page(settings: &Value, store_dir: &Path) -> Option<(String, u32, u32, u32)> {
    let size = (
        settings["width"].as_u64().unwrap_or(800).clamp(16, 4096) as u32,
        settings["height"].as_u64().unwrap_or(600).clamp(16, 4096) as u32,
    );
    let fps = settings["fps"].as_u64().unwrap_or(30).clamp(1, 60) as u32;
    let url = match crate::widgets::instance(settings) {
        Some(i) if crate::widgets::NATIVE.contains(&i.kind.as_str()) => return None,
        Some(i) => server(store_dir)?.url(&i.id, &i.key),
        None => {
            let u = settings["url"].as_str().filter(|u| !u.is_empty())?;
            match server(store_dir) {
                Some(s) => lightair_widgets::resolve(u, s.port()),
                None => u.to_string(),
            }
        }
    };
    Some((url, size.0, size.1, fps))
}
