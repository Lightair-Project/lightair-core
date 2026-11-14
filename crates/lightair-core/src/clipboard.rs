//! Copy, paste and duplicate (docs/07-scenes-layers-canvas.md, "Editing"): layers travel with
//! everything under them and the inputs they show, as plain data, so a copy can be pasted into
//! another scene or project.
//!
//! Pasting gives every layer a new id. A content input (text, image, colour, media, timer…) is
//! copied too, so editing the copy leaves the original alone; a device (camera, display,
//! window, microphone) is shared when the project already has it, since opening it twice gains
//! nothing. Copies land where the originals were on the canvas.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::model::{Id, Input, Layer, Project, Scene};

/// What the clipboard holds: whole subtrees (each root first, roots in their list order) and
/// the inputs they show. Roots' layouts are in canvas terms; the rest stay in their parent's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub layers: Vec<Layer>,
    pub inputs: Vec<Input>,
}

/// Marks Lightair's data on the system clipboard.
const TAG: &str = "lightair/layers@1\n";

/// Inputs that capture something on this machine: shared between copies, not opened twice.
pub fn is_device(kind: &str) -> bool {
    matches!(
        kind,
        "screen_capture"
            | "display_capture"
            | "window_capture"
            | "macos-avcapture"
            | "macos-avcapture-fast"
            | "av_capture_input_v2"
            | "av_capture_input"
            | "coreaudio_input_capture"
            | "coreaudio_output_capture"
            | "sck_audio_capture"
    )
}

impl Clip {
    /// `ids` (and what's under them) from `scene`. A layer inside another copied layer comes
    /// with it rather than on its own.
    pub fn copy(project: &Project, scene: Id, ids: &[Id]) -> Option<Clip> {
        let sc = project.scene(scene)?;
        let roots: Vec<Id> = sc
            .layers
            .iter()
            .map(|l| l.id)
            .filter(|id| ids.contains(id) && !ids.iter().any(|a| sc.is_under(*id, *a)))
            .collect();
        if roots.is_empty() {
            return None;
        }
        let mut layers = Vec::new();
        for root in roots {
            let range = sc.subtree(root)?;
            let mut block: Vec<Layer> = sc.layers[range].to_vec();
            // The root in canvas terms, so it can land in any parent.
            let world: BTreeMap<Id, _> =
                block[0].layouts.keys().filter_map(|f| Some((*f, sc.world(root, *f)?))).collect();
            block[0].layouts = world;
            block[0].parent = None;
            layers.extend(block);
        }
        let mut inputs: Vec<Input> = Vec::new();
        for l in &layers {
            if let Some(input) = l.input.and_then(|i| project.inputs.get(&i))
                && !inputs.iter().any(|i| i.id == input.id)
            {
                inputs.push((**input).clone());
            }
        }
        Some(Clip { layers, inputs })
    }

    /// The commands that paste this into `scene` under `parent` (`None`: the top level), at
    /// `index` among its children (`None`: in front), and the new roots' ids.
    pub fn paste(
        &self,
        project: &Project,
        scene: Id,
        parent: Option<Id>,
        index: Option<usize>,
    ) -> (Vec<Command>, Vec<Id>) {
        let Some(sc) = project.scene(scene) else { return (vec![], vec![]) };
        let mut commands = Vec::new();
        // Inputs: devices the project has are shared; everything else is a fresh copy.
        let mut input_ids: HashMap<Id, Id> = HashMap::new();
        for input in &self.inputs {
            let shared = is_device(&input.kind) && project.inputs.get(&input.id).is_some_and(|i| i.kind == input.kind);
            if shared {
                input_ids.insert(input.id, input.id);
            } else {
                let copy = Input { id: Id::new(), ..input.clone() };
                input_ids.insert(input.id, copy.id);
                commands.push(Command::AddInput { input: copy });
            }
        }
        let mut layer_ids: HashMap<Id, Id> = HashMap::new();
        let mut roots = Vec::new();
        let primary = project.primary_format().id;
        let mut next = index;
        for l in &self.layers {
            let id = Id::new();
            layer_ids.insert(l.id, id);
            let is_root = l.parent.is_none_or(|p| !layer_ids.contains_key(&p));
            let mut layer = Layer {
                id,
                input: l.input.map(|i| input_ids.get(&i).copied().unwrap_or(i)),
                parent: if is_root { parent } else { l.parent.and_then(|p| layer_ids.get(&p).copied()) },
                ..l.clone()
            };
            // Formats this project doesn't have: keep the first layout, in the primary format.
            let first = layer.layouts.values().next().copied();
            layer.layouts.retain(|f, _| project.format(*f).is_some());
            if layer.layouts.is_empty()
                && let Some(lo) = first
            {
                layer.layouts.insert(primary, lo);
            }
            if is_root {
                for (f, lo) in layer.layouts.iter_mut() {
                    if let Some(frame) = sc.frame_of(parent, *f) {
                        *lo = lo.to_local(&frame);
                    }
                }
                roots.push(id);
                commands.push(Command::AddLayer { scene, layer, index: next });
                next = next.map(|i| i + 1);
            } else {
                // Back to front, each in front of the last: the order comes out the same.
                commands.push(Command::AddLayer { scene, layer, index: None });
            }
        }
        (commands, roots)
    }

