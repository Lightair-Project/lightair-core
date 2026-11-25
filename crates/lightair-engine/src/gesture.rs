//! Canvas editing: hit testing, and a drag, pinch or turn applied to a layer's layout
//! with snapping to the canvas's edges and centre and to other layers, and to 15° steps.
//! Everything the interface hands in or gets back is in the preview view's points.

use lightair_model::{Id, Layout, Object, Scene};

/// The preview view (points) showing a canvas (pixels) edge to edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub view: (f32, f32),
    pub canvas: (f32, f32),
}

impl View {
    fn kx(&self) -> f32 {
        self.canvas.0 / self.view.0.max(1.0)
    }
    fn ky(&self) -> f32 {
        self.canvas.1 / self.view.1.max(1.0)
    }
    pub fn to_canvas(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.kx(), y * self.ky())
    }
    pub fn frame(&self, layer: Id, l: &Layout) -> LayerFrame {
        LayerFrame {
            layer,
            cx: (l.x + l.width / 2.0) / self.kx(),
            cy: (l.y + l.height / 2.0) / self.ky(),
            width: l.width.abs() / self.kx(),
            height: l.height.abs() / self.ky(),
            rotation: l.rotation,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerFrame {
    pub layer: Id,
    pub cx: f32,
    pub cy: f32,
    pub width: f32,
    pub height: f32,
    pub rotation: f32,
}

/// Cumulative since the gesture began.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delta {
    pub dx: f32,
    pub dy: f32,
    pub scale: f32,
    pub rotation: f32,
    pub snap_threshold: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    pub vertical: bool,
    pub position: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Feedback {
    pub frame: LayerFrame,
    pub guides: Vec<Guide>,
    pub snapped_rotation: bool,
    pub label: String,
}

/// Whether canvas point `p` is inside a layout's box, turned with it.
fn contains(l: &Layout, p: (f32, f32)) -> bool {
    let (cx, cy) = (l.x + l.width / 2.0, l.y + l.height / 2.0);
    let a = -l.rotation.to_radians();
    let (dx, dy) = (p.0 - cx, p.1 - cy);
    let (x, y) = (dx * a.cos() - dy * a.sin(), dx * a.sin() + dy * a.cos());
    x.abs() <= l.width.abs() / 2.0 && y.abs() <= l.height.abs() / 2.0
}

/// The topmost visible, unlocked object at view point (x, y) in `canvas`.
pub fn hit(scene: &Scene, canvas: Id, view: &View, x: f32, y: f32) -> Option<Id> {
    let p = view.to_canvas(x, y);
    scene
        .objects
        .iter()
        .rev()
        .find(|o| {
            !o.locked
                && o.layouts
                    .get(&canvas)
                    .is_some_and(|l| l.visible && contains(l, p))
        })
        .map(|o| o.id)
}

/// The layout after `d`, snapped; the guides shown and the angle snapped to (if any).
pub fn apply(
    start: &Layout,
    others: &[Layout],
    view: &View,
    d: &Delta,
) -> (Layout, Vec<Guide>, Option<f32>) {
    let mut l = *start;
    let scale = if d.scale.is_finite() && d.scale > 0.01 {
        d.scale
    } else {
        1.0
    };
    let (cx0, cy0) = (start.x + start.width / 2.0, start.y + start.height / 2.0);
    l.width = start.width * scale;
    l.height = start.height * scale;
    let (mut cx, mut cy) = (cx0 + d.dx * view.kx(), cy0 + d.dy * view.ky());
    let mut guides = Vec::new();
    // edges and centres: the box's (unturned) against the canvas's and the other layers'
    let snap =
        |c: f32, half: f32, size: f32, others: &mut dyn Iterator<Item = (f32, f32)>, t: f32| {
            let mut targets = vec![0.0, size / 2.0, size];
            for (o, oh) in others {
                targets.extend([o - oh, o, o + oh]);
            }
            let mut best: Option<(f32, f32)> = None; // (shift, target)
            for edge in [c - half, c, c + half] {
                for &tg in &targets {
                    let shift = tg - edge;
                    if shift.abs() <= t && best.is_none_or(|(s, _)| shift.abs() < s.abs()) {
                        best = Some((shift, tg));
                    }
                }
            }
            best
        };
    let t = d.snap_threshold.max(0.0);
    if d.dx != 0.0 || d.dy != 0.0 {
        if let Some((shift, tg)) = snap(
            cx,
            l.width.abs() / 2.0,
            view.canvas.0,
            &mut others
                .iter()
                .map(|o| (o.x + o.width / 2.0, o.width.abs() / 2.0)),
            t * view.kx(),
        ) {
            cx += shift;
            guides.push(Guide {
                vertical: true,
                position: tg / view.kx(),
            });
        }
        if let Some((shift, tg)) = snap(
            cy,
            l.height.abs() / 2.0,
            view.canvas.1,
            &mut others
                .iter()
                .map(|o| (o.y + o.height / 2.0, o.height.abs() / 2.0)),
            t * view.ky(),
        ) {
            cy += shift;
            guides.push(Guide {
                vertical: false,
                position: tg / view.ky(),
            });
        }
    }
    l.x = cx - l.width / 2.0;
    l.y = cy - l.height / 2.0;
    let mut snapped = None;
    if d.rotation != 0.0 {
        let r = start.rotation + d.rotation;
        let step = (r / 15.0).round() * 15.0;
        l.rotation = if (r - step).abs() <= 3.0 {
            snapped = Some(step);
            step
        } else {
            r
        };
        l.rotation = l.rotation.rem_euclid(360.0);
    }
    (l, guides, snapped)
}

/// What the interface shows while dragging.
pub fn label(l: &Layout, turning: bool) -> String {
    if turning {
        format!("{}°", l.rotation.round() as i32)
    } else {
        format!("X {} · Y {}", l.x.round() as i32, l.y.round() as i32)
    }
}

/// The other visible objects' layouts in `canvas` (what a layer snaps to).
pub fn others(scene: &Scene, canvas: Id, except: Id) -> Vec<Layout> {
    scene
        .objects
        .iter()
        .filter(|o: &&Object| o.id != except)
        .filter_map(|o| o.layouts.get(&canvas).filter(|l| l.visible).copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: View = View {
        view: (360.0, 640.0),
        canvas: (1080.0, 1920.0),
    };

    fn delta(dx: f32, dy: f32) -> Delta {
        Delta {
            dx,
            dy,
            scale: 1.0,
            rotation: 0.0,
            snap_threshold: 8.0,
        }
    }

    #[test]
    fn a_drag_snaps_to_the_centre_and_says_so() {
        let start = Layout::boxed(100.0, 100.0, 200.0, 100.0);
        // centre at 200 canvas px; drag 118 points = 354 px → 554; canvas centre 540 is 14 px off
        // (under 8 points = 24 px): snapped
        let (l, guides, _) = apply(&start, &[], &V, &delta(118.0, 0.0));
        assert_eq!(l.x + l.width / 2.0, 540.0);
        assert_eq!(
            guides,
            vec![Guide {
                vertical: true,
                position: 180.0
            }]
        );
        // far from anything: moves freely
        let (l, guides, _) = apply(&start, &[], &V, &delta(50.0, 10.0));
        assert_eq!((l.x, l.y), (250.0, 130.0));
        assert!(guides.is_empty());
    }

    #[test]
    fn pinches_keep_the_centre_and_turns_snap_to_15_degrees() {
        let start = Layout::boxed(0.0, 0.0, 100.0, 100.0);
        let d = Delta {
            dx: 0.0,
            dy: 0.0,
            scale: 2.0,
            rotation: 43.0,
            snap_threshold: 0.0,
        };
        let (l, _, snapped) = apply(&start, &[], &V, &d);
        assert_eq!((l.x, l.y, l.width), (-50.0, -50.0, 200.0));
        assert_eq!((l.rotation, snapped), (45.0, Some(45.0)));
        let d = Delta {
            rotation: 37.0,
            ..d
        };
        assert_eq!(apply(&start, &[], &V, &d).2, None);
    }

    #[test]
    fn hits_the_topmost_and_respects_turns() {
        let canvas = Id::new();
        let mut scene = Scene::new("S");
        let a = Object::new(
            "a",
            Id::new(),
            [(canvas, Layout::boxed(0.0, 0.0, 1080.0, 1920.0))],
        );
        let mut small = Layout::boxed(400.0, 800.0, 300.0, 30.0);
        small.rotation = 90.0;
        let b = Object::new("b", Id::new(), [(canvas, small)]);
        scene.objects = vec![a.clone(), b.clone()];
        // the turned bar stands upright around its centre (550, 815)
        assert_eq!(
            hit(&scene, canvas, &V, 550.0 / 3.0, 700.0 / 3.0),
            Some(b.id)
        );
        assert_eq!(
            hit(&scene, canvas, &V, 450.0 / 3.0, 815.0 / 3.0),
            Some(a.id)
        );
    }
}
