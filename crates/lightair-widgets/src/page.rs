//! A widget's page: a built-in widget's own, or an imported StreamElements widget's HTML, CSS
//! and JS put together with its `{{field}}`s filled in, the bridge and the StreamElements shim.

use serde_json::{Map, Value};

use crate::{Instance, catalog, fields};

pub const BRIDGE: &str = include_str!("../assets/lightair.js");
pub const SE_SHIM: &str = include_str!("../assets/se-shim.js");
pub const JQUERY: &str = include_str!("../assets/jquery-3.7.1.min.js");

/// `{{name}}` (spaces allowed inside) → the field's value.
pub fn substitute(text: &str, values: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        match values.get(name) {
            Some(Value::String(s)) => out.push_str(s),
            Some(Value::Null) | None if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') => {}
            Some(v) => out.push_str(&v.to_string()),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

pub fn build(i: &Instance) -> String {
    if let Some(page) = catalog::page(&i.kind) {
        return page.to_string();
    }
    let Some(c) = &i.custom else { return "<!doctype html><title>Widget</title>".into() };
    let values = fields::values(&fields::parse(&c.fields), &i.fields);
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8">
<style>html, body {{ margin: 0; background: transparent; overflow: hidden; }}</style>
<style>{css}</style>
<script>{jquery}</script>
<script src="/lightair.js"></script>
<script src="/se-shim.js"></script>
</head><body>
{html}
<script>{js}</script>
</body></html>"#,
        css = substitute(&c.css, &values),
        jquery = JQUERY,
        html = substitute(&c.html, &values),
        js = substitute(&c.js, &values),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_fill_in() {
        let mut v = Map::new();
        v.insert("color".into(), Value::String("#ff0".into()));
        v.insert("size".into(), Value::from(24));
        assert_eq!(substitute("a{ color: {{color}}; font-size: {{ size }}px }", &v), "a{ color: #ff0; font-size: 24px }");
        // unknown names are left as they were written when they aren't names at all
        assert_eq!(substitute("x {{ a b }} y", &v), "x {{ a b }} y");
        assert_eq!(substitute("x {{missing}} y", &v), "x  y");
    }
}
