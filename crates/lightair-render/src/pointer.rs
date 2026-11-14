//! The pointer through a take, from its input log: smoothed on a spring stepped on a fixed clock
//! (the same path every render), its shape, its tilt from its speed, and the clicks to draw.

use lightair_store::input_log::{Entry, Log};

use crate::compose::PointerDraw;

const HZ: f64 = 240.0;
/// How long a click's effect lasts.
pub const CLICK_MS: u64 = 450;

pub struct Track {
    /// (x, y) as fractions of the screen, and the x velocity (fractions/s), at 240 Hz.
    samples: Vec<(f32, f32, f32)>,
    shapes: Vec<(u64, u8)>,
    clicks: Vec<(u64, f32, f32)>,
}

fn spring(x: f32, v: f32, target: f32, omega: f32, dt: f32) -> (f32, f32) {
    let f = 1.0 + 2.0 * dt * omega;
    let oo = omega * omega;
    let (hoo, hhoo) = (dt * oo, dt * dt * oo);
    let inv = 1.0 / (f + hhoo);
    ((f * x + dt * v + hhoo * target) * inv, (v + hoo * (target - x)) * inv)
}

impl Track {
    /// The pointer of a take `duration_ms` long; `smoothing` 0 exact .. 1 a slow glide.
    pub fn build(log: &Log, duration_ms: u64, smoothing: f32) -> Track {
        let (mut ox, mut oy, mut w, mut h) = (0.0, 0.0, 1.0, 1.0);
        for r in &log.records {
            match r.entry {
                Entry::Display { x, y } => (ox, oy) = (x, y),
                Entry::DisplaySize { w: dw, h: dh } => (w, h) = (dw.max(1.0), dh.max(1.0)),
                _ => {}
            }
        }
        let norm = |x: f32, y: f32| ((x - ox) / w, (y - oy) / h);
        let mut shapes = Vec::new();
        let mut clicks = Vec::new();
        for r in &log.records {
            let t = r.at.as_millis() as u64;
            match r.entry {
                Entry::Shape(s) => shapes.push((t, s)),
                Entry::Down { x, y, .. } => {
                    let p = norm(x, y);
                    clicks.push((t, p.0, p.1));
                }
                _ => {}
            }
        }
        let omega = 60.0 - 52.0 * smoothing.clamp(0.0, 1.0);
        let n = (duration_ms as f64 / 1000.0 * HZ).ceil() as usize + 1;
        let dt = (1.0 / HZ) as f32;
        let mut samples = Vec::with_capacity(n);
        let moves: Vec<(f64, f32, f32)> = log
            .records
            .iter()
            .filter_map(|r| match r.entry {
                Entry::Move { x, y } => Some((r.at.as_secs_f64(), x, y)),
                _ => None,
            })
            .collect();
        let mut next = 0;
        let mut state: Option<(f32, f32, f32, f32)> = None;
        for i in 0..n {
            let at = i as f64 / HZ;
            // the moves either side of this moment (one walk through the log)
            while next < moves.len() && moves[next].0 <= at {
                next += 1;
            }
            let Some(before) = next.checked_sub(1).map(|k| moves[k]) else {
                samples.push((f32::NAN, f32::NAN, 0.0));
                continue;
            };
            let raw = match moves.get(next) {
                Some(after) if after.0 > before.0 => {
                    let k = ((at - before.0) / (after.0 - before.0)) as f32;
                    (before.1 + (after.1 - before.1) * k, before.2 + (after.2 - before.2) * k)
                }
                _ => (before.1, before.2),
            };
            let (tx, ty) = norm(raw.0, raw.1);
            let (x, y, vx, vy) = state.unwrap_or((tx, ty, 0.0, 0.0));
            let (nx, nvx) = spring(x, vx, tx, omega, dt);
            let (ny, nvy) = spring(y, vy, ty, omega, dt);
            state = Some((nx, ny, nvx, nvy));
            samples.push((nx, ny, nvx));
        }
        Track { samples, shapes, clicks }
    }

    /// The pointer at source time `t_ms` (`None` before it's known). `screen_w_px`: the screen's
    /// width in the result, for the tilt (which follows on-screen speed).
    pub fn at(&self, t_ms: u64, screen_w_px: f32) -> Option<PointerDraw> {
        let i = ((t_ms as f64 / 1000.0 * HZ) as usize).min(self.samples.len().checked_sub(1)?);
        let (x, y, vx) = self.samples[i];
        if x.is_nan() {
            return None;
        }
        let shape = self.shapes.iter().rev().find(|(t, _)| *t <= t_ms).map_or(0, |s| s.1);
        let tilt = (vx * screen_w_px * 0.012).clamp(-14.0, 14.0);
        Some(PointerDraw { x, y, shape, opacity: 1.0, tilt })
    }

    /// Clicks to draw at `t_ms`: where, and how far along their effect (0..1).
    pub fn clicks_at(&self, t_ms: u64) -> Vec<(f32, f32, f32)> {
        self.clicks
            .iter()
            .filter(|(t, ..)| *t <= t_ms && t_ms < t + CLICK_MS)
            .map(|&(t, x, y)| (x, y, (t_ms - t) as f32 / CLICK_MS as f32))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightair_store::input_log::Record;
    use std::time::Duration;

    #[test]
    fn the_path_glides_to_where_the_pointer_went_and_clicks_fade() {
        let ms = Duration::from_millis;
        let log = Log {
            listening: true,
            records: vec![
                Record { at: ms(0), entry: Entry::DisplaySize { w: 1000.0, h: 500.0 } },
                Record { at: ms(0), entry: Entry::Move { x: 100.0, y: 100.0 } },
                Record { at: ms(100), entry: Entry::Move { x: 900.0, y: 400.0 } },
                Record { at: ms(1_000), entry: Entry::Down { button: 0, x: 900.0, y: 400.0 } },
                Record { at: ms(1_200), entry: Entry::Shape(1) },
            ],
        };
        let t = Track::build(&log, 3_000, 0.5);
        let early = t.at(60, 1000.0).unwrap();
        assert!(early.x > 0.1 && early.x < 0.9, "on its way: {early:?}");
        let late = t.at(2_000, 1000.0).unwrap();
        assert!((late.x - 0.9).abs() < 0.01 && (late.y - 0.8).abs() < 0.01, "{late:?}");
        assert_eq!(late.shape, 1);
        assert_eq!(t.clicks_at(1_100).len(), 1);
        assert!(t.clicks_at(2_000).is_empty());
        let again = Track::build(&log, 3_000, 0.5);
        assert_eq!(again.at(777, 1000.0), t.at(777, 1000.0), "the same every time");
    }
}
