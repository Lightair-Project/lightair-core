//! Widgets drawn natively: the desktop's widget objects (`lightair_web` with
//! `"widget": {…}` in their settings, lightair-widgets' `Instance`) drawn from the same settings
//! with lightair-render's text, so a desktop project's widgets look the same on the phone. The
//! common set: chat box, alert box, event list, goal, label, counter. Custom (imported) widgets
//! and other web pages are the host's web views (the engine's `web.rs`).

use lightair_live::ChatEvent;
use lightair_render::generate;
use lightair_render::sequence::TextStyle;
use lightair_widgets::{Instance, fields};
use serde_json::{Map, Value};

pub const KIND: &str = "lightair_web";

/// The kinds drawn here.
pub const NATIVE: &[&str] = &[
    "chat-box",
    "alert-box",
    "event-list",
    "goal",
    "label",
    "counter",
    "subathon",
];

/// A widget object's instance, if the source is one.
pub fn instance(settings: &Value) -> Option<Instance> {
    Instance::from_settings(settings)
}

/// Its settings: the schema's defaults with what was changed.
pub fn values(i: &Instance) -> Map<String, Value> {
    fields::values(&fields::parse(i.schema()), &i.fields)
}

/// A new widget's source settings (its size from the catalogue).
pub fn new_settings(kind: &str) -> Option<(Value, (u32, u32))> {
    let b = lightair_widgets::catalog::by_kind(kind)?;
    let id = uuid::Uuid::new_v4().to_string();
    let instance = Instance {
        id: id.clone(),
        kind: kind.into(),
        key: id.replace('-', ""),
        fields: Value::Object(Map::new()),
        custom: None,
        platforms: Vec::new(),
    };
    let (w, h) = b.size;
    Some((
        serde_json::json!({ "url": "", "width": w, "height": h, "fps": 30, "widget": instance }),
        (w, h),
    ))
}

/// What a widget shows comes from here.
pub struct Feed<'a> {
    /// Chat and events so far, oldest first.
    pub chat: &'a [ChatEvent],
    /// The alert on now.
    pub alert: Option<&'a crate::alerts::Alert>,
    /// What the widget server keeps for it (a subathon timer's running state and time added),
    /// and the time now (ms since 1970).
    pub stored: Option<&'a Map<String, Value>>,
    pub now_ms: i64,
}

/// A subathon timer's time left (ms), as the widget server counts it: running to `endAt`,
/// stopped at `remaining`, or its starting minutes before it was ever started.
pub fn subathon_left(fields: &Map<String, Value>, stored: &Map<String, Value>, now_ms: i64) -> i64 {
    let running = stored
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match (
        running,
        stored.get("endAt").and_then(Value::as_i64),
        stored.get("remaining").and_then(Value::as_i64),
    ) {
        (true, Some(end), _) => (end - now_ms).max(0),
        (_, _, Some(r)) => r.max(0),
        _ => (num(fields, "startMinutes", 60.0).max(1.0) * 60_000.0) as i64,
    }
}

/// "1:02:03", or "4:05" under an hour.
fn clock(ms: i64) -> String {
    let s = (ms.max(0) + 999) / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn num(v: &Map<String, Value>, k: &str, d: f64) -> f64 {
    v.get(k)
        .and_then(|x| {
            x.as_f64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(d)
}

fn text<'a>(v: &'a Map<String, Value>, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn flag(v: &Map<String, Value>, k: &str, d: bool) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(d)
}

/// "#RRGGBB", "#RGB" or "rgba(r, g, b, a)" as (0xRRGGBB, alpha 0–1).
pub fn colour(s: &str) -> Option<(u32, f32)> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let hex = if hex.len() == 3 {
            hex.chars().flat_map(|c| [c, c]).collect::<String>()
        } else {
            hex.to_string()
        };
        return u32::from_str_radix(&hex[..6.min(hex.len())], 16)
            .ok()
            .map(|c| (c, 1.0));
    }
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))?
        .trim_end_matches(')');
    let p: Vec<f32> = inner
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    (p.len() >= 3).then(|| {
        (
            ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32,
            p.get(3).copied().unwrap_or(1.0),
        )
    })
}

