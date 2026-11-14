//! Lightair's own kinds for what's captured from the machine (docs/plans/windows.md, "Portable
//! project kinds"). A project stores these, not libobs's ids, so it opens on macOS and Windows
//! alike; the engine maps each to the platform's libobs source when it plays
//! (`lightair_engine::kinds`). Kinds libobs has on both platforms (colour, image, media, text,
//! browser) keep libobs's ids.
//!
//! Settings use the names below. Which display, window, app or device is machine-specific: on
//! another machine (or platform) it may not exist, and the Inspector asks for another.

use serde_json::{Map, Value, json};

use crate::Project;

/// A display, one window or one app's windows. Settings: `mode` (0 display, 1 window, 2 app),
/// `display`, `window`, `app` (the platform's ids), `cursor` (bool, shown unless false).
pub const SCREEN: &str = "lightair_screen";
/// A camera or capture card. Settings: `device`, `height` (the short side asked for: 720, 1080,
/// 2160; absent for the camera's own best).
pub const CAMERA: &str = "lightair_camera";
/// A microphone through the OS. Settings: `device` ("default" for the system's).
pub const MIC: &str = "lightair_mic";
/// Everything the computer plays. Settings: `device` (output device; "default").
pub const DESKTOP_AUDIO: &str = "lightair_desktop_audio";
/// One app's sound. Settings: `app` (macOS: bundle id; Windows: libobs's window string).
pub const APP_AUDIO: &str = "lightair_app_audio";

/// Screen modes (`mode`).
pub const DISPLAY: i64 = 0;
pub const WINDOW: i64 = 1;
pub const APP: i64 = 2;

/// Every portable kind.
pub const ALL: [&str; 5] = [SCREEN, CAMERA, MIC, DESKTOP_AUDIO, APP_AUDIO];

/// Sound-only kinds.
pub fn is_audio(kind: &str) -> bool {
    matches!(kind, MIC | DESKTOP_AUDIO | APP_AUDIO)
}

/// The setting that names which display, window, app or device an input takes, if its kind has
/// one to choose.
pub fn device_key(kind: &str, settings: &Value) -> Option<&'static str> {
    match kind {
        SCREEN => Some(match settings["mode"].as_i64().unwrap_or(DISPLAY) {
            WINDOW => "window",
            APP => "app",
            _ => "display",
        }),
        CAMERA | MIC => Some("device"),
        _ => None,
    }
}

/// The portable kind and settings for a libobs input of either platform (projects from before
/// portable kinds, and OBS imports), or `None` for kinds that are already portable.
pub fn from_libobs(kind: &str, settings: &Value) -> Option<(&'static str, Value)> {
    /// Moves `from` to `to` (when present).
    fn take(rest: &mut Map<String, Value>, from: &str, to: &str) {
        if let Some(v) = rest.remove(from) {
            rest.insert(to.into(), v);
        }
    }
    let mut rest = settings.as_object().cloned().unwrap_or_default();
    let out = match kind {
        // macOS: ScreenCaptureKit
        "screen_capture" => {
            take(&mut rest, "type", "mode");
            take(&mut rest, "display_uuid", "display");
            take(&mut rest, "application", "app");
            take(&mut rest, "show_cursor", "cursor");
            SCREEN
        }
        // macOS's older captures, and Windows's
        "display_capture" => {
            rest.remove("display");
            rest.insert("mode".into(), json!(DISPLAY));
            take(&mut rest, "show_cursor", "cursor");
            SCREEN
        }
        "monitor_capture" => {
            rest.insert("mode".into(), json!(DISPLAY));
            take(&mut rest, "monitor_id", "display");
            take(&mut rest, "capture_cursor", "cursor");
            SCREEN
        }
        "window_capture" => {
            rest.insert("mode".into(), json!(WINDOW));
            take(&mut rest, "show_cursor", "cursor");
            SCREEN
        }
        "av_capture_input_v2" | "av_capture_input" | "macos-avcapture" | "macos-avcapture-fast" => CAMERA,
        "dshow_input" => {
            take(&mut rest, "video_device_id", "device");
            CAMERA
        }
        "coreaudio_input_capture" | "wasapi_input_capture" => {
            take(&mut rest, "device_id", "device");
            MIC
        }
        "coreaudio_output_capture" | "wasapi_output_capture" => {
            take(&mut rest, "device_id", "device");
            DESKTOP_AUDIO
        }
        "sck_audio_capture" => {
            let app = rest.remove("type").and_then(|t| t.as_i64()) == Some(1);
            take(&mut rest, "application", "app");
            if app { APP_AUDIO } else { DESKTOP_AUDIO }
        }
        "wasapi_process_output_capture" => {
            take(&mut rest, "window", "app");
            APP_AUDIO
        }
        _ => return None,
    };
    Some((out, Value::Object(rest)))
}

/// Converts a project's libobs capture inputs to portable kinds. Returns whether any changed.
pub fn migrate(project: &mut Project) -> bool {
    let mut changed = false;
    for input in project.inputs.values_mut() {
        if let Some((kind, settings)) = from_libobs(&input.kind, &input.settings) {
            let i = std::sync::Arc::make_mut(input);
            i.kind = kind.into();
            i.settings = settings;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_and_windows_captures_become_portable() {
        let (k, s) =
            from_libobs("screen_capture", &json!({"type": 1, "window": 42, "show_cursor": false, "hide_obs": true}))
                .unwrap();
        assert_eq!(k, SCREEN);
        assert_eq!(s, json!({"mode": 1, "window": 42, "cursor": false, "hide_obs": true}));
        assert_eq!(device_key(k, &s), Some("window"));

        let (k, s) = from_libobs("monitor_capture", &json!({"monitor_id": "\\\\?\\DISPLAY#1"})).unwrap();
        assert_eq!((k, &s["mode"], &s["display"]), (SCREEN, &json!(0), &json!("\\\\?\\DISPLAY#1")));

        let (k, s) = from_libobs("sck_audio_capture", &json!({"type": 1, "application": "com.spotify.client"})).unwrap();
        assert_eq!((k, s), (APP_AUDIO, json!({"app": "com.spotify.client"})));
        let (k, s) = from_libobs("sck_audio_capture", &json!({"type": 0})).unwrap();
        assert_eq!((k, s), (DESKTOP_AUDIO, json!({})));

        let (k, s) = from_libobs("dshow_input", &json!({"video_device_id": "Cam:\\\\?\\usb"})).unwrap();
        assert_eq!((k, &s["device"]), (CAMERA, &json!("Cam:\\\\?\\usb")));
        let (k, s) = from_libobs("coreaudio_input_capture", &json!({"device_id": "default"})).unwrap();
        assert_eq!((k, s), (MIC, json!({"device": "default"})));

        // already portable, or the same on both platforms
        assert!(from_libobs(SCREEN, &json!({})).is_none());
        assert!(from_libobs("color_source_v3", &json!({})).is_none());
    }
}
