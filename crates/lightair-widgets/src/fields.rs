//! A widget's settings, described as StreamElements describes them (`fields.json`: a name, then
//! its `type`, `label`, `value` and the rest), so an imported StreamElements widget's fields
//! need no translation. Lightair's own widgets use the same format. Order matters (it's the
//! Inspector's order), so schemas are read keeping it.

use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value};

/// One setting.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub key: String,
    /// StreamElements' types: "text", "textfield", "number", "slider", "checkbox",
    /// "colorpicker", "dropdown", "googleFont", "fontpicker", "image-input", "sound-input",
    /// "video-input", "button", "hidden".
    pub kind: String,
    pub label: String,
    pub default: Value,
    /// A dropdown's choices: (value, label).
    pub options: Vec<(String, String)>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    /// The Inspector's heading for it.
    pub group: String,
}

impl Field {
    pub fn is_shown(&self) -> bool {
        self.kind != "hidden"
    }
}

struct Ordered(Vec<(String, Value)>);

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V)
    }
}

/// The fields a schema describes, in its order (an unreadable schema has none).
pub fn parse(schema: &str) -> Vec<Field> {
    let Ok(Ordered(entries)) = serde_json::from_str::<Ordered>(schema) else { return Vec::new() };
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    entries
        .into_iter()
        .filter(|(_, v)| v.is_object())
        .map(|(key, v)| {
            let options = match &v["options"] {
                Value::Object(o) => {
                    o.iter().map(|(k, l)| (k.clone(), l.as_str().map_or_else(|| l.to_string(), String::from))).collect()
                }
                Value::Array(a) => a
                    .iter()
                    .map(|o| match o {
                        Value::Object(_) => (
                            o["value"].as_str().unwrap_or("").to_string(),
                            o["label"].as_str().unwrap_or("").to_string(),
                        ),
                        other => (
                            other.as_str().map_or_else(|| other.to_string(), String::from),
                            other.as_str().map_or_else(|| other.to_string(), String::from),
                        ),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            Field {
                label: v["label"].as_str().filter(|l| !l.is_empty()).unwrap_or(&key).to_string(),
                kind: v["type"].as_str().unwrap_or("text").to_string(),
                default: v.get("value").cloned().unwrap_or(Value::Null),
                options,
                min: num(&v["min"]),
                max: num(&v["max"]),
                step: num(&v["step"]),
                group: v["group"].as_str().unwrap_or("").to_string(),
                key,
            }
        })
        .collect()
}

/// The values a widget runs with: each field's default, then what was set.
pub fn values(fields: &[Field], set: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for f in fields {
        out.insert(f.key.clone(), f.default.clone());
    }
    if let Value::Object(set) = set {
        for (k, v) in set {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_keep_their_order_and_options() {
        let fields = parse(
            r#"{"zeta": {"type": "text", "label": "Title", "value": "Hi"},
                "alpha": {"type": "dropdown", "label": "Side", "value": "left", "options": {"left": "Left", "right": "Right"}},
                "size": {"type": "slider", "label": "Size", "value": 24, "min": 8, "max": "96", "step": 1}}"#,
        );
        assert_eq!(fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(), ["zeta", "alpha", "size"]);
        assert_eq!(fields[1].options, vec![("left".into(), "Left".into()), ("right".into(), "Right".into())]);
        assert_eq!((fields[2].min, fields[2].max), (Some(8.0), Some(96.0)));
        let v = values(&fields, &serde_json::json!({"size": 40}));
        assert_eq!((v["zeta"].as_str(), v["size"].as_i64()), (Some("Hi"), Some(40)));
    }
}