/// A premultiplied RGBA canvas widgets are drawn on.
struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![0; (w * h * 4) as usize],
        }
    }

    /// Draws a premultiplied raster with its top-left at (x, y).
    fn draw(&mut self, r: &generate::Raster, x: i32, y: i32) {
        let (rw, rh) = (r.size.0 as i32, r.size.1 as i32);
        for yy in 0..rh {
            let ty = y + yy;
            if ty < 0 || ty >= self.h as i32 {
                continue;
            }
            for xx in 0..rw {
                let tx = x + xx;
                if tx < 0 || tx >= self.w as i32 {
                    continue;
                }
                let s = ((yy * rw + xx) * 4) as usize;
                let d = ((ty as u32 * self.w + tx as u32) * 4) as usize;
                let a = r.rgba[s + 3] as u32;
                for c in 0..4 {
                    self.px[d + c] = (r.rgba[s + c] as u32
                        + self.px[d + c] as u32 * (255 - a) / 255)
                        .min(255) as u8;
                }
            }
        }
    }

    fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, (rgb, alpha): (u32, f32)) {
        let a = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
        let c = [(rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF];
        for yy in y..(y + h).min(self.h) {
            for xx in x..(x + w).min(self.w) {
                let d = ((yy * self.w + xx) * 4) as usize;
                for (k, ck) in c.iter().enumerate() {
                    self.px[d + k] = (ck * a / 255 + self.px[d + k] as u32 * (255 - a) / 255) as u8;
                }
                self.px[d + 3] = (a + self.px[d + 3] as u32 * (255 - a) / 255) as u8;
            }
        }
    }
}

fn style(
    v: &Map<String, Value>,
    s: String,
    size: f32,
    width: f32,
    align: u8,
    bold: bool,
) -> TextStyle {
    TextStyle {
        text: s,
        size,
        color: colour(text(v, "textColor")).map_or(0xFFFFFF, |c| c.0),
        bold,
        align,
        background: None,
        outline: None,
        width,
        font: None,
    }
}

fn align_of(v: &Map<String, Value>) -> u8 {
    match text(v, "align") {
        "left" => 0,
        "right" => 2,
        _ => 1,
    }
}

