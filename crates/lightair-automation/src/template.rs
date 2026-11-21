//! Templates: `{{ name }}`, `{{ user.login }}`, `{{ args.0 }}`, `{{ vars.deaths }}`, with
//! filters (`{{ name | upper }}`, `| lower`, `| default: someone`) and `{{ random 1 6 }}`.
//! Paths read the run's context (JSON); a missing value is empty.

use serde_json::Value;

fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut v = ctx;
    for part in path.split('.') {
        v = match part.parse::<usize>() {
            Ok(i) if v.is_array() => v.get(i)?,
            _ => v.get(part)?,
        };
    }
    Some(v)
}

pub fn text_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

pub fn random_below(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b) % n
}

fn expr(e: &str, ctx: &Value) -> String {
    let mut parts = e.split('|');
    let head = parts.next().unwrap_or("").trim();
    let mut out = if let Some(rest) = head.strip_prefix("random ") {
        let n: Vec<i64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        let (lo, hi) = (n.first().copied().unwrap_or(1), n.get(1).copied().unwrap_or(100));
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        (lo + random_below((hi - lo + 1) as u64) as i64).to_string()
    } else {
        lookup(ctx, head).map(text_of).unwrap_or_default()
    };
    for f in parts {
        let f = f.trim();
        let (name, arg) = f.split_once(':').map_or((f, ""), |(n, a)| (n.trim(), a.trim()));
        out = match name {
            "upper" => out.to_uppercase(),
            "lower" => out.to_lowercase(),
            "trim" => out.trim().to_string(),
            "default" if out.is_empty() => arg.to_string(),
            "strip_at" => out.trim_start_matches('@').to_string(),
            _ => out,
        };
    }
    out
}

/// `text` with every `{{ … }}` filled in from `ctx`.
pub fn render(text: &str, ctx: &Value) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        out.push_str(&expr(&after[..end], ctx));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Every string in `v` rendered (for a Lightair method's parameters).
pub fn render_value(v: &Value, ctx: &Value) -> Value {
    match v {
        Value::String(s) => {
            let r = render(s, ctx);
            // a parameter that's only a number after filling in is a number
            if s.contains("{{")
                && let Ok(n) = r.parse::<f64>()
            {
                return serde_json::json!(n);
            }
            Value::String(r)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| render_value(x, ctx)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), render_value(x, ctx))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_and_filters_fill_in() {
        let ctx = json!({"name": "Mochi", "args": ["@pal", "x"], "vars": {"deaths": 3.0}});
        assert_eq!(render("Hi {{ name }}! {{ args.0 | strip_at }} {{vars.deaths}}", &ctx), "Hi Mochi! pal 3");
        assert_eq!(render("{{ nobody | default: friend }} {{ name|upper }}", &ctx), "friend MOCHI");
    }

    #[test]
    fn a_random_number_stays_in_its_range() {
        for _ in 0..200 {
            let r: i64 = render("{{ random 3 1 }}", &json!({})).parse().unwrap();
            assert!((1..=3).contains(&r), "{r}");
        }
    }

    #[test]
    fn a_value_that_is_only_a_number_stays_a_number() {
        let ctx = json!({"vars": {"deaths": 3.0}});
        assert_eq!(render_value(&json!({"n": "{{vars.deaths}}", "s": "a"}), &ctx), json!({"n": 3.0, "s": "a"}));
    }
}
