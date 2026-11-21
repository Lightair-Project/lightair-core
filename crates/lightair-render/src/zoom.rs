//! Zooms (docs/plans/screen-studio.md, 4.3): automatic ones from what the take's input log
//! shows, and the camera that moves between them.
//!
//! **Automatic zooms.** Clicks and typing that come close together are one moment of attention:
//! the zoom starts a little before the first, holds a little after the last, and looks at where
//! they happened (or follows the pointer when they're spread out). Moments close together merge.
//!
//! **The camera.** Scale and centre follow their targets on critically damped springs, stepped
//! on a fixed clock from the start, so the same edit always gives the same frames (and the
//! Editor's preview matches the export). The view never leaves the screen.

use lightair_store::input_log::{Entry, Log};

use crate::edit::{Focus, Zoom};

/// Before the first click of a moment, the zoom is already on its way.
const LEAD_MS: u64 = 700;
/// After the last one, it holds.
const HOLD_MS: u64 = 1_800;
/// Clicks and keys this close are one moment.
const GAP_MS: u64 = 2_500;
/// Zooms this close become one.
const MERGE_MS: u64 = 900;
/// A moment spread over more than this much of the screen follows the pointer instead.
const SPREAD: f32 = 0.35;
pub const DEFAULT_SCALE: f32 = 2.0;

/// The input log's display frame (points): where its positions are measured from.
fn display(log: &Log) -> (f32, f32, f32, f32) {
    let mut origin = (0.0, 0.0);
    let mut size = (1.0, 1.0);
    for r in &log.records {
        match r.entry {
            Entry::Display { x, y } => origin = (x, y),
            Entry::DisplaySize { w, h } => size = (w.max(1.0), h.max(1.0)),
            _ => {}
        }
    }
    (origin.0, origin.1, size.0, size.1)
}

/// A position in the log as a fraction of the screen.
fn norm(log: &Log, x: f32, y: f32) -> (f32, f32) {
    let (ox, oy, w, h) = display(log);
    (((x - ox) / w).clamp(0.0, 1.0), ((y - oy) / h).clamp(0.0, 1.0))
}

/// Automatic zooms for a take `duration_ms` long.
pub fn auto(log: &Log, duration_ms: u64) -> Vec<Zoom> {
    // moments of attention: clicks where they were, keys where the last click was
    let mut events: Vec<(u64, (f32, f32))> = Vec::new();
    let mut last_click = None;
    for r in &log.records {
        let t = r.at.as_millis() as u64;
        match r.entry {
            Entry::Down { x, y, .. } => {
                let p = norm(log, x, y);
                last_click = Some(p);
                events.push((t, p));
            }
            Entry::KeyDown { .. } => {
                if let Some(p) = last_click.or_else(|| log.pointer_at(r.at).map(|(x, y)| norm(log, x, y))) {
                    events.push((t, p));
                }
            }
            _ => {}
        }
    }
    let mut clusters: Vec<Vec<(u64, (f32, f32))>> = Vec::new();
    for e in events {
        match clusters.last_mut() {
            Some(c) if e.0 - c.last().map_or(0, |l| l.0) <= GAP_MS => c.push(e),
            _ => clusters.push(vec![e]),
        }
    }
    let mut zooms: Vec<Zoom> = Vec::new();
    for c in clusters {
        let (first, last) = (c[0].0, c[c.len() - 1].0);
        let n = c.len() as f32;
        let (cx, cy) = c.iter().fold((0.0, 0.0), |a, e| (a.0 + e.1.0 / n, a.1 + e.1.1 / n));
        let spread = c.iter().map(|e| (e.1.0 - cx).abs().max((e.1.1 - cy).abs())).fold(0.0f32, f32::max);
        let zoom = Zoom {
            start_ms: first.saturating_sub(LEAD_MS),
            end_ms: (last + HOLD_MS).min(duration_ms),
            scale: DEFAULT_SCALE,
            focus: if spread > SPREAD { Focus::Pointer } else { Focus::Point { x: cx, y: cy } },
            auto: true,
        };
        match zooms.last_mut() {
            Some(prev) if zoom.start_ms <= prev.end_ms + MERGE_MS => {
                prev.end_ms = zoom.end_ms;
                // two places: follow the pointer between them
                if prev.focus != zoom.focus {
                    prev.focus = Focus::Pointer;
                }
            }
            _ => zooms.push(zoom),
        }
    }
    zooms.retain(|z| z.end_ms > z.start_ms);
    zooms
}

/// The camera at one moment: the screen magnified `scale` times around (`x`, `y`) (fractions
/// of the screen).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub scale: f32,
    pub x: f32,
    pub y: f32,
}

impl View {
    pub const WHOLE: View = View { scale: 1.0, x: 0.5, y: 0.5 };

    /// The part of the screen shown: (left, top, width, height) as fractions.
    pub fn rect(&self) -> (f32, f32, f32, f32) {
        let s = 1.0 / self.scale;
        (self.x - s / 2.0, self.y - s / 2.0, s, s)
    }
}

/// The camera through a whole take, sampled at 240 Hz.
pub struct Camera {
    samples: Vec<View>,
}

const HZ: f64 = 240.0;

fn spring(x: f32, v: f32, target: f32, omega: f32, dt: f32) -> (f32, f32) {
    let f = 1.0 + 2.0 * dt * omega;
    let oo = omega * omega;
    let (hoo, hhoo) = (dt * oo, dt * dt * oo);
    let inv = 1.0 / (f + hhoo);
    ((f * x + dt * v + hhoo * target) * inv, (v + hoo * (target - x)) * inv)
}

