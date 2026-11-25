//! The colour grade: a `filters.json` preset (`assets/effect-stack/filters.json`, the desktop's
//! list) parsed into [`GradeParams`], applied per colour by [`apply`] and baked into a 25³ LUT
//! strip ([`build_lut`]). The pass that applies it is `FilterLut` (`gpu/post.wgsl`).

use serde_json::Value;

/// The LUT's entries per axis.
pub const LUT_N: usize = 25;

const FILTERS_JSON: &str = include_str!("../../assets/effect-stack/filters.json");

/// A grade's settings; amounts roughly −1..1, hue in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeParams {
    pub exposure: f32,
    pub contrast: f32,
    pub curve: f32,
    pub saturation: f32,
    pub vibrance: f32,
    pub warmth: f32,
    pub tint: f32,
    pub hue: f32,
    pub fade: f32,
    pub rolloff: f32,
    /// Per-channel gamma (1 = neutral).
    pub gamma: [f32; 3],
    pub shadow_tint: [f32; 3],
    pub shadow_amt: f32,
    pub highlight_tint: [f32; 3],
    pub highlight_amt: f32,
    pub mono: bool,
    pub mono_tint: [f32; 3],
}

impl Default for GradeParams {
    /// Every amount off, gamma 1.
    fn default() -> Self {
        GradeParams {
            exposure: 0.0,
            contrast: 0.0,
            curve: 0.0,
            saturation: 0.0,
            vibrance: 0.0,
            warmth: 0.0,
            tint: 0.0,
            hue: 0.0,
            fade: 0.0,
            rolloff: 0.0,
            gamma: [1.0; 3],
            shadow_tint: [0.5; 3],
            shadow_amt: 0.0,
            highlight_tint: [0.5; 3],
            highlight_amt: 0.0,
            mono: false,
            mono_tint: [1.0; 3],
        }
    }
}

/// "#rrggbb" → 0..1, `out` unchanged otherwise.
pub fn parse_hex(hex: &str, out: &mut [f32; 3]) {
    let Some(digits) = hex.strip_prefix('#') else {
        return;
    };
    if hex.len() < 7 {
        return;
    }
    // strtoul: the leading hex digits, as an unsigned long, cast to unsigned
    let v = digits.chars().map_while(|c| c.to_digit(16)).fold(0u64, |a, d| a.wrapping_mul(16).wrapping_add(d as u64))
        as u32;
    out[0] = ((v >> 16) & 0xff) as f32 / 255.0;
    out[1] = ((v >> 8) & 0xff) as f32 / 255.0;
    out[2] = (v & 0xff) as f32 / 255.0;
}

/// `obs_data_get_double` on a preset object: 0 when missing.
pub(crate) fn num(d: &Value, k: &str) -> f32 {
    d.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32
}

/// `d[k]` as a "#rrggbb" colour into `out`, unchanged when absent or malformed.
pub(crate) fn hex(d: &Value, k: &str, out: &mut [f32; 3]) {
    if let Some(s) = d.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()) {
        parse_hex(s, out);
    }
}

/// `{"name": name}` in `root[list]` or, with
/// categories, `root.categories[*][list]`.
pub(crate) fn find_in<'a>(root: &'a Value, list: &str, name: &str) -> Option<&'a Value> {
    if name.is_empty() {
        return None;
    }
    let named =
        |arr: &'a Value| arr.as_array()?.iter().find(|it| it.get("name").and_then(Value::as_str).unwrap_or("") == name);
    if let Some(cats) = root.get("categories").and_then(Value::as_array) {
        return cats.iter().find_map(|c| c.get(list).and_then(named));
    }
    root.get(list).and_then(named)
}

/// A preset's grade settings, the defaults for any it leaves out.
pub fn parse(d: &Value) -> GradeParams {
    let mut g = GradeParams {
        exposure: num(d, "exposure"),
        contrast: num(d, "contrast"),
        curve: num(d, "curve"),
        saturation: num(d, "saturation"),
        vibrance: num(d, "vibrance"),
        warmth: num(d, "warmth"),
        tint: num(d, "tint"),
        hue: num(d, "hue"),
        fade: num(d, "fade"),
        rolloff: num(d, "rolloff"),
        ..Default::default()
    };
    g.gamma[0] *= 1.0 + num(d, "gamma_r");
    g.gamma[1] *= 1.0 + num(d, "gamma_g");
    g.gamma[2] *= 1.0 + num(d, "gamma_b");
    hex(d, "shadows", &mut g.shadow_tint);
    g.shadow_amt = num(d, "shadows_amt");
    hex(d, "highlights", &mut g.highlight_tint);
    g.highlight_amt = num(d, "highlights_amt");
    g.mono = d.get("mono").and_then(Value::as_bool).unwrap_or(false);
    hex(d, "mono_tint", &mut g.mono_tint);
    g
}

