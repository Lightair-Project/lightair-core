//! Snapshot diffing: the minimal set of patches that turns one project into another.
//!
//! The engine's reconciler applies patches to libobs. Because patches are computed from
//! snapshots rather than recorded per command, applying `diff(empty, p)` to an empty engine
//! produces the same state as any sequence of commands that ends at `p` (the reconciler is
//! idempotent by construction).

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::model::{Format, Id, Input, Layer, Project, Scene, Track};

#[derive(Debug, Clone, PartialEq)]
pub enum Patch {
    FormatAdded(Format),
    FormatChanged(Format),
    FormatRemoved(Id),
    InputAdded(Arc<Input>),
    InputChanged {
        before: Arc<Input>,
        after: Arc<Input>,
    },
    InputRemoved(Id),
    SceneAdded(Arc<Scene>),
    /// Layers changed in an existing scene (added, removed, reordered or re-laid-out).
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
    /// Buses, mixes or the master changed (the mixer is rebuilt from the project).
    AudioSetup,
}

/// Patches in an order that is safe to apply: formats and inputs before the scenes that use
/// them, and scenes removed before the inputs they reference.
pub fn diff(before: &Project, after: &Project) -> Vec<Patch> {
    let mut out = Vec::new();

    for f in &after.formats {
        match before.format(f.id) {
            None => out.push(Patch::FormatAdded(f.clone())),
            Some(old) if old != f => out.push(Patch::FormatChanged(f.clone())),
            _ => {}
        }
    }

    for (id, input) in &after.inputs {
        match before.inputs.get(id) {
            None => out.push(Patch::InputAdded(input.clone())),
            Some(old) if !Arc::ptr_eq(old, input) && old != input => {
                out.push(Patch::InputChanged { before: old.clone(), after: input.clone() })
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
    let order = |p: &Project| p.tracks.iter().map(|t| t.id).collect::<Vec<_>>();
    if order(before) != order(after) {
        out.push(Patch::TrackOrder(order(after)));
    }
    if before.audio != after.audio {
        out.push(Patch::AudioSetup);
    }

    for id in before.inputs.keys() {
        if !after.inputs.contains_key(id) {
            out.push(Patch::InputRemoved(*id));
        }
    }
    for f in &before.formats {
        if after.format(f.id).is_none() {
            out.push(Patch::FormatRemoved(f.id));
        }
    }
    out
}

/// Layer-level changes inside one scene, for the reconciler.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerPatch<'a> {
    Added {
        layer: &'a Layer,
        index: usize,
    },
    Removed {
        layer: Id,
    },
    Changed {
        layer: &'a Layer,
    },
    /// Final bottom-to-top order, sent once if any layer moved.
    Order(Vec<Id>),
}

pub fn diff_layers<'a>(before: &'a Scene, after: &'a Scene) -> Vec<LayerPatch<'a>> {
    let mut out = Vec::new();
    for l in &before.layers {
        if after.layer(l.id).is_none() {
            out.push(LayerPatch::Removed { layer: l.id });
        }
    }
    for (index, l) in after.layers.iter().enumerate() {
        match before.layer(l.id) {
            None => out.push(LayerPatch::Added { layer: l, index }),
            Some(old) if old != l => out.push(LayerPatch::Changed { layer: l }),
            _ => {}
        }
    }
    let kept_before: Vec<Id> = before.layers.iter().map(|l| l.id).filter(|id| after.layer(*id).is_some()).collect();
    let kept_after: Vec<Id> = after.layers.iter().map(|l| l.id).filter(|id| before.layer(*id).is_some()).collect();
    if kept_before != kept_after {
        out.push(LayerPatch::Order(after.layers.iter().map(|l| l.id).collect()));
    }
    out
}
