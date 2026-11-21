//! Snapshot diffing: the minimal set of patches that turns one workspace into another.
//!
//! The engine's reconciler applies patches to libobs. Because patches are computed from
//! snapshots rather than recorded per command, applying `diff(empty, p)` to an empty engine
//! produces the same state as any sequence of commands that ends at `p` (the reconciler is
//! idempotent by construction).

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::model::{Canvas, Id, Object, Scene, Source, Track, Workspace};

#[derive(Debug, Clone, PartialEq)]
pub enum Patch {
    CanvasAdded(Canvas),
    CanvasChanged(Canvas),
    CanvasRemoved(Id),
    SourceAdded(Arc<Source>),
    SourceChanged {
        before: Arc<Source>,
        after: Arc<Source>,
    },
    SourceRemoved(Id),
    SceneAdded(Arc<Scene>),
    /// Objects changed in an existing scene (added, removed, reordered or re-laid-out).
    SceneChanged {
        before: Arc<Scene>,
        after: Arc<Scene>,
    },
    SceneRemoved(Id),
    ProgramChanged(Option<Id>),
    TrackAdded(Arc<Track>),
    TrackChanged {
        before: Arc<Track>,
        after: Arc<Track>,
    },
    TrackRemoved(Id),
    /// Final mixer order, sent when tracks were added, removed or moved.
    TrackOrder(Vec<Id>),
    /// Buses, mixes or the master changed (the mixer is rebuilt from the workspace).
    AudioSetup,
}

/// Patches in an order that is safe to apply: canvases and sources before the scenes that use
/// them, and scenes removed before the sources they reference.
pub fn diff(before: &Workspace, after: &Workspace) -> Vec<Patch> {
    let mut out = Vec::new();

    for f in &after.canvases {
        match before.canvas(f.id) {
            None => out.push(Patch::CanvasAdded(f.clone())),
            Some(old) if old != f => out.push(Patch::CanvasChanged(f.clone())),
            _ => {}
        }
    }

    for (id, source) in &after.sources {
        match before.sources.get(id) {
            None => out.push(Patch::SourceAdded(source.clone())),
            Some(old) if !Arc::ptr_eq(old, source) && old != source => {
                out.push(Patch::SourceChanged { before: old.clone(), after: source.clone() })
            }
            _ => {}
        }
    }

    let after_ids: BTreeSet<Id> = after.scenes.iter().map(|s| s.id).collect();
    for s in &before.scenes {
        if !after_ids.contains(&s.id) {
            out.push(Patch::SceneRemoved(s.id));
        }
    }
    for s in &after.scenes {
        match before.scene(s.id) {
            None => out.push(Patch::SceneAdded(s.clone())),
            Some(old) if !Arc::ptr_eq(old, s) && old != s => {
                out.push(Patch::SceneChanged { before: old.clone(), after: s.clone() })
            }
            _ => {}
        }
    }

    if before.program != after.program {
        out.push(Patch::ProgramChanged(after.program));
    }

    for t in &before.tracks {
        if after.track(t.id).is_none() {
            out.push(Patch::TrackRemoved(t.id));
        }
    }
    for t in &after.tracks {
        match before.track(t.id) {
            None => out.push(Patch::TrackAdded(t.clone())),
            Some(old) if !Arc::ptr_eq(old, t) && old != t => {
                out.push(Patch::TrackChanged { before: old.clone(), after: t.clone() })
            }
            _ => {}
        }
    }
    let order = |p: &Workspace| p.tracks.iter().map(|t| t.id).collect::<Vec<_>>();
    if order(before) != order(after) {
        out.push(Patch::TrackOrder(order(after)));
    }
    if before.audio != after.audio {
        out.push(Patch::AudioSetup);
    }

    for id in before.sources.keys() {
        if !after.sources.contains_key(id) {
            out.push(Patch::SourceRemoved(*id));
        }
    }
    for f in &before.canvases {
        if after.canvas(f.id).is_none() {
            out.push(Patch::CanvasRemoved(f.id));
        }
    }
    out
}

/// Object-level changes inside one scene, for the reconciler.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectPatch<'a> {
    Added {
        object: &'a Object,
        index: usize,
    },
    Removed {
        object: Id,
    },
    Changed {
        object: &'a Object,
    },
    /// Final bottom-to-top order, sent once if any object moved.
    Order(Vec<Id>),
}

pub fn diff_objects<'a>(before: &'a Scene, after: &'a Scene) -> Vec<ObjectPatch<'a>> {
    let mut out = Vec::new();
    for l in &before.objects {
        if after.object(l.id).is_none() {
            out.push(ObjectPatch::Removed { object: l.id });
        }
    }
    for (index, l) in after.objects.iter().enumerate() {
        match before.object(l.id) {
            None => out.push(ObjectPatch::Added { object: l, index }),
            Some(old) if old != l => out.push(ObjectPatch::Changed { object: l }),
            _ => {}
        }
    }
    let kept_before: Vec<Id> = before.objects.iter().map(|l| l.id).filter(|id| after.object(*id).is_some()).collect();
    let kept_after: Vec<Id> = after.objects.iter().map(|l| l.id).filter(|id| before.object(*id).is_some()).collect();
    if kept_before != kept_after {
        out.push(ObjectPatch::Order(after.objects.iter().map(|l| l.id).collect()));
    }
    out
}
