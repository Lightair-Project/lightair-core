//! Commands: plain-data descriptions of every change to the document
//! (docs/03-architecture.md, "Document flow"). UI gestures, hotkeys, the remote API and
//! scripts all produce commands, so they are all validated, undoable and testable alike.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{Format, Id, Input, Layer, Layout, Project, Scene};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    AddFormat {
        format: Format,
    },
    RemoveFormat {
        format: Id,
    },

    AddInput {
        input: Input,
    },
    RemoveInput {
        input: Id,
    },
    RenameInput {
        input: Id,
        name: String,
    },
    SetInputSettings {
        input: Id,
        settings: serde_json::Value,
    },

    AddScene {
        scene: Scene,
        index: Option<usize>,
    },
    RemoveScene {
        scene: Id,
    },
    RenameScene {
        scene: Id,
        name: String,
    },
    MoveScene {
        scene: Id,
        to: usize,
    },
    SetProgram {
        scene: Option<Id>,
    },

    /// `index` counts from the bottom; `None` puts the layer on top.
    AddLayer {
        scene: Id,
        layer: Layer,
        index: Option<usize>,
    },
    RemoveLayer {
        scene: Id,
        layer: Id,
    },
    RenameLayer {
        scene: Id,
        layer: Id,
        name: String,
    },
    MoveLayer {
        scene: Id,
        layer: Id,
        to: usize,
    },
    SetLayout {
        scene: Id,
        layer: Id,
        format: Id,
        layout: Layout,
    },
    SetLayerVisible {
        scene: Id,
        layer: Id,
        format: Id,
        visible: bool,
    },
    SetLayerLocked {
        scene: Id,
        layer: Id,
        locked: bool,
    },

    /// Several commands applied as one undo step.
    Batch {
        label: String,
        commands: Vec<Command>,
    },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CommandError {
    #[error("no scene {0}")]
    NoScene(Id),
    #[error("no layer {0}")]
    NoLayer(Id),
    #[error("no input {0}")]
    NoInput(Id),
    #[error("no format {0}")]
    NoFormat(Id),
    #[error("{0} already exists")]
    Duplicate(Id),
    #[error("the input is used by {0} layer(s)")]
    InputInUse(usize),
    #[error("a project needs at least one format")]
    LastFormat,
}

impl Command {
    /// Human-readable undo label ("Move layer", "Add scene").
    pub fn label(&self) -> String {
        match self {
            Command::AddFormat { .. } => "Add format".into(),
            Command::RemoveFormat { .. } => "Remove format".into(),
            Command::AddInput { .. } => "Add input".into(),
            Command::RemoveInput { .. } => "Remove input".into(),
            Command::RenameInput { .. } => "Rename input".into(),
            Command::SetInputSettings { .. } => "Change input settings".into(),
            Command::AddScene { .. } => "Add scene".into(),
            Command::RemoveScene { .. } => "Remove scene".into(),
            Command::RenameScene { .. } => "Rename scene".into(),
            Command::MoveScene { .. } => "Reorder scenes".into(),
            Command::SetProgram { .. } => "Switch scene".into(),
            Command::AddLayer { .. } => "Add layer".into(),
            Command::RemoveLayer { .. } => "Remove layer".into(),
            Command::RenameLayer { .. } => "Rename layer".into(),
            Command::MoveLayer { .. } => "Reorder layers".into(),
            Command::SetLayout { .. } => "Move layer".into(),
            Command::SetLayerVisible { visible: true, .. } => "Show layer".into(),
            Command::SetLayerVisible { visible: false, .. } => "Hide layer".into(),
            Command::SetLayerLocked { .. } => "Lock layer".into(),
            Command::Batch { label, .. } => label.clone(),
        }
    }

    /// Commands with the same key during one gesture merge into a single undo step
    /// (e.g. every pointer move while dragging a layer).
    pub fn coalesce_key(&self) -> Option<(&'static str, Id, Id)> {
        match self {
            Command::SetLayout { layer, format, .. } => Some(("layout", *layer, *format)),
            Command::SetInputSettings { input, .. } => Some(("settings", *input, *input)),
            _ => None,
        }
    }

    /// Whether this change belongs in undo history. Switching scenes on air is an action, not
    /// an edit.
    pub fn is_undoable(&self) -> bool {
        !matches!(self, Command::SetProgram { .. })
    }