fn filters() -> &'static Value {
    static ROOT: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| serde_json::from_str(FILTERS_JSON).unwrap_or(Value::Null))
}

/// The grades by category, as the desktop lists them: (category, name).
pub fn names() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in filters()["categories"].as_array().into_iter().flatten() {
        let cat = c["name"].as_str().unwrap_or("");
        for f in c["filters"].as_array().into_iter().flatten() {
            if let Some(n) = f["name"].as_str() {
                out.push((cat.to_string(), n.to_string()));
            }
        }
    }
    out
}

/// The named filters.json preset, if there is one.
pub fn preset(name: &str) -> Option<GradeParams> {
    find_in(filters(), "filters", name).map(parse)
}

/// Rec. 601 luma.
fn luma(c: &[f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

/// Applies the grade in place, on sRGB-encoded 0..1.
pub fn apply(g: &GradeParams, c: &mut [f32; 3]) {
    // exposure + white balance
    let e = (g.exposure * 0.8).exp2();
    c[0] *= e * (1.0 + g.warmth * 0.12);
    c[1] *= e * (1.0 - g.tint * 0.07);
    c[2] *= e * (1.0 - g.warmth * 0.12);
    c[0] *= 1.0 + g.tint * 0.03;
    c[2] *= 1.0 + g.tint * 0.03;

    // highlight roll-off (filmic shoulder)
    for v in c.iter_mut() {
        let mut x = *v;
        if g.rolloff > 0.0 && x > 0.7 {
            let over = x - 0.7;
            x = 0.7 + over / (1.0 + over * g.rolloff * 4.0);
        }
        *v = x.clamp(0.0, 1.0);
    }

    // contrast: S-curve for positive values, flatten towards mid grey for negative
    let k = g.contrast + g.curve;
    for v in c.iter_mut() {
        let x = *v;
        let s = x * x * (3.0 - 2.0 * x);
        *v = if k >= 0.0 { x + (s - x) * k } else { x + (0.5 - x) * (-k * 0.4) };
    }

    // hue rotation (YIQ)
    if g.hue != 0.0 {
        let y = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
        let i = 0.596 * c[0] - 0.274 * c[1] - 0.322 * c[2];
        let q = 0.211 * c[0] - 0.523 * c[1] + 0.312 * c[2];
        let a = g.hue * std::f32::consts::PI / 180.0;
        let (cs, sn) = (a.cos(), a.sin());
        let (i2, q2) = (i * cs - q * sn, i * sn + q * cs);
        c[0] = y + 0.956 * i2 + 0.621 * q2;
        c[1] = y - 0.272 * i2 - 0.647 * q2;
        c[2] = y - 1.106 * i2 + 1.703 * q2;
    }

    // saturation + vibrance (boosts muted colours more, protects saturated ones)
    let y = luma(c);
    let mx = c[0].max(c[1].max(c[2]));
    let mn = c[0].min(c[1].min(c[2]));
    let sat_now = mx - mn;
    let f = (1.0 + g.saturation) * (1.0 + g.vibrance * (1.0 - (sat_now * 2.0).clamp(0.0, 1.0)));
    for v in c.iter_mut() {
        *v = y + (*v - y) * f;
    }

    // split toning
    let y = luma(c).clamp(0.0, 1.0);
    let (ws, wh) = ((1.0 - y) * (1.0 - y), y * y);
    for (i, v) in c.iter_mut().enumerate() {
        *v += (g.shadow_tint[i] - 0.5) * g.shadow_amt * ws + (g.highlight_tint[i] - 0.5) * g.highlight_amt * wh;
    }

    // per-channel gamma
    for (i, v) in c.iter_mut().enumerate() {
        *v = v.clamp(0.0, 1.0).powf(1.0 / g.gamma[i].max(0.1));
    }

    if g.mono {
        let m = luma(c);
        for (i, v) in c.iter_mut().enumerate() {
            *v = m * g.mono_tint[i] / luma(&g.mono_tint).max(0.05) * 0.92 + m * 0.08;
        }
    }

    // fade: lifted blacks, slightly lowered whites
    for v in c.iter_mut() {
        *v = (*v * (1.0 - g.fade * 0.2) + g.fade * 0.11).clamp(0.0, 1.0);
    }
}

/// The grade baked into n³ entries as an (n·n)×n RGBA8 strip,
/// x = r + b·n, y = g.
pub fn build_lut(g: &GradeParams, n: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; n * n * n * 4];
    let d = (n - 1) as f32;
    for b in 0..n {
        for gg in 0..n {
            for r in 0..n {
                let mut c = [r as f32 / d, gg as f32 / d, b as f32 / d];
                apply(g, &mut c);
                let o = (gg * n * n + b * n + r) * 4;
                for i in 0..3 {
                    rgba[o + i] = (c[i].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
                rgba[o + 3] = 255;
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(lut: &[u8], n: usize, r: usize, g: usize, b: usize) -> [u8; 4] {
        let o = (g * n * n + b * n + r) * 4;
        lut[o..o + 4].try_into().unwrap()
    }

    #[test]
    fn every_preset_parses_and_unknown_names_are_off() {
        let root: Value = serde_json::from_str(FILTERS_JSON).unwrap();
        for c in root["categories"].as_array().unwrap() {
            for f in c["filters"].as_array().unwrap() {
                assert!(preset(f["name"].as_str().unwrap()).is_some(), "{f}");
            }
        }
        assert!(preset("").is_none(), "an empty preset is off");
        assert!(preset("portra").is_none(), "names match exactly");
        let portra = preset("Portra").unwrap();
        assert_eq!(portra.gamma.len(), 3);
    }

    #[test]
    fn the_lut_keeps_black_white_and_grey_where_the_grade_keeps_them() {
        let n = LUT_N;
        // Pop (contrast, saturation, vibrance): an S-curve and saturation about luma keep black,
        // white and every grey's ends
        let pop = preset("Pop").unwrap();
        let lut = build_lut(&pop, n);
        assert_eq!(lut.len(), n * n * n * 4);
        assert_eq!(at(&lut, n, 0, 0, 0), [0, 0, 0, 255]);
        assert_eq!(at(&lut, n, n - 1, n - 1, n - 1), [255, 255, 255, 255]);
        // mid grey stays grey (no tint, no split toning)
        let g = at(&lut, n, 12, 12, 12);
        assert!(g[0] == g[1] && g[1] == g[2], "{g:?}");
        // saturated red gets more saturated: red stays at 255, green and blue at 0
        assert_eq!(at(&lut, n, n - 1, 0, 0), [255, 0, 0, 255]);

        // neutral parameters are the identity everywhere
        let id = build_lut(&GradeParams::default(), n);
        for (r, g, b) in [(0, 0, 0), (24, 0, 0), (0, 24, 0), (0, 0, 24), (24, 24, 24), (3, 17, 9)] {
            let want = |v: usize| (v as f32 / 24.0 * 255.0 + 0.5) as u8;
            assert_eq!(at(&id, n, r, g, b), [want(r), want(g), want(b), 255]);
        }

        // a faded preset lifts black: Soft (contrast −0.25, fade 0.15)
        let soft = preset("Soft").unwrap();
        let mut black = [0.0f32; 3];
        apply(&soft, &mut black);
        let flat = 0.0 + (0.5 - 0.0) * (0.25 * 0.4);
        let want = flat * (1.0 - 0.15 * 0.2) + 0.15 * 0.11;
        assert!((black[0] - want).abs() < 1e-6, "{black:?} vs {want}");
        let lut = build_lut(&soft, n);
        let b = (want * 255.0 + 0.5) as u8;
        assert_eq!(at(&lut, n, 0, 0, 0), [b, b, b, 255]);
    }

    #[test]
    fn a_hex_colour_needs_its_hash_and_six_digits() {
        let mut c = [0.5; 3];
        parse_hex("#ff8000", &mut c);
        assert_eq!(c, [1.0, 128.0 / 255.0, 0.0]);
        let mut c = [0.5; 3];
        parse_hex("ff8000", &mut c);
        assert_eq!(c, [0.5; 3], "no #: unchanged");
        parse_hex("#fff", &mut c);
        assert_eq!(c, [0.5; 3], "too short: unchanged");
    }
}
