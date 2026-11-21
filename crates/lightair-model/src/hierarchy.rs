//! The scene hierarchy: objects inside objects (docs/07-scenes-objects-canvas.md, "Hierarchy").
//!
//! A scene's objects are one list, bottom to top, in which every object may name a `parent`. The
//! list keeps each object's subtree together right after it (a parent, then its children and
//! theirs), and siblings run back to front, so drawing the list in order draws a parent behind
//! its children and later siblings in front.
//!
//! A child's layout is in its parent's space: its position is relative to the parent's pivot,
//! its rotation adds to the parent's, and the parent's scale scales it. An *empty* is a object
//! without a source: just a pivot (its x, y), rotation and scale, holding other objects.
//! [`Scene::flatten`] turns a hierarchy into what is drawn: the pictures alone, each with its
//! world layout per canvas.

use std::ops::Range;

use crate::model::{Id, Layout, Object, Scene};

/// A object's own space as seen from its parent's: where its pivot sits, how it is turned and
/// scaled. Children are placed in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub cx: f32,
    pub cy: f32,
    /// Degrees clockwise.
    pub rotation: f32,
    pub sx: f32,
    pub sy: f32,
}

impl Frame {
    pub const IDENTITY: Frame = Frame { cx: 0.0, cy: 0.0, rotation: 0.0, sx: 1.0, sy: 1.0 };

    /// A point in this frame's space, in its parent's.
    pub fn apply(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        let (x, y) = (x * self.sx, y * self.sy);
        (self.cx + x * cos - y * sin, self.cy + x * sin + y * cos)
    }

    /// A point in the parent's space, in this frame's.
    pub fn unapply(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (sin, cos) = self.rotation.to_radians().sin_cos();
        let (dx, dy) = (x - self.cx, y - self.cy);
        let (x, y) = (dx * cos + dy * sin, -dx * sin + dy * cos);
        (x / self.sx.max(1e-4), y / self.sy.max(1e-4))
    }

    /// This frame followed by `child` (a frame in this one's space): `child` as seen from here.
    pub fn then(&self, child: &Frame) -> Frame {
        let (cx, cy) = self.apply((child.cx, child.cy));
        Frame { cx, cy, rotation: self.rotation + child.rotation, sx: self.sx * child.sx, sy: self.sy * child.sy }
    }
}

impl Layout {
    /// The space this layout's children live in: pivot at the box centre (an empty's box has
    /// no size, so its pivot is its x, y), turned and scaled like the layout.
    pub fn frame(&self) -> Frame {
        Frame {
            cx: self.x + self.width / 2.0,
            cy: self.y + self.height / 2.0,
            rotation: self.rotation,
            sx: self.scale_x,
            sy: self.scale_y,
        }
    }

    /// This layout (in `parent`'s space) in the space `parent` lives in. With a parent scaled
    /// unevenly and a child turned, the result can't skew, so it keeps the child's shape.
    pub fn to_world(&self, parent: &Frame) -> Layout {
        let (cx, cy) = parent.apply((self.x + self.width / 2.0, self.y + self.height / 2.0));
        let (w, h) = (self.width * parent.sx, self.height * parent.sy);
        Layout {
            x: cx - w / 2.0,
            y: cy - h / 2.0,
            width: w,
            height: h,
            rotation: self.rotation + parent.rotation,
            scale_x: self.scale_x * parent.sx,
            scale_y: self.scale_y * parent.sy,
            ..*self
        }
    }

    /// The inverse of [`Layout::to_world`]: this layout (in the space `parent` lives in) in
    /// `parent`'s own space.
    pub fn to_local(&self, parent: &Frame) -> Layout {
        let (sx, sy) = (parent.sx.max(1e-4), parent.sy.max(1e-4));
        let (cx, cy) = parent.unapply((self.x + self.width / 2.0, self.y + self.height / 2.0));
        let (w, h) = (self.width / sx, self.height / sy);
        Layout {
            x: cx - w / 2.0,
            y: cy - h / 2.0,
            width: w,
            height: h,
            rotation: self.rotation - parent.rotation,
            scale_x: self.scale_x / sx,
            scale_y: self.scale_y / sy,
            ..*self
        }
    }
}

impl Object {
    /// An empty: no picture, a pivot holding other objects.
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
    }
}

impl Scene {
    /// The objects directly under `parent` (`None`: the top level), back to front.
    pub fn children(&self, parent: Option<Id>) -> impl Iterator<Item = &Object> {
        self.objects.iter().filter(move |l| l.parent == parent)
    }

    /// Where `id` and everything under it sit in the list (always one run).
    pub fn subtree(&self, id: Id) -> Option<Range<usize>> {
        let start = self.object_index(id)?;
        let end = self.objects[start + 1..]
            .iter()
            .position(|l| !self.is_under(l.id, id))
            .map_or(self.objects.len(), |n| start + 1 + n);
        Some(start..end)
    }

    /// Whether `id` is somewhere under `ancestor` (not itself).
    pub fn is_under(&self, id: Id, ancestor: Id) -> bool {
        let mut at = self.object(id).and_then(|l| l.parent);
        while let Some(p) = at {
            if p == ancestor {
                return true;
            }
            at = self.object(p).and_then(|l| l.parent);
        }
        false
    }

