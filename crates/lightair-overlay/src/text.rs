//! Text and timer layers as the desktop stores them (libobs `text_ft2`, Lightair's timer), as
//! text styles to draw: shared by the engine and the broadcast extension.

use lightair_render::sequence::TextStyle;
use serde_json::Value;

/// libobs's 0xAABBGGRR as 0xRRGGBB.
fn rgb_of(abgr: u64) -> u32 {
    let v = abgr as u32;
    ((v & 0xFF) << 16) | (v & 0xFF00) | ((v >> 16) & 0xFF)
}

/// A text source's settings (libobs `text_ft2`, as the desktop writes them) as a text style.
pub fn text_style(s: &Value, text: String) -> TextStyle {
    let font = &s["font"];
    let family = font["face"]
        .as_str()
        .filter(|f| !f.is_empty())
        .map(str::to_string);
    let bold = font["flags"].as_i64().unwrap_or(0) & 1 == 1;
    TextStyle {
        text,
        size: font["size"].as_f64().unwrap_or(72.0) as f32,
        color: rgb_of(s["color1"].as_u64().unwrap_or(0xFFFF_FFFF)),
        bold,
        align: match s["align"].as_str() {
            Some("left") => 0,
            Some("right") => 2,
            _ => 1,
        },
        background: s["background"]
            .as_u64()
            .map(|c| (rgb_of(c), ((c >> 24) & 0xFF) as f32 / 255.0)),
        outline: s["outline"]
            .as_bool()
            .filter(|o| *o)
            .map(|_| rgb_of(s["outline_color"].as_u64().unwrap_or(0xFF00_0000))),
        width: s["width"].as_f64().unwrap_or(4096.0) as f32,
        font: family.map(|f| lightair_fonts::FontRef::new(f, if bold { 700 } else { 400 })),
    }
}

/// What a timer shows `elapsed_ns` after its scene went on air (the desktop's settings:
/// `mode` countdown | stopwatch | clock, `seconds`, `end_text`, `show_seconds`).
pub fn timer_text(s: &Value, elapsed_ns: i64, utc_offset_min: i32) -> String {
    let elapsed = elapsed_ns.max(0) / 1_000_000_000;
    let clock = |t: i64| {
        let (h, m, sec) = (t / 3600, t / 60 % 60, t % 60);
        if h > 0 {
            format!("{h}:{m:02}:{sec:02}")
        } else {
            format!("{m}:{sec:02}")
        }
    };
    match s["mode"].as_str() {
        Some("stopwatch") => clock(elapsed),
        Some("clock") => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
                + utc_offset_min as i64 * 60;
            let day = now.rem_euclid(86_400);
            if s["show_seconds"].as_bool().unwrap_or(false) {
                format!("{}:{:02}:{:02}", day / 3600, day / 60 % 60, day % 60)
            } else {
                format!("{}:{:02}", day / 3600, day / 60 % 60)
            }
        }
        _ => {
            let left = s["seconds"].as_i64().unwrap_or(300).max(0) - elapsed;
            match s["end_text"].as_str().filter(|t| !t.is_empty()) {
                Some(end) if left <= 0 => end.to_string(),
                _ => clock(left.max(0)),
            }
        }
    }
}