    /// The commands that place this clip as an instance of prefab `name` (its root linked to
    /// it), and the new root's id.
    pub fn instantiate(
        &self,
        project: &Project,
        scene: Id,
        name: &str,
        parent: Option<Id>,
        index: Option<usize>,
    ) -> (Vec<Command>, Option<Id>) {
        let mut clip = self.clone();
        if let Some(root) = clip.layers.first_mut() {
            root.prefab = Some(name.to_string());
        }
        let (commands, roots) = clip.paste(project, scene, parent, index);
        (commands, roots.first().copied())
    }

    /// The commands that rebuild instance `root` from this clip (a prefab's contents): it keeps
    /// its place in the tree, where it sits on the canvas and its name; everything under it is
    /// the prefab's. Returns the rebuilt root's id.
    pub fn replace_instance(&self, project: &Project, scene: Id, root: Id) -> (Vec<Command>, Option<Id>) {
        let Some(sc) = project.scene(scene) else { return (vec![], None) };
        let Some(old) = sc.layer(root) else { return (vec![], None) };
        let siblings: Vec<Id> = sc.children(old.parent).map(|l| l.id).collect();
        let at = siblings.iter().position(|s| *s == root);
        let mut clip = self.clone();
        let Some(first) = clip.layers.first_mut() else { return (vec![], None) };
        first.layouts = old.layouts.keys().filter_map(|f| Some((*f, sc.world(root, *f)?))).collect();
        first.name = old.name.clone();
        first.prefab = old.prefab.clone();
        let (paste, roots) = clip.paste(project, scene, old.parent, at);
        let mut commands = vec![Command::RemoveLayer { scene, layer: root }];
        commands.extend(paste);
        (commands, roots.first().copied())
    }

    /// The clipboard text for this clip.
    pub fn to_text(&self) -> String {
        format!("{TAG}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// A clip from clipboard text, if it is one.
    pub fn from_text(text: &str) -> Option<Clip> {
        serde_json::from_str(text.strip_prefix(TAG)?).ok()
    }
}

/// A whole scene saved as an asset (a `.scene` file): its name, and every layer with its
/// sources. Added back, it's a new scene with new layers (devices shared, as when pasting).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneAsset {
    pub name: String,
    pub clip: Clip,
}

impl SceneAsset {
    /// `scene` as an asset.
    pub fn of(project: &Project, scene: Id) -> Option<SceneAsset> {
        let sc = project.scene(scene)?;
        let roots: Vec<Id> = sc.layers.iter().filter(|l| l.parent.is_none()).map(|l| l.id).collect();
        let clip = Clip::copy(project, scene, &roots).unwrap_or(Clip { layers: vec![], inputs: vec![] });
        Some(SceneAsset { name: sc.name.clone(), clip })
    }

    /// The commands that add it as a new scene at `index` (`None`: last), and its id.
    pub fn add(&self, project: &Project, index: Option<usize>) -> (Vec<Command>, Id) {
        let scene = Scene::new(self.name.clone());
        let id = scene.id;
        // the layers are pasted into the new scene, as if it were already there
        let mut with = project.clone();
        with.scenes.push(std::sync::Arc::new(scene.clone()));
        let (layers, _) = self.clip.paste(&with, id, None, None);
        let mut commands = vec![Command::AddScene { scene, index }];
        commands.extend(layers);
        (commands, id)
    }
}