    /// `id`'s parents, nearest first.
    pub fn ancestors(&self, id: Id) -> Vec<Id> {
        let mut out = Vec::new();
        let mut at = self.object(id).and_then(|l| l.parent);
        while let Some(p) = at {
            if out.contains(&p) {
                break;
            }
            out.push(p);
            at = self.object(p).and_then(|l| l.parent);
        }
        out
    }

    pub fn depth(&self, id: Id) -> usize {
        self.ancestors(id).len()
    }

    /// The space `id`'s children live in, in canvas terms, for `canvas` (`None` if `id` or a
    /// parent has no layout there). `None` as `id`: the canvas itself.
    pub fn frame_of(&self, id: Option<Id>, canvas: Id) -> Option<Frame> {
        let Some(id) = id else { return Some(Frame::IDENTITY) };
        let mut chain = vec![id];
        chain.extend(self.ancestors(id));
        let mut frame = Frame::IDENTITY;
        for l in chain.iter().rev() {
            frame = frame.then(&self.object(*l)?.layouts.get(&canvas)?.frame());
        }
        Some(frame)
    }

    /// `id`'s layout in canvas terms for `canvas`: its own layout through its parents' frames.
    pub fn world(&self, id: Id, canvas: Id) -> Option<Layout> {
        let object = self.object(id)?;
        let local = object.layouts.get(&canvas)?;
        Some(local.to_world(&self.frame_of(object.parent, canvas)?))
    }

    /// `world` (a layout in canvas terms) in the space of `id`'s parent: what to store so `id`
    /// lands there.
    pub fn local(&self, id: Id, canvas: Id, world: &Layout) -> Option<Layout> {
        let parent = self.object(id)?.parent;
        Some(world.to_local(&self.frame_of(parent, canvas)?))
    }

    /// Whether `id` is shown in `canvas`: it and all its parents have a visible layout there.
    pub fn shown(&self, id: Id, canvas: Id) -> bool {
        std::iter::once(id)
            .chain(self.ancestors(id))
            .all(|l| self.object(l).and_then(|l| l.layouts.get(&canvas)).is_some_and(|lo| lo.visible))
    }

    /// Whether `id` or a parent is locked.
    pub fn locked(&self, id: Id) -> bool {
        std::iter::once(id).chain(self.ancestors(id)).any(|l| self.object(l).is_some_and(|l| l.locked))
    }

    /// What is drawn: the objects with pictures, in drawing order, each with its world layout
    /// per canvas. A parent's visibility, opacity and lock pass down; empties are left out.
    /// A scene without nesting comes back unchanged.
    pub fn flatten(&self) -> Scene {
        if self.objects.iter().all(|l| l.parent.is_none() && l.source.is_some()) {
            return self.clone();
        }
        let objects = self
            .objects
            .iter()
            .filter(|l| l.source.is_some())
            .map(|l| {
                let mut out = Object { parent: None, locked: self.locked(l.id), ..l.clone() };
                for (canvas, layout) in out.layouts.iter_mut() {
                    let Some(mut world) = self.world(l.id, *canvas) else { continue };
                    world.visible = self.shown(l.id, *canvas);
                    world.opacity = std::iter::once(l.id)
                        .chain(self.ancestors(l.id))
                        .filter_map(|a| self.object(a)?.layouts.get(canvas).map(|lo| lo.opacity))
                        .product();
                    *layout = world;
                }
                // A canvas a parent isn't in hides the object there.
                out.layouts.retain(|canvas, _| self.frame_of(l.parent, *canvas).is_some());
                out
            })
            .collect();
        Scene {
            id: self.id,
            name: self.name.clone(),
            objects,
            screen: self.screen,
            transition: self.transition,
            stinger: self.stinger.clone(),
        }
    }

    /// Whether the list keeps every subtree in one run after its parent (for tests and checks).
    pub fn is_well_formed(&self) -> bool {
        self.objects.iter().enumerate().all(|(i, l)| match l.parent {
            None => true,
            Some(p) => {
                let Some(pi) = self.object_index(p) else { return false };
                // The parent comes first and everything between belongs under it.
                pi < i && self.objects[pi + 1..i].iter().all(|m| self.is_under(m.id, p))
            }
        })
    }

    /// Takes `id` and its subtree out of the list (in order).
    pub(crate) fn take_subtree(&mut self, id: Id) -> Option<Vec<Object>> {
        let range = self.subtree(id)?;
        Some(self.objects.drain(range).collect())
    }

    /// Puts a subtree (its root first) under `parent` at `index` among that parent's children,
    /// back to front (`None`: in front of them all).
    pub(crate) fn put_subtree(&mut self, mut block: Vec<Object>, parent: Option<Id>, index: Option<usize>) {
        let Some(root) = block.first_mut() else { return };
        root.parent = parent;
        let siblings: Vec<Id> = self.children(parent).map(|l| l.id).collect();
        let at = match index.and_then(|i| siblings.get(i)) {
            // Before that sibling's subtree.
            Some(s) => self.object_index(*s).unwrap_or(self.objects.len()),
            // In front of the last sibling (after its subtree), or right after the parent.
            None => match (siblings.last(), parent) {
                (Some(last), _) => self.subtree(*last).map_or(self.objects.len(), |r| r.end),
                (None, Some(p)) => self.object_index(p).map_or(self.objects.len(), |i| i + 1),
                (None, None) => self.objects.len(),
            },
        };
        self.objects.splice(at..at, block);
    }
}