impl Camera {
    /// Steps the camera through `duration_ms` of source time. `pointer(t_ms)` gives the pointer
    /// as fractions of the screen (for zooms that follow it).
    pub fn build(zooms: &[Zoom], duration_ms: u64, pointer: impl Fn(u64) -> Option<(f32, f32)>) -> Camera {
        let n = (duration_ms as f64 / 1000.0 * HZ).ceil() as usize + 1;
        let dt = (1.0 / HZ) as f32;
        let mut samples = Vec::with_capacity(n);
        let (mut s, mut x, mut y) = (1.0f32, 0.5f32, 0.5f32);
        let (mut vs, mut vx, mut vy) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..n {
            let t = (i as f64 / HZ * 1000.0) as u64;
            let zoom = zooms.iter().find(|z| z.start_ms <= t && t < z.end_ms);
            let (ts, tx, ty) = match zoom {
                Some(z) => {
                    let (fx, fy) = match z.focus {
                        Focus::Point { x, y } => (x, y),
                        Focus::Pointer => pointer(t).unwrap_or((x, y)),
                    };
                    (z.scale.max(1.0), fx, fy)
                }
                None => (1.0, 0.5, 0.5),
            };
            (s, vs) = spring(s, vs, ts, 7.0, dt);
            (x, vx) = spring(x, vx, tx, 5.0, dt);
            (y, vy) = spring(y, vy, ty, 5.0, dt);
            // the view stays on the screen
            let half = 0.5 / s.max(1.0);
            samples.push(View { scale: s.max(1.0), x: x.clamp(half, 1.0 - half), y: y.clamp(half, 1.0 - half) });
        }
        Camera { samples }
    }

    /// The view at source time `t_ms`.
    pub fn at(&self, t_ms: u64) -> View {
        let f = t_ms as f64 / 1000.0 * HZ;
        let i = f.floor() as usize;
        let Some(a) = self.samples.get(i) else { return self.samples.last().copied().unwrap_or(View::WHOLE) };
        let Some(b) = self.samples.get(i + 1) else { return *a };
        let k = (f - i as f64) as f32;
        View { scale: a.scale + (b.scale - a.scale) * k, x: a.x + (b.x - a.x) * k, y: a.y + (b.y - a.y) * k }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightair_store::input_log::Record;
    use std::time::Duration;

    fn log(clicks: &[(u64, f32, f32)]) -> Log {
        let mut records = vec![
            Record { at: Duration::ZERO, entry: Entry::Display { x: 0.0, y: 0.0 } },
            Record { at: Duration::ZERO, entry: Entry::DisplaySize { w: 1000.0, h: 1000.0 } },
        ];
        for &(t, x, y) in clicks {
            records.push(Record { at: Duration::from_millis(t), entry: Entry::Move { x, y } });
            records.push(Record { at: Duration::from_millis(t), entry: Entry::Down { button: 0, x, y } });
        }
        Log { listening: true, records }
    }

    #[test]
    fn clicks_close_together_make_one_zoom_on_them() {
        let l = log(&[(5_000, 200.0, 300.0), (6_000, 220.0, 310.0), (7_500, 210.0, 290.0)]);
        let z = auto(&l, 20_000);
        assert_eq!(z.len(), 1);
        assert!(z[0].start_ms < 5_000 && z[0].end_ms > 7_500, "in before the first, out after the last: {:?}", z[0]);
        let Focus::Point { x, y } = z[0].focus else { panic!("a fixed focus") };
        assert!((x - 0.21).abs() < 0.01 && (y - 0.30).abs() < 0.01, "{x} {y}");
    }

    #[test]
    fn separate_moments_make_separate_zooms_and_spread_ones_follow() {
        let l = log(&[(2_000, 100.0, 100.0), (15_000, 900.0, 900.0), (16_000, 100.0, 900.0)]);
        let z = auto(&l, 30_000);
        assert_eq!(z.len(), 2);
        assert!(matches!(z[0].focus, Focus::Point { .. }));
        assert_eq!(z[1].focus, Focus::Pointer, "clicks far apart: follow the pointer");
        // the take's end cuts a zoom short
        let near_end = auto(&log(&[(29_500, 500.0, 500.0)]), 30_000);
        assert_eq!(near_end[0].end_ms, 30_000);
    }

    #[test]
    fn the_camera_settles_in_a_zoom_and_comes_back_deterministically() {
        let zooms =
            [Zoom { start_ms: 1_000, end_ms: 4_000, scale: 2.0, focus: Focus::Point { x: 0.9, y: 0.1 }, auto: true }];
        let cam = Camera::build(&zooms, 8_000, |_| None);
        let mid = cam.at(3_500);
        assert!((mid.scale - 2.0).abs() < 0.02, "{mid:?}");
        // the view can't leave the screen: centred no closer than a quarter to the edge
        assert!((mid.x - 0.75).abs() < 0.01 && (mid.y - 0.25).abs() < 0.01, "{mid:?}");
        let after = cam.at(7_900);
        assert!((after.scale - 1.0).abs() < 0.01 && (after.x - 0.5).abs() < 0.01, "{after:?}");
        let again = Camera::build(&zooms, 8_000, |_| None);
        assert_eq!(again.at(2_345), cam.at(2_345), "the same edit gives the same camera");
    }
}
