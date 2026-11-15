//! Copy, paste and duplicate (docs/07-scenes-objects-canvas.md, "Editing"): objects travel with
//! everything under them and the sources they show, as plain data, so a copy can be pasted into
//! another scene or workspace.
//!
//! Pasting gives every object a new id. A content source (text, image, colour, media, timer…) is
//! copied too, so editing the copy leaves the original alone; a device (camera, display,
//! window, microphone) is shared when the workspace already has it, since opening it twice gains
//! nothing. Copies land where the originals were on the canvas.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::model::{Id, Source, Object, Workspace, Scene};

/// What the clipboard holds: whole subtrees (each root first, roots in their list order) and
/// the sources they show. Roots' layouts are in canvas terms; the rest stay in their parent's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    #[serde(alias = "layers")]
    pub objects: Vec<Object>,
    pub sources: Vec<Source>,
}

/// Marks Lightair's data on the system clipboard.
const TAG: &str = "lightair/objects@1\n";
/// The tag before objects were called objects (still read).
const OLD_TAG: &str = "lightair/layers@1\n";

/// Sources that capture something on this machine: shared between copies, not opened twice.
pub fn is_device(kind: &str) -> bool {
    crate::kinds::ALL.contains(&kind) || crate::kinds::from_libobs(kind, &serde_json::Value::Null).is_some()
}

impl Clip {
    /// `ids` (and what's under them) from `scene`. A object inside another copied object comes
    /// with it rather than on its own.
    pub fn copy(workspace: &Workspace, scene: Id, ids: &[Id]) -> Option<Clip> {
        let sc = workspace.scene(scene)?;
        let roots: Vec<Id> = sc
            .objects
            .iter()
            .map(|l| l.id)
            .filter(|id| ids.contains(id) && !ids.iter().any(|a| sc.is_under(*id, *a)))
            .collect();
        if roots.is_empty() {
            return None;
        }
        let mut objects = Vec::new();
        for root in roots {
            let range = sc.subtree(root)?;
            let mut block: Vec<Object> = sc.objects[range].to_vec();
            // The root in canvas terms, so it can land in any parent.
            let world: BTreeMap<Id, _> =
                block[0].layouts.keys().filter_map(|f| Some((*f, sc.world(root, *f)?))).collect();
            block[0].layouts = world;
            block[0].parent = None;
            objects.extend(block);
        }
        let mut sources: Vec<Source> = Vec::new();
        for l in &objects {
            if let Some(source) = l.source.and_then(|i| workspace.sources.get(&i))
                && !sources.iter().any(|i| i.id == source.id)
            {
                sources.push((**source).clone());
            }
        }
        Some(Clip { objects, sources })
    }

    /// The commands that paste this into `scene` under `parent` (`None`: the top level), at
    /// `index` among its children (`None`: in front), and the new roots' ids.
    pub fn paste(
        &self,
        workspace: &Workspace,
        scene: Id,
        parent: Option<Id>,
        index: Option<usize>,
    ) -> (Vec<Command>, Vec<Id>) {
        let Some(sc) = workspace.scene(scene) else { return (vec![], vec![]) };
        let mut commands = Vec::new();
        // Sources: devices the workspace has are shared; everything else is a fresh copy.
        let mut source_ids: HashMap<Id, Id> = HashMap::new();
        for source in &self.sources {
            // copied by a Lightair from before portable kinds
            let mut source = source.clone();
            if let Some((kind, settings)) = crate::kinds::from_libobs(&source.kind, &source.settings) {
                source.kind = kind.into();
                source.settings = settings;
            }
            let source = &source;
            let shared = is_device(&source.kind) && workspace.sources.get(&source.id).is_some_and(|i| i.kind == source.kind);
            if shared {
                source_ids.insert(source.id, source.id);
            } else {
                let copy = Source { id: Id::new(), ..source.clone() };
                source_ids.insert(source.id, copy.id);
                commands.push(Command::AddSource { source: copy });
            }
        }
        let mut object_ids: HashMap<Id, Id> = HashMap::new();
        let mut roots = Vec::new();
        let primary = workspace.primary_canvas().id;
        let mut next = index;
        for l in &self.objects {
            let id = Id::new();
            object_ids.insert(l.id, id);
            let is_root = l.parent.is_none_or(|p| !object_ids.contains_key(&p));
            let mut object = Object {
                id,
                source: l.source.map(|i| source_ids.get(&i).copied().unwrap_or(i)),
                parent: if is_root { parent } else { l.parent.and_then(|p| object_ids.get(&p).copied()) },
                ..l.clone()
            };
            // Canvases this workspace doesn't have: keep the first layout, in the primary canvas.
            let first = object.layouts.values().next().copied();
            object.layouts.retain(|f, _| workspace.canvas(*f).is_some());
            if object.layouts.is_empty()
                && let Some(lo) = first
            {
                object.layouts.insert(primary, lo);
            }
            if is_root {
                for (f, lo) in object.layouts.iter_mut() {
                    if let Some(frame) = sc.frame_of(parent, *f) {
                        *lo = lo.to_local(&frame);
                    }
                }
                roots.push(id);
                commands.push(Command::AddObject { scene, object, index: next });
                next = next.map(|i| i + 1);
            } else {
                // Back to front, each in front of the last: the order comes out the same.
                commands.push(Command::AddObject { scene, object, index: None });
            }
        }
        (commands, roots)
    }