/// The widget's picture now: premultiplied RGBA of `size`. Also a key that changes when it
/// does (to draw it again only then).
pub fn draw(i: &Instance, size: (u32, u32), feed: &Feed) -> (Vec<u8>, (u32, u32)) {
    let v = values(i);
    let (w, h) = (size.0.clamp(16, 2048), size.1.clamp(16, 2048));
    let mut c = Canvas::new(w, h);
    let font = num(&v, "fontSize", 24.0) as f32;
    let pad = (font * 0.5) as i32;
    let events = || feed.chat.iter().filter(|e| e.alert_kind().is_some());
    let latest = |kind: &str| {
        feed.chat
            .iter()
            .rev()
            .find(|e| e.alert_kind() == Some(kind))
    };
    match i.kind.as_str() {
        "chat-box" => {
            let hidden: Vec<String> = text(&v, "hideUsers")
                .split(',')
                .map(|u| u.trim().to_lowercase())
                .filter(|u| !u.is_empty())
                .collect();
            let shown: Vec<&ChatEvent> = feed
                .chat
                .iter()
                .filter(|e| {
                    e.kind == lightair_live::Kind::Message
                        || (flag(&v, "showEvents", true) && e.alert_kind().is_some())
                })
                .filter(|e| {
                    !e.deleted && !(flag(&v, "hideCommands", true) && e.text.starts_with('!'))
                })
                .filter(|e| !hidden.contains(&e.name().to_lowercase()))
                .collect();
            let max = num(&v, "maxMessages", 30.0).max(1.0) as usize;
            let lines: Vec<String> = shown
                .iter()
                .rev()
                .take(max)
                .map(|e| {
                    let body = if e.text.is_empty() {
                        e.headline.clone().unwrap_or_default()
                    } else {
                        e.text.clone()
                    };
                    format!("{}: {body}", e.name())
                })
                .collect();
            // newest at the bottom (or the top), as many as fit
            let from_top = text(&v, "align") == "top";
            let mut y = if from_top { pad } else { h as i32 - pad };
            for line in lines {
                let r = generate::text(&style(
                    &v,
                    line,
                    font,
                    (w as i32 - 2 * pad) as f32,
                    0,
                    false,
                ));
                if from_top {
                    if y + r.size.1 as i32 > h as i32 {
                        break;
                    }
                    c.draw(&r, pad, y);
                    y += r.size.1 as i32 + pad / 2;
                } else {
                    y -= r.size.1 as i32;
                    if y < 0 {
                        break;
                    }
                    c.draw(&r, pad, y);
                    y -= pad / 2;
                }
            }
        }
        "alert-box" => {
            if let Some(a) = feed.alert {
                let big = generate::text(&style(
                    &v,
                    a.title.clone(),
                    font * 1.8,
                    w as f32 - 40.0,
                    1,
                    true,
                ));
                let msg = (flag(&v, "showMessage", true) && !a.message.is_empty()).then(|| {
                    generate::text(&style(
                        &v,
                        a.message.clone(),
                        font,
                        w as f32 - 40.0,
                        1,
                        false,
                    ))
                });
                let total = big.size.1 + msg.as_ref().map_or(0, |m| m.size.1 + pad as u32);
                let mut y = (h as i32 - total as i32) / 2;
                c.draw(&big, (w as i32 - big.size.0 as i32) / 2, y);
                y += big.size.1 as i32 + pad;
                if let Some(m) = msg {
                    c.draw(&m, (w as i32 - m.size.0 as i32) / 2, y);
                }
            }
        }
        "event-list" => {
            if let Some(bg) = colour(text(&v, "background")) {
                c.rect(0, 0, w, h, bg);
            }
            let max = num(&v, "maxEvents", 6.0).max(1.0) as usize;
            let mut y = pad;
            for e in events()
                .filter(|e| flag(&v, &format!("{}Show", e.alert_kind().unwrap_or("")), true))
                .rev()
                .take(max)
            {
                let what = match e.alert_kind() {
                    Some("follow") => "followed".to_string(),
                    Some("sub") => "subscribed".to_string(),
                    Some("raid") => "raided".to_string(),
                    _ => e.paid.as_ref().map_or(String::new(), |p| p.display.clone()),
                };
                let r = generate::text(&style(
                    &v,
                    format!("{} · {what}", e.name()),
                    font,
                    w as f32 - 2.0 * pad as f32,
                    0,
                    false,
                ));
                if y + r.size.1 as i32 > h as i32 {
                    break;
                }
                c.draw(&r, pad, y);
                y += r.size.1 as i32 + pad / 2;
            }
        }
        "goal" => {
            let counts = text(&v, "counts");
            let done: f64 = events()
                .filter(|e| {
                    e.alert_kind() == Some(counts)
                        || (counts == "paid" && e.alert_kind() == Some("paid"))
                })
                .map(|e| {
                    if counts == "paid" {
                        e.paid.as_ref().map_or(0.0, |p| p.amount)
                    } else {
                        1.0
                    }
                })
                .sum::<f64>()
                + num(&v, "start", 0.0);
            let target = num(&v, "target", 50.0).max(1.0);
            let mut label = text(&v, "title").to_string();
            if flag(&v, "showNumbers", true) {
                label = format!("{label}  {} / {}", done.round(), target.round());
            }
            let r = generate::text(&style(&v, label, font, w as f32, 0, true));
            c.draw(&r, 0, 0);
            let bar_y = r.size.1 + pad as u32 / 2;
            let bar_h = num(&v, "barHeight", 28.0) as u32;
            c.rect(
                0,
                bar_y,
                w,
                bar_h,
                colour(text(&v, "trackColor")).unwrap_or((0, 0.45)),
            );
            let filled = ((done / target).clamp(0.0, 1.0) * w as f64) as u32;
            c.rect(
                0,
                bar_y,
                filled,
                bar_h,
                colour(text(&v, "fillColor")).unwrap_or((0xF5A623, 1.0)),
            );
        }
        "label" => {
            let shows = text(&v, "shows");
            let s = match latest(shows) {
                Some(e) => text(&v, "template").replace("{name}", e.name()).replace(
                    "{amount}",
                    e.paid.as_ref().map_or("", |p| p.display.as_str()),
                ),
                None => text(&v, "empty").to_string(),
            };
            if !s.is_empty() {
                let r = generate::text(&style(
                    &v,
                    s,
                    font,
                    w as f32,
                    align_of(&v),
                    flag(&v, "bold", true),
                ));
                c.draw(&r, 0, (h as i32 - r.size.1 as i32) / 2);
            }
        }
        "subathon" => {
            let empty = Map::new();
            let stored = feed.stored.unwrap_or(&empty);
            let left = subathon_left(&v, stored, feed.now_ms);
            let title = text(&v, "title");
            let mut y = 0i32;
            if !title.is_empty() {
                let r =
                    generate::text(&style(&v, title.to_string(), font * 0.3, w as f32, 1, true));
                c.draw(&r, 0, y);
                y += r.size.1 as i32;
            }
            let r = generate::text(&style(&v, clock(left), font, w as f32, 1, true));
            c.draw(&r, 0, y);
            y += r.size.1 as i32;
            // the time just added, for five seconds, in the accent colour
            if let Some(added) = stored.get("lastAdded")
                && let (Some(ms), Some(at)) = (added["ms"].as_i64(), added["at"].as_i64())
                && ms > 0
                && feed.now_ms - at < 5_000
            {
                let mut accent = v.clone();
                accent.insert(
                    "textColor".into(),
                    v.get("accentColor").cloned().unwrap_or(Value::Null),
                );
                let from = added["from"].as_str().unwrap_or("");
                let s = if from.is_empty() {
                    format!("+{}", clock(ms))
                } else {
                    format!("+{}  {from}", clock(ms))
                };
                let r = generate::text(&style(&accent, s, font * 0.35, w as f32, 1, true));
                c.draw(&r, 0, y);
            }
        }
        "counter" => {
            let value = num(&v, "_value", 0.0);
            let s = text(&v, "template")
                .replace("{title}", text(&v, "title"))
                .replace("{value}", &format!("{}", value.round()));
            let r = generate::text(&style(&v, s, font, w as f32, align_of(&v), true));
            c.draw(&r, 0, (h as i32 - r.size.1 as i32) / 2);
        }
        other => {
            let name =
                lightair_widgets::catalog::by_kind(other).map_or("Custom widget", |b| b.name);
            c.rect(0, 0, w, h, (0x202024, 0.6));
            let r = generate::text(&style(
                &v,
                format!("{name}: not shown on phones yet"),
                24.0,
                w as f32 - 20.0,
                1,
                false,
            ));
            c.draw(&r, 10, (h as i32 - r.size.1 as i32) / 2);
        }
    }
    (c.px, (w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_and_settings_read_as_the_desktop_writes_them() {
        assert_eq!(colour("#F5A623"), Some((0xF5A623, 1.0)));
        assert_eq!(colour("#fff"), Some((0xFFFFFF, 1.0)));
        assert_eq!(colour("rgba(0,0,0,0.45)"), Some((0, 0.45)));
        let (settings, size) = new_settings("goal").unwrap();
        let i = instance(&settings).unwrap();
        assert_eq!((i.kind.as_str(), size.0 > 0), ("goal", true));
    }

    #[test]
    fn a_chat_box_draws_its_messages_and_an_alert_box_its_alert() {
        let mut e = ChatEvent::new("twitch", "x", lightair_live::Kind::Message);
        e.text = "hello".into();
        e.user = crate::alerts::test_event("follow").user;
        let chat = [e];
        let (settings, _) = new_settings("chat-box").unwrap();
        let (px, size) = draw(
            &instance(&settings).unwrap(),
            (300, 200),
            &Feed {
                chat: &chat,
                alert: None,
                stored: None,
                now_ms: 0,
            },
        );
        assert_eq!(size, (300, 200));
        assert!(px.chunks(4).any(|p| p[3] > 0), "text drawn");
        let (settings, _) = new_settings("alert-box").unwrap();
        let empty = draw(
            &instance(&settings).unwrap(),
            (300, 200),
            &Feed {
                chat: &[],
                alert: None,
                stored: None,
                now_ms: 0,
            },
        )
        .0;
        assert!(
            empty.chunks(4).all(|p| p[3] == 0),
            "nothing without an alert"
        );
        let a =
            crate::alerts::from_event(&crate::alerts::test_event("follow"), &Default::default())
                .unwrap();
        let on = draw(
            &instance(&settings).unwrap(),
            (300, 200),
            &Feed {
                chat: &[],
                alert: Some(&a),
                stored: None,
                now_ms: 0,
            },
        )
        .0;
        assert!(on.chunks(4).any(|p| p[3] > 0));
    }

    #[test]
    fn a_subathon_timer_counts_as_the_widget_server_keeps_it() {
        let (settings, size) = new_settings("subathon").unwrap();
        let i = instance(&settings).unwrap();
        let v = values(&i);
        let never: Map<String, Value> = Map::new();
        assert_eq!(
            subathon_left(&v, &never, 0),
            60 * 60_000,
            "its starting minutes"
        );
        let running: Map<String, Value> =
            serde_json::from_value(serde_json::json!({"running": true, "endAt": 100_000})).unwrap();
        assert_eq!(subathon_left(&v, &running, 40_000), 60_000);
        assert_eq!(subathon_left(&v, &running, 200_000), 0, "never below zero");
        assert_eq!(
            (clock(3_723_000), clock(65_000)),
            ("1:02:03".into(), "1:05".into())
        );
        let added: Map<String, Value> = serde_json::from_value(
            serde_json::json!({"running": true, "endAt": 400_000, "lastAdded": {"ms": 300_000, "from": "fan", "at": 1_000}}),
        )
        .unwrap();
        let (px, _) = draw(
            &i,
            size,
            &Feed {
                chat: &[],
                alert: None,
                stored: Some(&added),
                now_ms: 2_000,
            },
        );
        assert!(px.chunks(4).any(|p| p[3] > 0));
    }
}
