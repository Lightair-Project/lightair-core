//! Keyframes per property (CapCut's ◇ on every field): a clip's transform, filter, crop and
//! effect settings each change over time on keys of their own, eased between. A property
//! without keys keeps its one value. (Volume has its own keys, `Clip::volume_keys`.)
//!
//! Property names: "x", "y", "scale", "rotation", "opacity"; "brightness", "contrast",
//! "saturation", "temperature", "vignette", "blur"; "crop.l", "crop.t", "crop.r", "crop.b";
//! "fx.<n>.<setting>" (the clip's n-th effect).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sequence::{Clip, ClipEffect, Filter, Transform};

/// A property's value at a moment of a clip (ms from its start, timeline time).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Key {
    pub at_ms: u64,
    pub v: f32,
}

/// Keys by property.
pub type Anim = BTreeMap<String, Vec<Key>>;

pub const TRANSFORM: [&str; 5] = ["x", "y", "scale", "rotation", "opacity"];
pub const FILTER: [&str; 6] = ["brightness", "contrast", "saturation", "temperature", "vignette", "blur"];
pub const CROP: [&str; 4] = ["crop.l", "crop.t", "crop.r", "crop.b"];

/// Two keys this close (ms) are one.
const NEAR: u64 = 40;

fn ease(k: f32) -> f32 {
    let k = k.clamp(0.0, 1.0);
    k * k * (3.0 - 2.0 * k)
}

/// The value of `keys` at `at` (ms from the clip's start): held before the first and after the
/// last, eased between.
pub fn value(keys: &[Key], at: u64) -> Option<f32> {
    let (first, last) = (keys.first()?, keys.last()?);
    if at <= first.at_ms {
        return Some(first.v);
    }
    if at >= last.at_ms {
        return Some(last.v);
    }
    let i = keys.partition_point(|k| k.at_ms <= at);
    let (a, b) = (&keys[i - 1], &keys[i]);
    let k = ease((at - a.at_ms) as f32 / (b.at_ms - a.at_ms).max(1) as f32);
    Some(a.v + (b.v - a.v) * k)
}

impl Clip {
    fn rel(&self, t: u64) -> u64 {
        t.saturating_sub(self.start_ms).min(self.len_ms())
    }

    /// The keys of property `p`.
    pub fn keys_of(&self, p: &str) -> &[Key] {
        self.anim.get(p).map_or(&[], Vec::as_slice)
    }

    /// Property `p` at timeline time `t`: its keys there, else `base`.
    pub fn prop_at(&self, p: &str, t: u64, base: f32) -> f32 {
        value(self.keys_of(p), t.saturating_sub(self.start_ms)).unwrap_or(base)
    }

    /// Whether property `p` has a key at `t`.
    pub fn has_key(&self, p: &str, t: u64) -> bool {
        let at = self.rel(t);
        self.keys_of(p).iter().any(|k| k.at_ms.abs_diff(at) < NEAR)
    }

    /// Sets property `p` at `t`: its key there (made if needed) when it has keys; false when it
    /// has none (the caller sets its one value).
    pub fn set_key(&mut self, p: &str, t: u64, v: f32) -> bool {
        self.migrate_keyframes();
        let at = self.rel(t);
        let Some(keys) = self.anim.get_mut(p).filter(|k| !k.is_empty()) else { return false };
        match keys.iter_mut().find(|k| k.at_ms.abs_diff(at) < NEAR) {
            Some(k) => k.v = v,
            None => {
                keys.push(Key { at_ms: at, v });
                keys.sort_by_key(|k| k.at_ms);
            }
        }
        true
    }

    /// A key for property `p` at `t` (its value there), or the one there taken away. Its value
    /// without keys is `base`; when the last key goes, that's the value it keeps.
    pub fn toggle_key(&mut self, p: &str, t: u64, base: &mut f32) {
        self.migrate_keyframes();
        let at = self.rel(t);
        let now = self.prop_at(p, t, *base);
        let keys = self.anim.entry(p.to_string()).or_default();
        match keys.iter().position(|k| k.at_ms.abs_diff(at) < NEAR) {
            Some(i) => {
                let gone = keys.remove(i);
                if keys.is_empty() {
                    *base = gone.v;
                    self.anim.remove(p);
                }
            }
            None => {
                keys.push(Key { at_ms: at, v: now });
                keys.sort_by_key(|k| k.at_ms);
            }
        }
    }

    /// Where its keys are (ms from its start): every property's, its volume's.
    pub fn key_times(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.anim.values().flatten().map(|k| k.at_ms).chain(self.keyframes.iter().map(|k| k.at_ms)).collect();
        v.sort();
        v.dedup_by(|a, b| a.abs_diff(*b) < NEAR);
        v
    }

    /// Keyframes of the whole transform (the older kind) as keys per property.
    pub fn migrate_keyframes(&mut self) {
        if self.keyframes.is_empty() {
            return;
        }
        for (i, p) in TRANSFORM.iter().enumerate() {
            let keys = self.keyframes.iter().map(|k| Key { at_ms: k.at_ms, v: transform_field(&k.transform, i) }).collect();
            self.anim.insert(p.to_string(), keys);
        }
        if let Some(first) = self.keyframes.first() {
            self.transform = first.transform;
        }
        self.keyframes.clear();
    }