    /// The commands that place this clip as an instance of prefab `name` (its root linked to
    /// it), and the new root's id.
    pub fn instantiate(
        &self,
        workspace: &Workspace,
        scene: Id,
        name: &str,
        parent: Option<Id>,
        index: Option<usize>,
    ) -> (Vec<Command>, Option<Id>) {
        let mut clip = self.clone();
        if let Some(root) = clip.objects.first_mut() {
            root.prefab = Some(name.to_string());
        }
        let (commands, roots) = clip.paste(workspace, scene, parent, index);
        (commands, roots.first().copied())
    }

    /// The commands that rebuild instance `root` from this clip (a prefab's contents): it keeps
    /// its place in the tree, where it sits on the canvas and its name; everything under it is
    /// the prefab's. Returns the rebuilt root's id.
    pub fn replace_instance(&self, workspace: &Workspace, scene: Id, root: Id) -> (Vec<Command>, Option<Id>) {
        let Some(sc) = workspace.scene(scene) else { return (vec![], None) };
        let Some(old) = sc.object(root) else { return (vec![], None) };
        let siblings: Vec<Id> = sc.children(old.parent).map(|l| l.id).collect();
        let at = siblings.iter().position(|s| *s == root);
        let mut clip = self.clone();
        let Some(first) = clip.objects.first_mut() else { return (vec![], None) };
        first.layouts = old.layouts.keys().filter_map(|f| Some((*f, sc.world(root, *f)?))).collect();
        first.name = old.name.clone();
        first.prefab = old.prefab.clone();
        let (paste, roots) = clip.paste(workspace, scene, old.parent, at);
        let mut commands = vec![Command::RemoveObject { scene, object: root }];
        commands.extend(paste);
        (commands, roots.first().copied())
    }

    /// The clipboard text for this clip.
    pub fn to_text(&self) -> String {
        format!("{TAG}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// A clip from clipboard text, if it is one.
    pub fn from_text(text: &str) -> Option<Clip> {
        serde_json::from_str(text.strip_prefix(TAG).or_else(|| text.strip_prefix(OLD_TAG))?).ok()
    }
}

/// A whole scene saved as an asset (a `.scene` file): its name, and every object with its
/// sources. Added back, it's a new scene with new objects (devices shared, as when pasting).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneAsset {
    pub name: String,
    pub clip: Clip,
}

impl SceneAsset {
    /// `scene` as an asset.
    pub fn of(workspace: &Workspace, scene: Id) -> Option<SceneAsset> {
        let sc = workspace.scene(scene)?;
        let roots: Vec<Id> = sc.objects.iter().filter(|l| l.parent.is_none()).map(|l| l.id).collect();
        let clip = Clip::copy(workspace, scene, &roots).unwrap_or(Clip { objects: vec![], sources: vec![] });
        Some(SceneAsset { name: sc.name.clone(), clip })
    }

    /// The commands that add it as a new scene at `index` (`None`: last), and its id.
    pub fn add(&self, workspace: &Workspace, index: Option<usize>) -> (Vec<Command>, Id) {
        let scene = Scene::new(self.name.clone());
        let id = scene.id;
        // the objects are pasted into the new scene, as if it were already there
        let mut with = workspace.clone();
        with.scenes.push(std::sync::Arc::new(scene.clone()));
        let (objects, _) = self.clip.paste(&with, id, None, None);
        let mut commands = vec![Command::AddScene { scene, index }];
        commands.extend(objects);
        (commands, id)
    }
}
