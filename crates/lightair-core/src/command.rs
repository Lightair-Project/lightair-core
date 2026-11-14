//! Commands: plain-data descriptions of every change to the document
//! (docs/03-architecture.md, "Document flow"). UI gestures, hotkeys, the remote API and
//! scripts all produce commands, so they are all validated, undoable and testable alike.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{
    ClipSettings, Destination, Effect, Format, Id, Input, Layer, Layout, Project, Reconnect, RecordingSettings, Scene,
    Stinger, Track, TransitionSettings, AudioMix, Bus, Master, Route, MixerStrip,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    AddFormat {
        format: Format,
    },
    /// A canvas's new name, size or frame rate (sizes even, 16–7680; 1–240 fps).
    UpdateFormat {
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

    /// `index` counts from the back among the layer's siblings (its `parent`'s children);
    /// `None` puts it in front of them.
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
    /// Moves a layer (with everything under it) into `parent` (`None`: the top level) at
    /// `index` among its children, back to front (`None`: in front). It stays where it is on
    /// the canvas.
    ReparentLayer {
        scene: Id,
        layer: Id,
        parent: Option<Id>,
        index: Option<usize>,
    },
    /// Links a layer to a prefab asset, or unlinks it (`None`: it becomes ordinary layers).
    SetLayerPrefab {
        scene: Id,
        layer: Id,
        prefab: Option<String>,
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
    /// Renames a track, bus or mix (Program keeps its name).
    RenameStrip {
        strip: MixerStrip,
        name: String,
    },
    /// A strip's colour in the mixer (`None`: the default).
    SetStripColor {
        strip: MixerStrip,
        color: Option<u32>,
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
    /// Replaces a track (its strip: channels, pan, trim, routing, sends, effects…).
    UpdateTrack {
        track: Track,
    },
    AddBus {
        bus: Bus,
        index: Option<usize>,
    },
    /// Strips routed to it go to Program.
    RemoveBus {
        bus: Id,
    },
    UpdateBus {
        bus: Bus,
    },
    AddMix {
        mix: AudioMix,
    },
    /// Sends to it are dropped.
    RemoveMix {
        mix: Id,
    },
    UpdateMix {
        mix: AudioMix,
    },
    UpdateMaster {
        master: Master,
    },
    /// Effects on any mixer strip (a track, bus, mix or the master).
    AddStripEffect {
        strip: MixerStrip,
        effect: Effect,
        index: Option<usize>,
    },
    RemoveStripEffect {
        strip: MixerStrip,
        effect: Id,
    },
    SetStripEffectEnabled {
        strip: MixerStrip,
        effect: Id,
        enabled: bool,
    },
    SetStripEffectSettings {
        strip: MixerStrip,
        effect: Id,
        settings: serde_json::Value,
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
    SetStinger {
        stinger: Option<Stinger>,
    },
    SetClipSettings {
        settings: ClipSettings,
    },
    /// Replaces a destination's settings (matched by id).
    UpdateDestination {
        destination: Destination,
    },
    SetReconnect {
        reconnect: Reconnect,
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
    #[error("no object {0}")]
    NoLayer(Id),
    #[error("no input {0}")]
    NoInput(Id),
    #[error("no format {0}")]
    NoFormat(Id),
    #[error("{0} already exists")]
    Duplicate(Id),
    #[error("the input is used by {0} object(s)")]
    InputInUse(usize),
    #[error("a project needs at least one format")]
    LastFormat,
    #[error("a canvas is 16 to 7680 pixels each way, at 1 to 240 fps")]
    BadFormat,
    #[error("an object can't sit inside itself, and its parent must come before it")]
    BadHierarchy,
    #[error("no track {0}")]
    NoTrack(Id),
    #[error("no effect {0}")]
    NoEffect(Id),
    #[error("no destination {0}")]
    NoDestination(Id),
    #[error("no bus {0}")]
    NoBus(Id),
    #[error("no mix {0}")]
    NoMix(Id),
    #[error("buses can't feed each other in a loop")]
    AudioLoop,
}

impl Command {
    /// Human-readable undo label ("Move layer", "Add scene").
    pub fn label(&self) -> String {
        match self {
            Command::AddFormat { .. } => "Add format".into(),
            Command::RemoveFormat { .. } => "Remove format".into(),
            Command::UpdateFormat { .. } => "Change canvas".into(),
            Command::AddInput { .. } => "Add input".into(),
            Command::RemoveInput { .. } => "Remove input".into(),
            Command::RenameInput { .. } => "Rename input".into(),
            Command::SetInputSettings { .. } => "Change input settings".into(),
            Command::AddScene { .. } => "Add scene".into(),
            Command::RemoveScene { .. } => "Remove scene".into(),
            Command::RenameScene { .. } => "Rename scene".into(),
            Command::MoveScene { .. } => "Reorder scenes".into(),
            Command::SetProgram { .. } => "Switch scene".into(),
            Command::AddLayer { .. } => "Add object".into(),
            Command::RemoveLayer { .. } => "Remove object".into(),
            Command::RenameLayer { .. } => "Rename object".into(),
            Command::MoveLayer { .. } => "Reorder objects".into(),
            Command::ReparentLayer { parent: Some(_), .. } => "Move into".into(),
            Command::ReparentLayer { parent: None, .. } => "Move out".into(),
            Command::SetLayerPrefab { prefab: Some(_), .. } => "Link to prefab".into(),
            Command::SetLayerPrefab { prefab: None, .. } => "Unpack prefab".into(),
            Command::SetLayout { .. } => "Move object".into(),
            Command::SetLayerVisible { visible: true, .. } => "Show object".into(),
            Command::SetLayerVisible { visible: false, .. } => "Hide object".into(),
            Command::SetLayerLocked { .. } => "Lock object".into(),
            Command::AddTrack { .. } => "Add track".into(),
            Command::RemoveTrack { .. } => "Remove track".into(),
            Command::RenameTrack { .. } => "Rename track".into(),
            Command::RenameStrip { .. } => "Rename".into(),
            Command::SetStripColor { .. } => "Strip colour".into(),
            Command::MoveTrack { .. } => "Reorder tracks".into(),
            Command::SetTrackVolume { .. } => "Change volume".into(),
            Command::SetTrackMuted { muted: true, .. } => "Mute".into(),
            Command::SetTrackMuted { muted: false, .. } => "Unmute".into(),
            Command::SetTrackCue { .. } => "Listen in headphones".into(),
            Command::UpdateTrack { track } => format!("Change {}", track.name),
            Command::AddBus { .. } => "Add bus".into(),
            Command::RemoveBus { .. } => "Remove bus".into(),
            Command::UpdateBus { bus } => format!("Change {}", bus.name),
            Command::AddMix { .. } => "Add mix".into(),
            Command::RemoveMix { .. } => "Remove mix".into(),
            Command::UpdateMix { mix } => format!("Change {}", mix.name),
            Command::UpdateMaster { .. } => "Change master".into(),
            Command::AddStripEffect { .. } => "Add effect".into(),
            Command::RemoveStripEffect { .. } => "Remove effect".into(),
            Command::SetStripEffectEnabled { enabled: true, .. } => "Turn effect on".into(),
            Command::SetStripEffectEnabled { enabled: false, .. } => "Bypass effect".into(),
            Command::SetStripEffectSettings { .. } => "Change effect".into(),
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
            Command::SetStinger { .. } => "Change stinger".into(),
            Command::SetClipSettings { .. } => "Change clip settings".into(),
            Command::UpdateDestination { .. } => "Change destination".into(),
            Command::SetReconnect { .. } => "Change reconnecting".into(),
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
            Command::UpdateTrack { track } => Some(("track", track.id, track.id)),
            Command::UpdateBus { bus } => Some(("bus", bus.id, bus.id)),
            Command::UpdateMix { mix } => Some(("mix", mix.id, mix.id)),
            Command::UpdateMaster { .. } => Some(("master", Id(uuid::Uuid::nil()), Id(uuid::Uuid::nil()))),
            Command::SetStripEffectSettings { strip, effect, .. } => Some(("strip-effect", strip.id(), *effect)),
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
                if project.outputs.clips.format == Some(*format) {
                    project.outputs.clips.format = None;
                }
                for scene in &mut project.scenes {
                    for layer in &mut Arc::make_mut(scene).layers {
                        layer.layouts.remove(format);
                    }
                }
            }
            Command::UpdateFormat { format } => {
                let f =
                    project.formats.iter_mut().find(|f| f.id == format.id).ok_or(CommandError::NoFormat(format.id))?;
                let fps = format.fps_num as f32 / format.fps_den.max(1) as f32;
                if !(16..=7680).contains(&format.width)
                    || !(16..=7680).contains(&format.height)
                    || !(1.0..=240.0).contains(&fps)
                {
                    return Err(CommandError::BadFormat);
                }
                *f = Format {
                    width: format.width & !1,
                    height: format.height & !1,
                    name: if format.name.trim().is_empty() { f.name.clone() } else { format.name.clone() },
                    ..format.clone()
                };
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
                if !scene.is_well_formed() {
                    return Err(CommandError::BadHierarchy);
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
                if let Some(p) = layer.parent {
                    s.layer(p).ok_or(CommandError::NoLayer(p))?;
                }
                s.put_subtree(vec![layer.clone()], layer.parent, *index);
            }
            Command::RemoveLayer { scene, layer } => {
                // With everything under it.
                scene_mut(project, *scene)?.take_subtree(*layer).ok_or(CommandError::NoLayer(*layer))?;
            }
            Command::RenameLayer { scene, layer, name } => layer_mut(project, *scene, *layer)?.name = name.clone(),
            Command::MoveLayer { scene, layer, to } => {
                // Among its siblings, with everything under it.
                let s = scene_mut(project, *scene)?;
                let parent = s.layer(*layer).ok_or(CommandError::NoLayer(*layer))?.parent;
                let block = s.take_subtree(*layer).ok_or(CommandError::NoLayer(*layer))?;
                s.put_subtree(block, parent, Some(*to));
            }
            Command::ReparentLayer { scene, layer, parent, index } => {
                let s = scene_mut(project, *scene)?;
                let l = s.layer(*layer).ok_or(CommandError::NoLayer(*layer))?.clone();
                if let Some(p) = parent {
                    s.layer(*p).ok_or(CommandError::NoLayer(*p))?;
                    if *p == *layer || s.is_under(*p, *layer) {
                        return Err(CommandError::BadHierarchy);
                    }
                }
                // Same place on the canvas: world layouts re-expressed in the new parent's space.
                let mut layouts = l.layouts.clone();
                for (format, local) in layouts.iter_mut() {
                    if let (Some(world), Some(frame)) = (s.world(*layer, *format), s.frame_of(*parent, *format)) {
                        *local = world.to_local(&frame);
                    }
                }
                let mut block = s.take_subtree(*layer).ok_or(CommandError::NoLayer(*layer))?;
                block[0].layouts = layouts;
                s.put_subtree(block, *parent, *index);
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
            Command::SetLayerPrefab { scene, layer, prefab } => {
                layer_mut(project, *scene, *layer)?.prefab = prefab.clone()
            }
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
            Command::RenameStrip { strip, name } => match *strip {
                MixerStrip::Track(id) => track_mut(project, id)?.name = name.clone(),
                MixerStrip::Bus(id) => {
                    project.audio.buses.iter_mut().find(|b| b.id == id).ok_or(CommandError::NoBus(id))?.name = name.clone()
                }
                MixerStrip::Mix(id) => {
                    project.audio.mixes.iter_mut().find(|m| m.id == id).ok_or(CommandError::NoMix(id))?.name = name.clone()
                }
                MixerStrip::Master => {}
            },
            Command::SetStripColor { strip, color } => match *strip {
                MixerStrip::Track(id) => track_mut(project, id)?.color = *color,
                MixerStrip::Bus(id) => {
                    project.audio.buses.iter_mut().find(|b| b.id == id).ok_or(CommandError::NoBus(id))?.color = *color
                }
                MixerStrip::Mix(id) => {
                    project.audio.mixes.iter_mut().find(|m| m.id == id).ok_or(CommandError::NoMix(id))?.color = *color
                }
                MixerStrip::Master => project.audio.master.color = *color,
            },
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
            Command::UpdateTrack { track } => {
                if !project.inputs.contains_key(&track.input) {
                    return Err(CommandError::NoInput(track.input));
                }
                *track_mut(project, track.id)? = track.clone();
                check_audio(project)?;
            }
            Command::AddBus { bus, index } => {
                if project.audio.bus(bus.id).is_some() {
                    return Err(CommandError::Duplicate(bus.id));
                }
                let buses = &mut project.audio.buses;
                buses.insert(index.unwrap_or(buses.len()).min(buses.len()), bus.clone());
                check_audio(project)?;
            }
            Command::RemoveBus { bus } => {
                let idx = project.audio.buses.iter().position(|b| b.id == *bus).ok_or(CommandError::NoBus(*bus))?;
                project.audio.buses.remove(idx);
                for t in &mut project.tracks {
                    if t.route == Route::Bus(*bus) {
                        Arc::make_mut(t).route = Route::Program;
                    }
                }
                for b in &mut project.audio.buses {
                    if b.route == Route::Bus(*bus) {
                        b.route = Route::Program;
                    }
                }
            }
            Command::UpdateBus { bus } => {
                let b = project.audio.buses.iter_mut().find(|b| b.id == bus.id).ok_or(CommandError::NoBus(bus.id))?;
                *b = bus.clone();
                check_audio(project)?;
            }
            Command::AddMix { mix } => {
                if project.audio.mix(mix.id).is_some() {
                    return Err(CommandError::Duplicate(mix.id));
                }
                project.audio.mixes.push(mix.clone());
            }
            Command::RemoveMix { mix } => {
                let idx = project.audio.mixes.iter().position(|m| m.id == *mix).ok_or(CommandError::NoMix(*mix))?;
                project.audio.mixes.remove(idx);
                for t in &mut project.tracks {
                    if t.sends.iter().any(|s| s.mix == *mix) {
                        Arc::make_mut(t).sends.retain(|s| s.mix != *mix);
                    }
                }
                for b in &mut project.audio.buses {
                    b.sends.retain(|s| s.mix != *mix);
                }
            }
            Command::UpdateMix { mix } => {
                let m = project.audio.mixes.iter_mut().find(|m| m.id == mix.id).ok_or(CommandError::NoMix(mix.id))?;
                *m = mix.clone();
            }
            Command::UpdateMaster { master } => project.audio.master = master.clone(),
            Command::AddStripEffect { strip, effect, index } => {
                let list = strip_effects(project, *strip)?;
                if list.iter().any(|e| e.id == effect.id) {
                    return Err(CommandError::Duplicate(effect.id));
                }
                let at = index.unwrap_or(list.len()).min(list.len());
                list.insert(at, effect.clone());
            }
            Command::RemoveStripEffect { strip, effect } => {
                let list = strip_effects(project, *strip)?;
                let at = list.iter().position(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?;
                list.remove(at);
            }
            Command::SetStripEffectEnabled { strip, effect, enabled } => {
                let list = strip_effects(project, *strip)?;
                list.iter_mut().find(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?.enabled = *enabled;
            }
            Command::SetStripEffectSettings { strip, effect, settings } => {
                let list = strip_effects(project, *strip)?;
                list.iter_mut().find(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?.settings =
                    settings.clone();
            }
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
            Command::UpdateDestination { destination } => {
                if let Some(f) = destination.format {
                    project.format(f).ok_or(CommandError::NoFormat(f))?;
                }
                let d = project
                    .outputs
                    .destinations
                    .iter_mut()
                    .find(|d| d.id == destination.id)
                    .ok_or(CommandError::NoDestination(destination.id))?;
                *d = destination.clone();
            }
            Command::SetReconnect { reconnect } => {
                project.outputs.reconnect =
                    Reconnect { retries: reconnect.retries.min(100), delay_s: reconnect.delay_s.clamp(1, 60) }
            }
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
            Command::SetStinger { stinger } => project.stinger = stinger.clone(),
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

/// A mixer strip's effect list.
fn strip_effects(project: &mut Project, strip: MixerStrip) -> Result<&mut Vec<Effect>, CommandError> {
    Ok(match strip {
        MixerStrip::Track(id) => &mut track_mut(project, id)?.effects,
        MixerStrip::Bus(id) => {
            &mut project.audio.buses.iter_mut().find(|b| b.id == id).ok_or(CommandError::NoBus(id))?.effects
        }
        MixerStrip::Mix(id) => {
            &mut project.audio.mixes.iter_mut().find(|m| m.id == id).ok_or(CommandError::NoMix(id))?.effects
        }
        MixerStrip::Master => &mut project.audio.master.effects,
    })
}

/// Routes go to buses that exist, without loops; sends go to mixes that exist.
fn check_audio(project: &Project) -> Result<(), CommandError> {
    let a = &project.audio;
    let strips = project
        .tracks
        .iter()
        .map(|t| (t.id, t.route, &t.sends))
        .chain(a.buses.iter().map(|b| (b.id, b.route, &b.sends)));
    for (id, route, sends) in strips {
        if let Route::Bus(b) = route {
            if a.bus(b).is_none() {
                return Err(CommandError::NoBus(b));
            }
            // follow the chain of buses: it must reach Program without coming back
            let mut seen = vec![id];
            let mut cur = b;
            while let Some(bus) = a.bus(cur) {
                if seen.contains(&cur) {
                    return Err(CommandError::AudioLoop);
                }
                seen.push(cur);
                match bus.route {
                    Route::Bus(next) => cur = next,
                    _ => break,
                }
            }
        }
        if let Some(s) = sends.iter().find(|s| a.mix(s.mix).is_none()) {
            return Err(CommandError::NoMix(s.mix));
        }
    }
    Ok(())
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
    if let Some(input) = layer.input
        && !project.inputs.contains_key(&input)
    {
        return Err(CommandError::NoInput(input));
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
        Self {
            id: Id::new(),
            name: name.into(),
            input: Some(input),
            parent: None,
            prefab: None,
            layouts: formats.into_iter().collect(),
            locked: false,
        }
    }

    /// An empty with its pivot at `at` in every given format.
    pub fn empty(name: impl Into<String>, formats: impl IntoIterator<Item = (Id, (f32, f32))>) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            input: None,
            parent: None,
            prefab: None,
            layouts: formats.into_iter().map(|(f, (x, y))| (f, Layout::boxed(x, y, 0.0, 0.0))).collect(),
            locked: false,
        }
    }

    /// This layer under `parent`.
    pub fn under(self, parent: Id) -> Self {
        Self { parent: Some(parent), ..self }
    }
}
