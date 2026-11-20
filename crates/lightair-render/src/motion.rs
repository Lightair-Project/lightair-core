//! A clip's animations, as CapCut has them: one coming in (its first moments), one going out
//! (its last), or a combo across the whole clip. Each moves, scales, turns, fades or blurs the
//! picture on top of its transform; none changes the transform itself.

use serde::{Deserialize, Serialize};

use crate::sequence::Clip;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MotionKind {
    // in (and out, played backwards)
    Fade,
    ZoomIn,
    ZoomOut,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    Spin,
    Pop,
    Blur,
    Drop,
    Swing,
    // combos (across the whole clip)
    Sway,
    Pulse,
    Float,
    Shake,
    Turn,
    Wobble,
    Bounce,
    Through,
    Glide,
    KenBurns,
}

/// The animations that come in (and go out).
pub const IN: [(&str, MotionKind); 12] = [
    ("Fade", MotionKind::Fade),
    ("Zoom in", MotionKind::ZoomIn),
    ("Zoom out", MotionKind::ZoomOut),
    ("Slide left", MotionKind::SlideLeft),
    ("Slide right", MotionKind::SlideRight),
    ("Slide up", MotionKind::SlideUp),
    ("Slide down", MotionKind::SlideDown),
    ("Spin", MotionKind::Spin),
    ("Pop", MotionKind::Pop),
    ("Blur", MotionKind::Blur),
    ("Drop", MotionKind::Drop),
    ("Swing", MotionKind::Swing),
];

/// The combos.
pub const COMBO: [(&str, MotionKind); 10] = [
    ("Sway", MotionKind::Sway),
    ("Pulse", MotionKind::Pulse),
    ("Float", MotionKind::Float),
    ("Shake", MotionKind::Shake),
    ("Turn", MotionKind::Turn),
    ("Wobble", MotionKind::Wobble),
    ("Bounce", MotionKind::Bounce),
    ("Zoom through", MotionKind::Through),
    ("Glide", MotionKind::Glide),
    ("Ken Burns", MotionKind::KenBurns),
];

/// An animation: what, and how long (a combo: one cycle).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Motion {
    pub kind: MotionKind,
    pub ms: u64,
}

/// A clip's animations.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Motions {
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    #[serde(default, rename = "out", skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combo: Option<Motion>,
}

impl Motions {
    pub fn is_none(&self) -> bool {
        self.enter.is_none() && self.exit.is_none() && self.combo.is_none()
    }
}

/// What an animation does at a moment, on top of the clip's transform: a move (fractions of
/// the frame), a scale, a turn (degrees), an opacity and a blur.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delta {
    pub dx: f32,
    pub dy: f32,
    pub scale: f32,
    pub rotation: f32,
    pub opacity: f32,
    pub blur: f32,
}

impl Default for Delta {
    fn default() -> Self {
        Delta { dx: 0.0, dy: 0.0, scale: 1.0, rotation: 0.0, opacity: 1.0, blur: 0.0 }
    }
}

impl Delta {
    fn then(self, o: Delta) -> Delta {
        Delta {
            dx: self.dx + o.dx,
            dy: self.dy + o.dy,
            scale: self.scale * o.scale,
            rotation: self.rotation + o.rotation,
            opacity: self.opacity * o.opacity,
            blur: self.blur + o.blur,
        }
    }
}

fn out_cubic(p: f32) -> f32 {
    1.0 - (1.0 - p).powi(3)
}

fn out_back(p: f32) -> f32 {
    let (c1, c3) = (1.70158, 2.70158);
    1.0 + c3 * (p - 1.0).powi(3) + c1 * (p - 1.0).powi(2)
}

fn out_bounce(p: f32) -> f32 {
    let (n, d) = (7.5625, 2.75);
    if p < 1.0 / d {
        n * p * p
    } else if p < 2.0 / d {
        let p = p - 1.5 / d;
        n * p * p + 0.75
    } else if p < 2.5 / d {
        let p = p - 2.25 / d;
        n * p * p + 0.9375
    } else {
        let p = p - 2.625 / d;
        n * p * p + 0.984375
    }
}

/// An animation coming in, `p` of the way (0 nothing shown yet, 1 in place).
pub fn coming(kind: MotionKind, p: f32) -> Delta {
    let p = p.clamp(0.0, 1.0);
    let e = out_cubic(p);
    let d = Delta::default();
    match kind {
        MotionKind::Fade => Delta { opacity: e, ..d },
        MotionKind::ZoomIn => Delta { scale: 0.4 + 0.6 * e, opacity: e.min(1.0), ..d },
        MotionKind::ZoomOut => Delta { scale: 1.8 - 0.8 * e, opacity: e, ..d },
        MotionKind::SlideLeft => Delta { dx: 1.0 - e, ..d },
        MotionKind::SlideRight => Delta { dx: -(1.0 - e), ..d },
        MotionKind::SlideUp => Delta { dy: 1.0 - e, ..d },
        MotionKind::SlideDown => Delta { dy: -(1.0 - e), ..d },
        MotionKind::Spin => Delta { rotation: -(1.0 - e) * 360.0, scale: 0.2 + 0.8 * e, opacity: (p * 2.0).min(1.0), ..d },
        MotionKind::Pop => Delta { scale: out_back(p).max(0.0), opacity: (p * 3.0).min(1.0), ..d },
        MotionKind::Blur => Delta { blur: (1.0 - e) * 0.8, opacity: (p * 1.5).min(1.0), ..d },
        MotionKind::Drop => Delta { dy: -(1.0 - out_bounce(p)) * 0.8, opacity: (p * 4.0).min(1.0), ..d },
        MotionKind::Swing => Delta { rotation: (1.0 - e) * 35.0 * (p * std::f32::consts::PI * 3.0).cos(), opacity: (p * 3.0).min(1.0), ..d },
        _ => d,
    }
}

