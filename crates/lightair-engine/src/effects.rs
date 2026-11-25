//! Per-layer effects with presets, as the desktop does them: the catalogue's effects
//! that phones draw (lightair-render's `fx.*` shaders, the colour filter, `.cube` LUTs), each
//! with its controls, added to a layer's source, changed, switched off, reordered; and presets:
//! named settings of an effect kept for the whole app. No one-tap "looks".

use std::path::{Path, PathBuf};

use lightair_model::catalog::{self, Control, EffectDef, Pick};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Whether phones draw an effect kind (the background effects on cameras, with the host's
/// person masks: `background.rs`).
pub fn drawn(kind: &str) -> bool {
    kind.starts_with("fx.")
        || kind == "color_filter_v2"
        || kind == "clut_filter"
        || crate::background::KINDS.contains(&kind)
        || crate::keying::KINDS.contains(&kind)
        || LOOKS.contains(&kind)
}

/// The looks phones draw (`looks`, the desktop's own passes).
pub const LOOKS: [&str; 3] = ["studio.beauty", "studio.light", "studio.grade"];

/// Controls the desktop has that phones don't use: which macOS model segments, the colour its
/// own effect keys out, and light wrap (not drawn on phones yet).
const DESKTOP_ONLY: [&str; 3] = ["bg_model", "bg_key", "bg_wrap"];

/// The effects phones draw, in the catalogue's order.
pub fn kinds() -> Vec<&'static EffectDef> {
    catalog::CATALOG.iter().filter(|d| drawn(d.kind)).collect()
}

fn pick(p: Pick) -> Value {
    p.json()
}

/// An effect's controls as JSON, for the interface's form: `[{type, key, label, …}]`.
pub fn controls_json(d: &EffectDef) -> Value {
    let mut controls: Vec<Value> = d
        .controls
        .iter()
        .filter(|c| !c.key().is_some_and(|k| DESKTOP_ONLY.contains(&k)))
        .map(|c| match c {
            Control::Heading(label) => json!({ "type": "heading", "label": label }),
            Control::Slider(s) => json!({
                "type": "slider", "key": s.key, "label": s.label, "min": s.min, "max": s.max,
                "step": s.step, "default": s.default, "unit": format!("{:?}", s.unit).to_lowercase(), "int": s.int,
            }),
            Control::Choice { key, label, options, default } => json!({
                "type": "choice", "key": key, "label": label, "default": pick(*default),
                "options": options.iter().map(|(l, v)| json!({ "label": l, "value": pick(*v) })).collect::<Vec<_>>(),
            }),
            Control::Colour { key, label, default } => {
                json!({ "type": "colour", "key": key, "label": label, "default": default, "format": "0xAABBGGRR" })
            }
            // the grades phones draw, as a choice
            Control::Presets { key, label, default, set: catalog::Presets::Grade } => json!({
                "type": "choice", "key": key, "label": label, "default": default,
                "options": crate::looks::grade::names().into_iter().map(|(cat, name)| json!({
                    "label": format!("{cat} · {name}"), "value": name,
                })).collect::<Vec<_>>(),
            }),
            Control::Presets { key, label, default, set: catalog::Presets::Light } => json!({
                "type": "choice", "key": key, "label": label, "default": default,
                "options": crate::looks::render::light_names().into_iter().map(|n| json!({
                    "label": n, "value": n,
                })).collect::<Vec<_>>(),
            }),
            // the makeup looks, as a choice: None, then each look
            Control::Presets { key, label, default, set: catalog::Presets::Makeup } => json!({
                "type": "choice", "key": key, "label": label, "default": default,
                "options": std::iter::once("None".to_string())
                    .chain(crate::looks::render::makeup_names())
                    .map(|n| json!({ "label": n, "value": n }))
                    .collect::<Vec<_>>(),
            }),
            Control::Presets { key, label, default, .. } => {
                json!({ "type": "presets", "key": key, "label": label, "default": default })
            }
            Control::File { key, label, extensions } => {
                json!({ "type": "file", "key": key, "label": label, "extensions": extensions })
            }
            Control::Switch { key, label, default } => {
                json!({ "type": "switch", "key": key, "label": label, "default": default })
            }
            Control::Track { key, label } => json!({ "type": "track", "key": key, "label": label }),
        })
        .collect();
    // the Looks screen's part switches, read by the looks' settings
    if d.kind == "studio.beauty" {
        for (key, label) in [("skin_on", "Skin"), ("shape_on", "Shape"), ("makeup_on", "Makeup")] {
            controls.push(json!({ "type": "switch", "key": key, "label": label, "default": true }));
        }
    }
    Value::Array(controls)
}

/// A named setting of an effect, kept for the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub kind: String,
    pub settings: Value,
}

fn file(dir: &Path) -> PathBuf {
    dir.join("effect-presets.json")
}

pub fn presets(dir: &Path) -> Vec<Preset> {
    std::fs::read(file(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Keeps `preset` (one of the same name and kind is replaced).
pub fn save_preset(dir: &Path, preset: Preset) -> std::io::Result<()> {
    let mut all = presets(dir);
    all.retain(|p| !(p.name == preset.name && p.kind == preset.kind));
    all.push(preset);
    write(dir, &all)
}

pub fn delete_preset(dir: &Path, kind: &str, name: &str) -> std::io::Result<()> {
    let mut all = presets(dir);
    all.retain(|p| !(p.name == name && p.kind == kind));
    write(dir, &all)
}

fn write(dir: &Path, all: &[Preset]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = file(dir).with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(all).unwrap_or_default())?;
    std::fs::rename(tmp, file(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_offers_only_the_effects_and_controls_it_draws() {
        let k = kinds();
        assert!(k.iter().any(|d| d.kind == "fx.vignette") && k.iter().any(|d| d.kind == "color_filter_v2"));
        // of the desktop's Studio effects: the background ones (the host's person masks) and
        // the looks (beauty, light, grade)
        assert!(
            k.iter()
                .filter(|d| d.kind.starts_with("studio."))
                .all(|d| crate::background::KINDS.contains(&d.kind) || LOOKS.contains(&d.kind)),
        );
        let bg = controls_json(catalog::def("studio.bg_replace").unwrap());
        let keys: Vec<&str> = bg.as_array().unwrap().iter().filter_map(|c| c["key"].as_str()).collect();
        assert!(keys.contains(&"bg_color") && keys.contains(&"bg_quality"));
        assert!(!keys.contains(&"bg_model") && !keys.contains(&"bg_wrap"), "{keys:?}");
        let v = controls_json(catalog::def("fx.vignette").unwrap());
        assert!(v.as_array().unwrap().iter().any(|c| c["type"] == "slider"));
    }

    #[test]
    fn a_preset_is_saved_replaced_by_its_name_and_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Preset { name: "Soft".into(), kind: "fx.vignette".into(), settings: json!({ "amount": 0.3 }) };
        save_preset(tmp.path(), p.clone()).unwrap();
        save_preset(tmp.path(), Preset { settings: json!({ "amount": 0.4 }), ..p.clone() }).unwrap();
        assert_eq!(presets(tmp.path()).len(), 1, "replaced, not added");
        delete_preset(tmp.path(), "fx.vignette", "Soft").unwrap();
        assert!(presets(tmp.path()).is_empty());
    }
}
