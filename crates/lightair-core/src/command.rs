//! Commands: plain-data descriptions of every change to the document
//! (docs/03-architecture.md, "Document flow"). UI gestures, hotkeys, the remote API and
//! scripts all produce commands, so they are all validated, undoable and testable alike.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{
    ClipSettings, Destination, Effect, Format, Id, Input, Layer, Layout, Project, RecordingSettings, Scene, Track,
    TransitionSettings,
};

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

    AddTrack {
        track: Track,
        index: Option<usize>,
    },
    RemoveTrack {
        track: Id,
    },
    RenameTrack {
        track: Id,
        name: String,
    },
    MoveTrack {
        track: Id,
        to: usize,
    },
    SetTrackVolume {
        track: Id,
        volume_db: f32,
    },
    SetTrackMuted {
        track: Id,
        muted: bool,
    },
    SetTrackCue {
        track: Id,
        cue: bool,
    },
    AddEffect {
        track: Id,
        effect: Effect,
        index: Option<usize>,
    },
    RemoveEffect {
        track: Id,
        effect: Id,
    },
    SetEffectEnabled {
        track: Id,
        effect: Id,
        enabled: bool,
    },
    SetEffectSettings {
        track: Id,
        effect: Id,
        settings: serde_json::Value,
    },

    AddDestination {
        destination: Destination,
    },
    RemoveDestination {
        destination: Id,
    },
    SetDestinationEnabled {
        destination: Id,
        enabled: bool,
    },
    SetDestinationFormat {
        destination: Id,
        format: Option<Id>,
    },
    SetRecordingSettings {
        settings: RecordingSettings,
    },
    SetStreamBitrate {
        kbps: u32,
    },

    SetTransition {
        settings: TransitionSettings,
    },
    SetClipSettings {
        settings: ClipSettings,
    },

    AddInputEffect {
        input: Id,
        effect: Effect,
        index: Option<usize>,
    },
    RemoveInputEffect {
        input: Id,
        effect: Id,
    },
    SetInputEffectEnabled {
        input: Id,
        effect: Id,
        enabled: bool,
    },
    SetInputEffectSettings {
        input: Id,
        effect: Id,
        settings: serde_json::Value,
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
    #[error("no track {0}")]
    NoTrack(Id),
    #[error("no effect {0}")]
    NoEffect(Id),
    #[error("no destination {0}")]
    NoDestination(Id),
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
            Command::AddTrack { .. } => "Add track".into(),
            Command::RemoveTrack { .. } => "Remove track".into(),
            Command::RenameTrack { .. } => "Rename track".into(),
            Command::MoveTrack { .. } => "Reorder tracks".into(),
            Command::SetTrackVolume { .. } => "Change volume".into(),
            Command::SetTrackMuted { muted: true, .. } => "Mute".into(),
            Command::SetTrackMuted { muted: false, .. } => "Unmute".into(),
            Command::SetTrackCue { .. } => "Cue".into(),
            Command::AddEffect { .. } => "Add effect".into(),
            Command::RemoveEffect { .. } => "Remove effect".into(),
            Command::SetEffectEnabled { enabled: true, .. } => "Turn effect on".into(),
            Command::SetEffectEnabled { enabled: false, .. } => "Bypass effect".into(),
            Command::SetEffectSettings { .. } => "Change effect".into(),
            Command::AddDestination { .. } => "Add destination".into(),
            Command::RemoveDestination { .. } => "Remove destination".into(),
            Command::SetDestinationEnabled { .. } => "Toggle destination".into(),
            Command::SetDestinationFormat { .. } => "Change destination format".into(),
            Command::SetRecordingSettings { .. } => "Change recording settings".into(),
            Command::SetStreamBitrate { .. } => "Change stream bitrate".into(),
            Command::SetTransition { .. } => "Change transition".into(),
            Command::SetClipSettings { .. } => "Change clip settings".into(),
            Command::AddInputEffect { .. } => "Add effect".into(),
            Command::RemoveInputEffect { .. } => "Remove effect".into(),
            Command::SetInputEffectEnabled { enabled: true, .. } => "Turn effect on".into(),
            Command::SetInputEffectEnabled { enabled: false, .. } => "Turn effect off".into(),
            Command::SetInputEffectSettings { .. } => "Change effect".into(),
            Command::Batch { label, .. } => label.clone(),
        }
    }

    /// Commands with the same key during one gesture merge into a single undo step
    /// (e.g. every pointer move while dragging a layer).
    pub fn coalesce_key(&self) -> Option<(&'static str, Id, Id)> {
        match self {
            Command::SetLayout { layer, format, .. } => Some(("layout", *layer, *format)),
            Command::SetInputSettings { input, .. } => Some(("settings", *input, *input)),
            Command::SetTrackVolume { track, .. } => Some(("volume", *track, *track)),
            Command::SetEffectSettings { track, effect, .. } => Some(("effect", *track, *effect)),
            Command::SetInputEffectSettings { input, effect, .. } => Some(("input-effect", *input, *effect)),
            // Moving several layers together: one step per gesture, like a single layer.
            Command::Batch { commands, .. }
                if !commands.is_empty() && commands.iter().all(|c| matches!(c, Command::SetLayout { .. })) =>
            {
                match &commands[0] {
                    Command::SetLayout { layer, format, .. } => Some(("layouts", *layer, *format)),
                    _ => None,
                }
            }
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
                for d in &mut project.outputs.destinations {
                    if d.format == Some(*format) {
                        d.format = None;
                    }
                }
                if project.outputs.recording.format == Some(*format) {
                    project.outputs.recording.format = None;
                }
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
                let users =
                    project.layers_using(*input).count() + project.tracks.iter().filter(|t| t.input == *input).count();
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
            Command::AddTrack { track, index } => {
                if project.track(track.id).is_some() {
                    return Err(CommandError::Duplicate(track.id));
                }
                if !project.inputs.contains_key(&track.input) {
                    return Err(CommandError::NoInput(track.input));
                }
                let at = index.unwrap_or(project.tracks.len()).min(project.tracks.len());
                project.tracks.insert(at, Arc::new(track.clone()));
            }
            Command::RemoveTrack { track } => {
                let idx = project.track_index(*track).ok_or(CommandError::NoTrack(*track))?;
                project.tracks.remove(idx);
            }
            Command::RenameTrack { track, name } => track_mut(project, *track)?.name = name.clone(),
            Command::MoveTrack { track, to } => {
                let idx = project.track_index(*track).ok_or(CommandError::NoTrack(*track))?;
                let t = project.tracks.remove(idx);
                let to = (*to).min(project.tracks.len());
                project.tracks.insert(to, t);
            }
            Command::SetTrackVolume { track, volume_db } => {
                track_mut(project, *track)?.volume_db = volume_db.clamp(f32::NEG_INFINITY, 12.0)
            }
            Command::SetTrackMuted { track, muted } => track_mut(project, *track)?.muted = *muted,
            Command::SetTrackCue { track, cue } => track_mut(project, *track)?.cue = *cue,
            Command::AddEffect { track, effect, index } => {
                let t = track_mut(project, *track)?;
                if t.effects.iter().any(|e| e.id == effect.id) {
                    return Err(CommandError::Duplicate(effect.id));
                }
                let at = index.unwrap_or(t.effects.len()).min(t.effects.len());
                t.effects.insert(at, effect.clone());
            }
            Command::RemoveEffect { track, effect } => {
                let t = track_mut(project, *track)?;
                let idx = t.effects.iter().position(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?;
                t.effects.remove(idx);
            }
            Command::SetEffectEnabled { track, effect, enabled } => {
                effect_mut(project, *track, *effect)?.enabled = *enabled
            }
            Command::SetEffectSettings { track, effect, settings } => {
                effect_mut(project, *track, *effect)?.settings = settings.clone()
            }
            Command::AddDestination { destination } => {
                if project.outputs.destinations.iter().any(|d| d.id == destination.id) {
                    return Err(CommandError::Duplicate(destination.id));
                }
                project.outputs.destinations.push(destination.clone());
            }
            Command::RemoveDestination { destination } => {
                let before = project.outputs.destinations.len();
                project.outputs.destinations.retain(|d| d.id != *destination);
                if project.outputs.destinations.len() == before {
                    return Err(CommandError::NoDestination(*destination));
                }
            }
            Command::SetDestinationEnabled { destination, enabled } => {
                project
                    .outputs
                    .destinations
                    .iter_mut()
                    .find(|d| d.id == *destination)
                    .ok_or(CommandError::NoDestination(*destination))?
                    .enabled = *enabled;
            }
            Command::SetDestinationFormat { destination, format } => {
                if let Some(f) = format {
                    project.format(*f).ok_or(CommandError::NoFormat(*f))?;
                }
                project
                    .outputs
                    .destinations
                    .iter_mut()
                    .find(|d| d.id == *destination)
                    .ok_or(CommandError::NoDestination(*destination))?
                    .format = *format;
            }
            Command::SetRecordingSettings { settings } => project.outputs.recording = settings.clone(),
            Command::SetStreamBitrate { kbps } => project.outputs.stream_bitrate_kbps = (*kbps).clamp(500, 51_000),
            Command::SetClipSettings { settings } => {
                if let Some(f) = settings.format {
                    project.format(f).ok_or(CommandError::NoFormat(f))?;
                }
                project.outputs.clips = ClipSettings { seconds: settings.seconds.clamp(10, 300), ..*settings }
            }
            Command::SetTransition { settings } => {
                project.transition =
                    TransitionSettings { duration_ms: settings.duration_ms.clamp(50, 5000), ..*settings }
            }
            Command::AddInputEffect { input, effect, index } => {
                let i = Arc::make_mut(project.inputs.get_mut(input).ok_or(CommandError::NoInput(*input))?);
                if i.effects.iter().any(|e| e.id == effect.id) {
                    return Err(CommandError::Duplicate(effect.id));
                }
                let at = index.unwrap_or(i.effects.len()).min(i.effects.len());
                i.effects.insert(at, effect.clone());
            }
            Command::RemoveInputEffect { input, effect } => {
                let i = Arc::make_mut(project.inputs.get_mut(input).ok_or(CommandError::NoInput(*input))?);
                let idx = i.effects.iter().position(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?;
                i.effects.remove(idx);
            }
            Command::SetInputEffectEnabled { input, effect, enabled } => {
                input_effect_mut(project, *input, *effect)?.enabled = *enabled
            }
            Command::SetInputEffectSettings { input, effect, settings } => {
                input_effect_mut(project, *input, *effect)?.settings = settings.clone()
            }
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

fn track_mut(project: &mut Project, id: Id) -> Result<&mut Track, CommandError> {
    let idx = project.track_index(id).ok_or(CommandError::NoTrack(id))?;
    Ok(Arc::make_mut(&mut project.tracks[idx]))
}

fn input_effect_mut(project: &mut Project, input: Id, effect: Id) -> Result<&mut Effect, CommandError> {
    Arc::make_mut(project.inputs.get_mut(&input).ok_or(CommandError::NoInput(input))?)
        .effects
        .iter_mut()
        .find(|e| e.id == effect)
        .ok_or(CommandError::NoEffect(effect))
}

fn effect_mut(project: &mut Project, track: Id, effect: Id) -> Result<&mut Effect, CommandError> {
    track_mut(project, track)?.effects.iter_mut().find(|e| e.id == effect).ok_or(CommandError::NoEffect(effect))
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
        Self { id: Id::new(), name: name.into(), kind: kind.into(), settings, effects: Vec::new() }
    }
}

impl Layer {
    /// A layer for `input`, laid out the same way in every given format.
    pub fn new(name: impl Into<String>, input: Id, formats: impl IntoIterator<Item = (Id, Layout)>) -> Self {
        Self { id: Id::new(), name: name.into(), input, layouts: formats.into_iter().collect(), locked: false }
    }
}