/// A combo at `u` seconds into the clip, `p` of the way through it; `cycle` its speed (s).
pub fn combo(kind: MotionKind, u: f32, p: f32, cycle: f32) -> Delta {
    let tau = std::f32::consts::TAU;
    let w = tau * u / cycle.max(0.1);
    let d = Delta::default();
    match kind {
        MotionKind::Sway => Delta { rotation: 6.0 * w.sin(), ..d },
        MotionKind::Pulse => Delta { scale: 1.0 + 0.06 * w.sin(), ..d },
        MotionKind::Float => Delta { dy: 0.03 * w.sin(), ..d },
        MotionKind::Shake => Delta { dx: 0.012 * (u * 37.0).sin(), dy: 0.012 * (u * 29.0 + 1.0).sin(), rotation: 1.5 * (u * 23.0).sin(), ..d },
        MotionKind::Turn => Delta { rotation: 360.0 * u / cycle.max(0.1), ..d },
        MotionKind::Wobble => Delta { rotation: 4.0 * (w * 1.3).sin(), scale: 1.0 + 0.04 * (w * 0.9).sin(), ..d },
        MotionKind::Bounce => Delta { dy: -0.06 * (w / 2.0).sin().abs(), ..d },
        // in, held, out: the first and last fifth
        MotionKind::Through => coming(MotionKind::ZoomIn, p / 0.2).then(going(MotionKind::ZoomOut, (p - 0.8) / 0.2)),
        MotionKind::Glide => coming(MotionKind::SlideLeft, p / 0.2).then(going(MotionKind::SlideLeft, (p - 0.8) / 0.2)),
        MotionKind::KenBurns => Delta { scale: 1.0 + 0.18 * p, dx: -0.03 * p, ..d },
        _ => d,
    }
}

/// An animation going out, `q` of the way (0 still in place, 1 gone): the one coming in,
/// backwards, leaving the way it came in heading.
pub fn going(kind: MotionKind, q: f32) -> Delta {
    let q = q.clamp(0.0, 1.0);
    if q <= 0.0 {
        return Delta::default();
    }
    let c = coming(kind, 1.0 - q);
    Delta { dx: -c.dx, dy: -c.dy, rotation: -c.rotation, ..c }
}

impl Clip {
    /// What its animations do at timeline time `t`.
    pub fn motion_at(&self, t: u64) -> Delta {
        let m = &self.motion;
        if m.is_none() {
            return Delta::default();
        }
        let len = self.len_ms().max(1);
        let rel = t.saturating_sub(self.start_ms).min(len);
        let mut d = Delta::default();
        if let Some(c) = m.combo {
            d = d.then(combo(c.kind, rel as f32 / 1000.0, rel as f32 / len as f32, c.ms as f32 / 1000.0));
        }
        if let Some(a) = m.enter
            && rel < a.ms
        {
            d = d.then(coming(a.kind, rel as f32 / a.ms.max(1) as f32));
        }
        if let Some(a) = m.exit {
            let from = len.saturating_sub(a.ms);
            if rel > from {
                d = d.then(going(a.kind, (rel - from) as f32 / a.ms.max(1) as f32));
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightair_model::Id;

    #[test]
    fn animations_come_in_and_go_out() {
        let mut c = Clip::new(Id::nil(), 1_000, 4_000);
        assert_eq!(c.motion_at(1_000), Delta::default());
        c.motion.enter = Some(Motion { kind: MotionKind::Fade, ms: 500 });
        c.motion.exit = Some(Motion { kind: MotionKind::SlideLeft, ms: 1_000 });
        assert_eq!(c.motion_at(1_000).opacity, 0.0, "not shown yet");
        assert_eq!(c.motion_at(2_000), Delta::default(), "in place between");
        let leaving = c.motion_at(4_500);
        assert!(leaving.dx < 0.0 && leaving.dx > -1.0, "heading left: {leaving:?}");
        assert!((c.motion_at(5_000).dx + 1.0).abs() < 1e-5, "gone at its end");
        // the clip's transform is its own
        assert_eq!(c.transform_at(1_000), crate::sequence::Transform::default());
    }

    #[test]
    fn every_animation_ends_in_place() {
        for (_, k) in IN {
            let d = coming(k, 1.0);
            assert!(d.dx.abs() < 1e-4 && d.dy.abs() < 1e-4 && (d.scale - 1.0).abs() < 1e-4 && d.rotation.abs() < 1e-3 && (d.opacity - 1.0).abs() < 1e-4 && d.blur.abs() < 1e-4, "{k:?}: {d:?}");
        }
    }
}