    /// Its filter at `t` (keyed settings at their values there).
    pub fn filter_at(&self, t: u64) -> Filter {
        let f = self.filter;
        Filter {
            brightness: self.prop_at("brightness", t, f.brightness),
            contrast: self.prop_at("contrast", t, f.contrast),
            saturation: self.prop_at("saturation", t, f.saturation),
            temperature: self.prop_at("temperature", t, f.temperature),
            vignette: self.prop_at("vignette", t, f.vignette),
            blur: self.prop_at("blur", t, f.blur),
        }
    }

    /// Its crop at `t`.
    pub fn crop_at(&self, t: u64) -> (f32, f32, f32, f32) {
        let c = self.crop;
        (self.prop_at("crop.l", t, c.0), self.prop_at("crop.t", t, c.1), self.prop_at("crop.r", t, c.2), self.prop_at("crop.b", t, c.3))
    }

    /// Its effects at `t` (keyed settings at their values there).
    pub fn effects_at(&self, t: u64) -> Vec<ClipEffect> {
        let mut out = self.effects.clone();
        for (n, e) in out.iter_mut().enumerate() {
            let prefix = format!("fx.{n}.");
            for (p, keys) in self.anim.range(prefix.clone()..) {
                let Some(setting) = p.strip_prefix(&prefix) else { break };
                if let Some(v) = value(keys, t.saturating_sub(self.start_ms)) {
                    e.settings.insert(setting.to_string(), serde_json::json!(v));
                }
            }
        }
        out
    }

    /// Its keys moved by `by` ms (its start moved the other way: they stay put on the timeline).
    pub(crate) fn shift_keys(&mut self, by: i64) {
        for keys in self.anim.values_mut() {
            for k in keys.iter_mut() {
                k.at_ms = (k.at_ms as i64 + by).max(0) as u64;
            }
        }
    }

    /// Its keys cut at `cut` (ms from its start): those before, and those after (from the cut),
    /// each side with a key at the cut holding the value there.
    pub(crate) fn split_keys(&self, cut: u64) -> (Anim, Anim) {
        let (mut before, mut after) = (Anim::new(), Anim::new());
        for (p, keys) in &self.anim {
            let here = value(keys, cut).unwrap_or(0.0);
            let mut b: Vec<Key> = keys.iter().filter(|k| k.at_ms < cut).copied().collect();
            b.push(Key { at_ms: cut, v: here });
            let mut a = vec![Key { at_ms: 0, v: here }];
            a.extend(keys.iter().filter(|k| k.at_ms > cut).map(|k| Key { at_ms: k.at_ms - cut, v: k.v }));
            before.insert(p.clone(), b);
            after.insert(p.clone(), a);
        }
        (before, after)
    }
}

/// A transform's field by its place in [`TRANSFORM`].
pub fn transform_field(t: &Transform, i: usize) -> f32 {
    match i {
        0 => t.x,
        1 => t.y,
        2 => t.scale,
        3 => t.rotation,
        _ => t.opacity,
    }
}

pub fn set_transform_field(t: &mut Transform, i: usize, v: f32) {
    match i {
        0 => t.x = v,
        1 => t.y = v,
        2 => t.scale = v,
        3 => t.rotation = v,
        _ => t.opacity = v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightair_core::Id;

    #[test]
    fn properties_key_on_their_own() {
        let mut c = Clip::new(Id::nil(), 1_000, 4_000);
        // scale keyed: 1 at the start, 2 at 2 s in; x isn't
        let mut base = c.transform.scale;
        c.toggle_key("scale", 1_000, &mut base);
        assert!(c.set_key("scale", 3_000, 2.0));
        assert!(!c.set_key("x", 3_000, 50.0), "x has no keys: it keeps one value");
        assert_eq!(c.transform_at(2_000).scale, 1.5, "halfway, eased");
        assert_eq!(c.transform_at(4_000).scale, 2.0);
        assert!(c.has_key("scale", 3_000) && !c.has_key("scale", 2_000));
        // the brightness keyed apart from it
        let mut b = c.filter.brightness;
        c.toggle_key("brightness", 1_000, &mut b);
        c.set_key("brightness", 5_000, 0.5);
        assert!((c.filter_at(3_000).brightness - 0.25).abs() < 1e-6);
        assert_eq!(c.key_times(), vec![0, 2_000, 4_000]);
        // the last key taken away: its value stays
        let mut s = c.transform.scale;
        c.toggle_key("scale", 3_000, &mut s);
        c.toggle_key("scale", 1_000, &mut s);
        assert!(c.keys_of("scale").is_empty() && s == 1.0);
    }

    #[test]
    fn whole_transform_keyframes_become_keys() {
        use crate::sequence::Keyframe;
        let mut c = Clip::new(Id::nil(), 0, 4_000);
        c.keyframes = vec![
            Keyframe { at_ms: 0, transform: Transform { x: 0.0, ..Transform::default() } },
            Keyframe { at_ms: 4_000, transform: Transform { x: 400.0, ..Transform::default() } },
        ];
        let before = c.transform_at(1_000);
        c.migrate_keyframes();
        assert!(c.keyframes.is_empty());
        assert_eq!(c.transform_at(1_000), before);
    }
}