    /// Applies the command to `project`, validating it first. On error the project is unchanged.
    pub fn apply(&self, project: &mut Project) -> Result<(), CommandError> {
        match self {
            Command::AddFormat { format } => {
                if project.format(format.id).is_some() {
                    return Err(CommandError::Duplicate(format.id));
                }
                project.formats.push(format.clone());
            }
            Command::RemoveFormat { format } => {
                let idx =
                    project.formats.iter().position(|f| f.id == *format).ok_or(CommandError::NoFormat(*format))?;
                if project.formats.len() == 1 {
                    return Err(CommandError::LastFormat);
                }
                project.formats.remove(idx);
                for scene in &mut project.scenes {
                    for layer in &mut Arc::make_mut(scene).layers {
                        layer.layouts.remove(format);
                    }
                }
            }
            Command::AddInput { input } => {
                if project.inputs.contains_key(&input.id) {
                    return Err(CommandError::Duplicate(input.id));
                }
                project.inputs.insert(input.id, Arc::new(input.clone()));
            }
            Command::RemoveInput { input } => {
                if !project.inputs.contains_key(input) {
                    return Err(CommandError::NoInput(*input));
                }
                let users = project.layers_using(*input).count();
                if users > 0 {
                    return Err(CommandError::InputInUse(users));
                }
                project.inputs.remove(input);
            }
            Command::RenameInput { input, name } => {
                Arc::make_mut(project.inputs.get_mut(input).ok_or(CommandError::NoInput(*input))?).name = name.clone();
            }
            Command::SetInputSettings { input, settings } => {
                Arc::make_mut(project.inputs.get_mut(input).ok_or(CommandError::NoInput(*input))?).settings =
                    settings.clone();
            }
            Command::AddScene { scene, index } => {
                if project.scene(scene.id).is_some() {
                    return Err(CommandError::Duplicate(scene.id));
                }
                for layer in &scene.layers {
                    check_layer(project, layer)?;
                }
                let at = index.unwrap_or(project.scenes.len()).min(project.scenes.len());
                project.scenes.insert(at, Arc::new(scene.clone()));
                if project.program.is_none() {
                    project.program = Some(scene.id);
                }
            }
            Command::RemoveScene { scene } => {
                let idx = project.scene_index(*scene).ok_or(CommandError::NoScene(*scene))?;
                project.scenes.remove(idx);
                if project.program == Some(*scene) {
                    project.program =
                        project.scenes.get(idx.saturating_sub(1)).or(project.scenes.first()).map(|s| s.id);
                }
            }
            Command::RenameScene { scene, name } => scene_mut(project, *scene)?.name = name.clone(),
            Command::MoveScene { scene, to } => {
                let idx = project.scene_index(*scene).ok_or(CommandError::NoScene(*scene))?;
                let s = project.scenes.remove(idx);
                let to = (*to).min(project.scenes.len());
                project.scenes.insert(to, s);
            }
            Command::SetProgram { scene } => {
                if let Some(id) = scene {
                    project.scene(*id).ok_or(CommandError::NoScene(*id))?;
                }
                project.program = *scene;
            }
            Command::AddLayer { scene, layer, index } => {
                check_layer(project, layer)?;
                let s = scene_mut(project, *scene)?;
                if s.layer(layer.id).is_some() {
                    return Err(CommandError::Duplicate(layer.id));
                }
                let at = index.unwrap_or(s.layers.len()).min(s.layers.len());
                s.layers.insert(at, layer.clone());
            }
            Command::RemoveLayer { scene, layer } => {
                let s = scene_mut(project, *scene)?;
                let idx = s.layer_index(*layer).ok_or(CommandError::NoLayer(*layer))?;
                s.layers.remove(idx);
            }
            Command::RenameLayer { scene, layer, name } => layer_mut(project, *scene, *layer)?.name = name.clone(),
            Command::MoveLayer { scene, layer, to } => {
                let s = scene_mut(project, *scene)?;
                let idx = s.layer_index(*layer).ok_or(CommandError::NoLayer(*layer))?;
                let l = s.layers.remove(idx);
                let to = (*to).min(s.layers.len());
                s.layers.insert(to, l);
            }
            Command::SetLayout { scene, layer, format, layout } => {
                project.format(*format).ok_or(CommandError::NoFormat(*format))?;
                layer_mut(project, *scene, *layer)?.layouts.insert(*format, *layout);
            }
            Command::SetLayerVisible { scene, layer, format, visible } => {
                project.format(*format).ok_or(CommandError::NoFormat(*format))?;
                let l = layer_mut(project, *scene, *layer)?;
                if let Some(layout) = l.layouts.get_mut(format) {
                    layout.visible = *visible;
                }
            }
            Command::SetLayerLocked { scene, layer, locked } => layer_mut(project, *scene, *layer)?.locked = *locked,
            Command::Batch { commands, .. } => {
                // Apply to a scratch copy so a failing step leaves the project untouched.
                let mut scratch = project.clone();
                for c in commands {
                    c.apply(&mut scratch)?;
                }
                *project = scratch;
            }
        }
        Ok(())
    }
}

fn check_layer(project: &Project, layer: &Layer) -> Result<(), CommandError> {
    if !project.inputs.contains_key(&layer.input) {
        return Err(CommandError::NoInput(layer.input));
    }
    for format in layer.layouts.keys() {
        project.format(*format).ok_or(CommandError::NoFormat(*format))?;
    }
    Ok(())
}

fn scene_mut(project: &mut Project, id: Id) -> Result<&mut Scene, CommandError> {
    let idx = project.scene_index(id).ok_or(CommandError::NoScene(id))?;
    Ok(Arc::make_mut(&mut project.scenes[idx]))
}

fn layer_mut(project: &mut Project, scene: Id, layer: Id) -> Result<&mut Layer, CommandError> {
    scene_mut(project, scene)?.layers.iter_mut().find(|l| l.id == layer).ok_or(CommandError::NoLayer(layer))
}

impl Input {
    pub fn new(name: impl Into<String>, kind: impl Into<String>, settings: serde_json::Value) -> Self {
        Self { id: Id::new(), name: name.into(), kind: kind.into(), settings }
    }
}

impl Layer {
    /// A layer for `input`, laid out the same way in every given format.
    pub fn new(name: impl Into<String>, input: Id, formats: impl IntoIterator<Item = (Id, Layout)>) -> Self {
        Self { id: Id::new(), name: name.into(), input, layouts: formats.into_iter().collect(), locked: false }
    }
}
