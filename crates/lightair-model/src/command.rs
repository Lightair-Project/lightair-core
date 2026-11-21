//! Commands: plain-data descriptions of every change to the document
//! (docs/03-architecture.md, "Document flow"). UI gestures, hotkeys, the remote API and
//! scripts all produce commands, so they are all validated, undoable and testable alike.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{
    AudioMix, Bus, Canvas, ClipSettings, Destination, Effect, Id, Layout, Master, MixerStrip, Object, Reconnect,
    RecordingSettings, Route, Scene, Source, Stinger, Track, TransitionSettings, VirtualCameraSettings, Workspace,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    #[serde(alias = "AddFormat")]
    AddCanvas {
        #[serde(alias = "format")]
        canvas: Canvas,
    },
    /// A canvas's new name, size or frame rate (sizes even, 16–7680; 1–240 fps).
    #[serde(alias = "UpdateFormat")]
    UpdateCanvas {
        #[serde(alias = "format")]
        canvas: Canvas,
    },
    #[serde(alias = "RemoveFormat")]
    RemoveCanvas {
        #[serde(alias = "format")]
        canvas: Id,
    },

    #[serde(alias = "AddInput")]
    AddSource {
        #[serde(alias = "input")]
        source: Source,
    },
    #[serde(alias = "RemoveInput")]
    RemoveSource {
        #[serde(alias = "input")]
        source: Id,
    },
    #[serde(alias = "RenameInput")]
    RenameSource {
        #[serde(alias = "input")]
        source: Id,
        name: String,
    },
    #[serde(alias = "SetInputSettings")]
    SetSourceSettings {
        #[serde(alias = "input")]
        source: Id,
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
    /// How the scene's screen captures zoom and show the pointer.
    #[serde(alias = "SetSceneScreen")]
    SetScreenZoomPointer {
        scene: Id,
        screen: Option<crate::model::ScreenZoomPointer>,
    },
    /// A scene's own transition (used when switching to it); `None`: the workspace's.
    SetSceneTransition {
        scene: Id,
        transition: Option<TransitionSettings>,
    },
    /// A scene's own stinger video; `None`: the workspace's.
    SetSceneStinger {
        scene: Id,
        stinger: Option<Stinger>,
    },
    MoveScene {
        scene: Id,
        to: usize,
    },
    SetProgram {
        scene: Option<Id>,
    },

    /// `index` counts from the back among the object's siblings (its `parent`'s children);
    /// `None` puts it in front of them.
    #[serde(alias = "AddLayer")]
    AddObject {
        scene: Id,
        #[serde(alias = "layer")]
        object: Object,
        index: Option<usize>,
    },
    #[serde(alias = "RemoveLayer")]
    RemoveObject {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
    },
    #[serde(alias = "RenameLayer")]
    RenameObject {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
        name: String,
    },
    #[serde(alias = "MoveLayer")]
    MoveObject {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
        to: usize,
    },
    /// Moves a object (with everything under it) into `parent` (`None`: the top level) at
    /// `index` among its children, back to front (`None`: in front). It stays where it is on
    /// the canvas.
    #[serde(alias = "ReparentLayer")]
    ReparentObject {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
        parent: Option<Id>,
        index: Option<usize>,
    },
    /// Links an object to a prefab asset (its file and its id), or unlinks it (`None`: it
    /// becomes ordinary objects).
    #[serde(alias = "SetLayerPrefab")]
    SetObjectPrefab {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
        prefab: Option<String>,
        #[serde(default)]
        prefab_id: Option<Id>,
    },
    SetLayout {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
        #[serde(alias = "format")]
        canvas: Id,
        layout: Layout,
    },
    #[serde(alias = "SetLayerVisible")]
    SetObjectVisible {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
        #[serde(alias = "format")]
        canvas: Id,
        visible: bool,
    },
    #[serde(alias = "SetLayerLocked")]
    SetObjectLocked {
        scene: Id,
        #[serde(alias = "layer")]
        object: Id,
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
    /// Puts a destination at place `to` in the list (its order is the order they start in).
    MoveDestination {
        destination: Id,
        to: usize,
    },
    SetDestinationEnabled {
        destination: Id,
        enabled: bool,
    },
    #[serde(alias = "SetDestinationFormat")]
    SetDestinationCanvas {
        destination: Id,
        #[serde(alias = "format")]
        canvas: Option<Id>,
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
    /// What the virtual camera shows (a scene or the one on air, and the canvas).
    SetVirtualCamera {
        settings: VirtualCameraSettings,
    },

    #[serde(alias = "AddInputEffect")]
    AddSourceEffect {
        #[serde(alias = "input")]
        source: Id,
        effect: Effect,
        index: Option<usize>,
    },
    #[serde(alias = "RemoveInputEffect")]
    RemoveSourceEffect {
        #[serde(alias = "input")]
        source: Id,
        effect: Id,
    },
    #[serde(alias = "SetInputEffectEnabled")]
    SetSourceEffectEnabled {
        #[serde(alias = "input")]
        source: Id,
        effect: Id,
        enabled: bool,
    },
    #[serde(alias = "SetInputEffectSettings")]
    SetSourceEffectSettings {
        #[serde(alias = "input")]
        source: Id,
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
    NoObject(Id),
    #[error("no input {0}")]
    NoSource(Id),
    #[error("no canvas {0}")]
    NoCanvas(Id),
    #[error("{0} already exists")]
    Duplicate(Id),
    #[error("the input is used by {0} object(s)")]
    SourceInUse(usize),
    #[error("a workspace needs at least one canvas")]
    LastCanvas,
    #[error("a canvas is 16 to 7680 pixels each way, at 1 to 240 fps")]
    BadCanvas,
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
    /// Human-readable undo label ("Move object", "Add scene").
    pub fn label(&self) -> String {
        match self {
            Command::AddCanvas { .. } => "Add canvas".into(),
            Command::RemoveCanvas { .. } => "Remove canvas".into(),
            Command::UpdateCanvas { .. } => "Change canvas".into(),
            Command::AddSource { .. } => "Add input".into(),
            Command::RemoveSource { .. } => "Remove input".into(),
            Command::RenameSource { .. } => "Rename input".into(),
            Command::SetSourceSettings { .. } => "Change input settings".into(),
            Command::AddScene { .. } => "Add scene".into(),
            Command::RemoveScene { .. } => "Remove scene".into(),
            Command::RenameScene { .. } => "Rename scene".into(),
            Command::SetScreenZoomPointer { .. } => "Change screen zoom and pointer".into(),
            Command::SetSceneTransition { .. } => "Change the scene's transition".into(),
            Command::SetSceneStinger { .. } => "Change the scene's stinger".into(),
            Command::MoveScene { .. } => "Reorder scenes".into(),
            Command::SetProgram { .. } => "Switch scene".into(),
            Command::AddObject { .. } => "Add object".into(),
            Command::RemoveObject { .. } => "Remove object".into(),
            Command::RenameObject { .. } => "Rename object".into(),
            Command::MoveObject { .. } => "Reorder objects".into(),
            Command::ReparentObject { parent: Some(_), .. } => "Move into".into(),
            Command::ReparentObject { parent: None, .. } => "Move out".into(),
            Command::SetObjectPrefab { prefab: Some(_), .. } => "Link to prefab".into(),
            Command::SetObjectPrefab { prefab: None, .. } => "Unpack prefab".into(),
            Command::SetLayout { .. } => "Move object".into(),
            Command::SetObjectVisible { visible: true, .. } => "Show object".into(),
            Command::SetObjectVisible { visible: false, .. } => "Hide object".into(),
            Command::SetObjectLocked { .. } => "Lock object".into(),
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
            Command::MoveDestination { .. } => "Reorder destinations".into(),
            Command::SetDestinationEnabled { .. } => "Toggle destination".into(),
            Command::SetDestinationCanvas { .. } => "Change destination canvas".into(),
            Command::SetRecordingSettings { .. } => "Change recording settings".into(),
            Command::SetStreamBitrate { .. } => "Change stream bitrate".into(),
            Command::SetTransition { .. } => "Change transition".into(),
            Command::SetStinger { .. } => "Change stinger".into(),
            Command::SetClipSettings { .. } => "Change clip settings".into(),
            Command::UpdateDestination { .. } => "Change destination".into(),
            Command::SetReconnect { .. } => "Change reconnecting".into(),
            Command::SetVirtualCamera { .. } => "Change what the virtual camera shows".into(),
            Command::AddSourceEffect { .. } => "Add effect".into(),
            Command::RemoveSourceEffect { .. } => "Remove effect".into(),
            Command::SetSourceEffectEnabled { enabled: true, .. } => "Turn effect on".into(),
            Command::SetSourceEffectEnabled { enabled: false, .. } => "Turn effect off".into(),
            Command::SetSourceEffectSettings { .. } => "Change effect".into(),
            Command::Batch { label, .. } => label.clone(),
        }
    }

    /// Commands with the same key during one gesture merge into a single undo step
    /// (e.g. every pointer move while dragging a object).
    pub fn coalesce_key(&self) -> Option<(&'static str, Id, Id)> {
        match self {
            Command::SetLayout { object, canvas, .. } => Some(("layout", *object, *canvas)),
            Command::SetSourceSettings { source, .. } => Some(("settings", *source, *source)),
            Command::SetTrackVolume { track, .. } => Some(("volume", *track, *track)),
            Command::UpdateTrack { track } => Some(("track", track.id, track.id)),
            Command::UpdateBus { bus } => Some(("bus", bus.id, bus.id)),
            Command::UpdateMix { mix } => Some(("mix", mix.id, mix.id)),
            Command::UpdateMaster { .. } => Some(("master", Id(uuid::Uuid::nil()), Id(uuid::Uuid::nil()))),
            Command::SetStripEffectSettings { strip, effect, .. } => Some(("strip-effect", strip.id(), *effect)),
            Command::SetEffectSettings { track, effect, .. } => Some(("effect", *track, *effect)),
            Command::SetSourceEffectSettings { source, effect, .. } => Some(("input-effect", *source, *effect)),
            // Moving several objects together: one step per gesture, like a single object.
            Command::Batch { commands, .. }
                if !commands.is_empty() && commands.iter().all(|c| matches!(c, Command::SetLayout { .. })) =>
            {
                match &commands[0] {
                    Command::SetLayout { object, canvas, .. } => Some(("layouts", *object, *canvas)),
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

    /// Applies the command to `workspace`, validating it first. On error the workspace is unchanged.
    pub fn apply(&self, workspace: &mut Workspace) -> Result<(), CommandError> {
        match self {
            Command::AddCanvas { canvas } => {
                if workspace.canvas(canvas.id).is_some() {
                    return Err(CommandError::Duplicate(canvas.id));
                }
                workspace.canvases.push(canvas.clone());
            }
            Command::RemoveCanvas { canvas } => {
                let idx =
                    workspace.canvases.iter().position(|f| f.id == *canvas).ok_or(CommandError::NoCanvas(*canvas))?;
                if workspace.canvases.len() == 1 {
                    return Err(CommandError::LastCanvas);
                }
                workspace.canvases.remove(idx);
                for d in &mut workspace.outputs.destinations {
                    if d.canvas == Some(*canvas) {
                        d.canvas = None;
                    }
                }
                if workspace.outputs.recording.canvas == Some(*canvas) {
                    workspace.outputs.recording.canvas = None;
                }
                if workspace.outputs.clips.canvas == Some(*canvas) {
                    workspace.outputs.clips.canvas = None;
                }
                workspace.outputs.clips.canvases.retain(|f| f != canvas);
                if workspace.outputs.virtual_camera.canvas == Some(*canvas) {
                    workspace.outputs.virtual_camera.canvas = None;
                }
                for scene in &mut workspace.scenes {
                    for object in &mut Arc::make_mut(scene).objects {
                        object.layouts.remove(canvas);
                    }
                }
            }
            Command::UpdateCanvas { canvas } => {
                let f = workspace
                    .canvases
                    .iter_mut()
                    .find(|f| f.id == canvas.id)
                    .ok_or(CommandError::NoCanvas(canvas.id))?;
                let fps = canvas.fps_num as f32 / canvas.fps_den.max(1) as f32;
                if !(16..=7680).contains(&canvas.width)
                    || !(16..=7680).contains(&canvas.height)
                    || !(1.0..=240.0).contains(&fps)
                {
                    return Err(CommandError::BadCanvas);
                }
                *f = Canvas {
                    width: canvas.width & !1,
                    height: canvas.height & !1,
                    name: if canvas.name.trim().is_empty() { f.name.clone() } else { canvas.name.clone() },
                    ..canvas.clone()
                };
            }
            Command::AddSource { source } => {
                if workspace.sources.contains_key(&source.id) {
                    return Err(CommandError::Duplicate(source.id));
                }
                workspace.sources.insert(source.id, Arc::new(source.clone()));
            }
            Command::RemoveSource { source } => {
                if !workspace.sources.contains_key(source) {
                    return Err(CommandError::NoSource(*source));
                }
                let users = workspace.objects_using(*source).count()
                    + workspace.tracks.iter().filter(|t| t.source == *source).count();
                if users > 0 {
                    return Err(CommandError::SourceInUse(users));
                }
                workspace.sources.remove(source);
            }
            Command::RenameSource { source, name } => {
                Arc::make_mut(workspace.sources.get_mut(source).ok_or(CommandError::NoSource(*source))?).name =
                    name.clone();
            }
            Command::SetSourceSettings { source, settings } => {
                Arc::make_mut(workspace.sources.get_mut(source).ok_or(CommandError::NoSource(*source))?).settings =
                    settings.clone();
            }
            Command::AddScene { scene, index } => {
                if workspace.scene(scene.id).is_some() {
                    return Err(CommandError::Duplicate(scene.id));
                }
                for object in &scene.objects {
                    check_object(workspace, object)?;
                }
                if !scene.is_well_formed() {
                    return Err(CommandError::BadHierarchy);
                }
                let at = index.unwrap_or(workspace.scenes.len()).min(workspace.scenes.len());
                workspace.scenes.insert(at, Arc::new(scene.clone()));
                if workspace.program.is_none() {
                    workspace.program = Some(scene.id);
                }
            }
            Command::RemoveScene { scene } => {
                let idx = workspace.scene_index(*scene).ok_or(CommandError::NoScene(*scene))?;
                workspace.scenes.remove(idx);
                // the virtual camera on that scene: back to the scene on air
                if workspace.outputs.virtual_camera.scene == Some(*scene) {
                    workspace.outputs.virtual_camera.scene = None;
                }
                if workspace.program == Some(*scene) {
                    workspace.program =
                        workspace.scenes.get(idx.saturating_sub(1)).or(workspace.scenes.first()).map(|s| s.id);
                }
            }
            Command::RenameScene { scene, name } => scene_mut(workspace, *scene)?.name = name.clone(),
            Command::SetScreenZoomPointer { scene, screen } => scene_mut(workspace, *scene)?.screen = *screen,
            Command::MoveScene { scene, to } => {
                let idx = workspace.scene_index(*scene).ok_or(CommandError::NoScene(*scene))?;
                let s = workspace.scenes.remove(idx);
                let to = (*to).min(workspace.scenes.len());
                workspace.scenes.insert(to, s);
            }
            Command::SetProgram { scene } => {
                if let Some(id) = scene {
                    workspace.scene(*id).ok_or(CommandError::NoScene(*id))?;
                }
                workspace.program = *scene;
            }
            Command::AddObject { scene, object, index } => {
                check_object(workspace, object)?;
                let s = scene_mut(workspace, *scene)?;
                if s.object(object.id).is_some() {
                    return Err(CommandError::Duplicate(object.id));
                }
                if let Some(p) = object.parent {
                    s.object(p).ok_or(CommandError::NoObject(p))?;
                }
                s.put_subtree(vec![object.clone()], object.parent, *index);
            }
            Command::RemoveObject { scene, object } => {
                // With everything under it.
                scene_mut(workspace, *scene)?.take_subtree(*object).ok_or(CommandError::NoObject(*object))?;
            }
            Command::RenameObject { scene, object, name } => {
                object_mut(workspace, *scene, *object)?.name = name.clone()
            }
            Command::MoveObject { scene, object, to } => {
                // Among its siblings, with everything under it.
                let s = scene_mut(workspace, *scene)?;
                let parent = s.object(*object).ok_or(CommandError::NoObject(*object))?.parent;
                let block = s.take_subtree(*object).ok_or(CommandError::NoObject(*object))?;
                s.put_subtree(block, parent, Some(*to));
            }
            Command::ReparentObject { scene, object, parent, index } => {
                let s = scene_mut(workspace, *scene)?;
                let l = s.object(*object).ok_or(CommandError::NoObject(*object))?.clone();
                if let Some(p) = parent {
                    s.object(*p).ok_or(CommandError::NoObject(*p))?;
                    if *p == *object || s.is_under(*p, *object) {
                        return Err(CommandError::BadHierarchy);
                    }
                }
                // Same place on the canvas: world layouts re-expressed in the new parent's space.
                let mut layouts = l.layouts.clone();
                for (canvas, local) in layouts.iter_mut() {
                    if let (Some(world), Some(frame)) = (s.world(*object, *canvas), s.frame_of(*parent, *canvas)) {
                        *local = world.to_local(&frame);
                    }
                }
                let mut block = s.take_subtree(*object).ok_or(CommandError::NoObject(*object))?;
                block[0].layouts = layouts;
                s.put_subtree(block, *parent, *index);
            }
            Command::SetLayout { scene, object, canvas, layout } => {
                workspace.canvas(*canvas).ok_or(CommandError::NoCanvas(*canvas))?;
                object_mut(workspace, *scene, *object)?.layouts.insert(*canvas, *layout);
            }
            Command::SetObjectVisible { scene, object, canvas, visible } => {
                workspace.canvas(*canvas).ok_or(CommandError::NoCanvas(*canvas))?;
                let l = object_mut(workspace, *scene, *object)?;
                if let Some(layout) = l.layouts.get_mut(canvas) {
                    layout.visible = *visible;
                }
            }
            Command::SetObjectLocked { scene, object, locked } => {
                object_mut(workspace, *scene, *object)?.locked = *locked
            }
            Command::SetObjectPrefab { scene, object, prefab, prefab_id } => {
                let o = object_mut(workspace, *scene, *object)?;
                o.prefab = prefab.clone();
                o.prefab_id = prefab.as_ref().and(*prefab_id);
            }
            Command::AddTrack { track, index } => {
                if workspace.track(track.id).is_some() {
                    return Err(CommandError::Duplicate(track.id));
                }
                if !workspace.sources.contains_key(&track.source) {
                    return Err(CommandError::NoSource(track.source));
                }
                let at = index.unwrap_or(workspace.tracks.len()).min(workspace.tracks.len());
                workspace.tracks.insert(at, Arc::new(track.clone()));
            }
            Command::RemoveTrack { track } => {
                let idx = workspace.track_index(*track).ok_or(CommandError::NoTrack(*track))?;
                workspace.tracks.remove(idx);
            }
            Command::RenameTrack { track, name } => {
                let t = track_mut(workspace, *track)?;
                t.name = name.clone();
                t.named = true;
            }
            Command::RenameStrip { strip, name } => match *strip {
                MixerStrip::Track(id) => {
                    let t = track_mut(workspace, id)?;
                    t.name = name.clone();
                    t.named = true;
                }
                MixerStrip::Bus(id) => {
                    workspace.audio.buses.iter_mut().find(|b| b.id == id).ok_or(CommandError::NoBus(id))?.name =
                        name.clone()
                }
                MixerStrip::Mix(id) => {
                    workspace.audio.mixes.iter_mut().find(|m| m.id == id).ok_or(CommandError::NoMix(id))?.name =
                        name.clone()
                }
                MixerStrip::Master => {}
            },
            Command::SetStripColor { strip, color } => match *strip {
                MixerStrip::Track(id) => track_mut(workspace, id)?.color = *color,
                MixerStrip::Bus(id) => {
                    workspace.audio.buses.iter_mut().find(|b| b.id == id).ok_or(CommandError::NoBus(id))?.color = *color
                }
                MixerStrip::Mix(id) => {
                    workspace.audio.mixes.iter_mut().find(|m| m.id == id).ok_or(CommandError::NoMix(id))?.color = *color
                }
                MixerStrip::Master => workspace.audio.master.color = *color,
            },
            Command::MoveTrack { track, to } => {
                let idx = workspace.track_index(*track).ok_or(CommandError::NoTrack(*track))?;
                let t = workspace.tracks.remove(idx);
                let to = (*to).min(workspace.tracks.len());
                workspace.tracks.insert(to, t);
            }
            Command::SetTrackVolume { track, volume_db } => {
                track_mut(workspace, *track)?.volume_db = volume_db.clamp(f32::NEG_INFINITY, 12.0)
            }
            Command::SetTrackMuted { track, muted } => track_mut(workspace, *track)?.muted = *muted,
            Command::SetTrackCue { track, cue } => track_mut(workspace, *track)?.cue = *cue,
            Command::UpdateTrack { track } => {
                if !workspace.sources.contains_key(&track.source) {
                    return Err(CommandError::NoSource(track.source));
                }
                *track_mut(workspace, track.id)? = track.clone();
                check_audio(workspace)?;
            }
            Command::AddBus { bus, index } => {
                if workspace.audio.bus(bus.id).is_some() {
                    return Err(CommandError::Duplicate(bus.id));
                }
                let buses = &mut workspace.audio.buses;
                buses.insert(index.unwrap_or(buses.len()).min(buses.len()), bus.clone());
                check_audio(workspace)?;
            }
            Command::RemoveBus { bus } => {
                let idx = workspace.audio.buses.iter().position(|b| b.id == *bus).ok_or(CommandError::NoBus(*bus))?;
                workspace.audio.buses.remove(idx);
                for t in &mut workspace.tracks {
                    if t.route == Route::Bus(*bus) {
                        Arc::make_mut(t).route = Route::Master;
                    }
                }
                for b in &mut workspace.audio.buses {
                    if b.route == Route::Bus(*bus) {
                        b.route = Route::Master;
                    }
                }
            }
            Command::UpdateBus { bus } => {
                let b = workspace.audio.buses.iter_mut().find(|b| b.id == bus.id).ok_or(CommandError::NoBus(bus.id))?;
                *b = bus.clone();
                check_audio(workspace)?;
            }
            Command::AddMix { mix } => {
                if workspace.audio.mix(mix.id).is_some() {
                    return Err(CommandError::Duplicate(mix.id));
                }
                workspace.audio.mixes.push(mix.clone());
            }
            Command::RemoveMix { mix } => {
                let idx = workspace.audio.mixes.iter().position(|m| m.id == *mix).ok_or(CommandError::NoMix(*mix))?;
                workspace.audio.mixes.remove(idx);
                for t in &mut workspace.tracks {
                    if t.sends.iter().any(|s| s.mix == *mix) {
                        Arc::make_mut(t).sends.retain(|s| s.mix != *mix);
                    }
                }
                for b in &mut workspace.audio.buses {
                    b.sends.retain(|s| s.mix != *mix);
                }
                // destinations on it: back to the master, no VOD track
                for d in &mut workspace.outputs.destinations {
                    if d.audio_mix == Some(*mix) {
                        d.audio_mix = None;
                    }
                    if d.vod_mix == Some(*mix) {
                        d.vod_mix = None;
                    }
                }
            }
            Command::UpdateMix { mix } => {
                let m = workspace.audio.mixes.iter_mut().find(|m| m.id == mix.id).ok_or(CommandError::NoMix(mix.id))?;
                *m = mix.clone();
            }
            Command::UpdateMaster { master } => workspace.audio.master = master.clone(),
            Command::AddStripEffect { strip, effect, index } => {
                let list = strip_effects(workspace, *strip)?;
                if list.iter().any(|e| e.id == effect.id) {
                    return Err(CommandError::Duplicate(effect.id));
                }
                let at = index.unwrap_or(list.len()).min(list.len());
                list.insert(at, effect.clone());
            }
            Command::RemoveStripEffect { strip, effect } => {
                let list = strip_effects(workspace, *strip)?;
                let at = list.iter().position(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?;
                list.remove(at);
            }
            Command::SetStripEffectEnabled { strip, effect, enabled } => {
                let list = strip_effects(workspace, *strip)?;
                list.iter_mut().find(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?.enabled = *enabled;
            }
            Command::SetStripEffectSettings { strip, effect, settings } => {
                let list = strip_effects(workspace, *strip)?;
                list.iter_mut().find(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?.settings =
                    settings.clone();
            }
            Command::AddEffect { track, effect, index } => {
                let t = track_mut(workspace, *track)?;
                if t.effects.iter().any(|e| e.id == effect.id) {
                    return Err(CommandError::Duplicate(effect.id));
                }
                let at = index.unwrap_or(t.effects.len()).min(t.effects.len());
                t.effects.insert(at, effect.clone());
            }
            Command::RemoveEffect { track, effect } => {
                let t = track_mut(workspace, *track)?;
                let idx = t.effects.iter().position(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?;
                t.effects.remove(idx);
            }
            Command::SetEffectEnabled { track, effect, enabled } => {
                effect_mut(workspace, *track, *effect)?.enabled = *enabled
            }
            Command::SetEffectSettings { track, effect, settings } => {
                effect_mut(workspace, *track, *effect)?.settings = settings.clone()
            }
            Command::AddDestination { destination } => {
                if workspace.outputs.destinations.iter().any(|d| d.id == destination.id) {
                    return Err(CommandError::Duplicate(destination.id));
                }
                workspace.outputs.destinations.push(destination.clone());
            }
            Command::RemoveDestination { destination } => {
                let before = workspace.outputs.destinations.len();
                workspace.outputs.destinations.retain(|d| d.id != *destination);
                if workspace.outputs.destinations.len() == before {
                    return Err(CommandError::NoDestination(*destination));
                }
            }
            Command::MoveDestination { destination, to } => {
                let list = &mut workspace.outputs.destinations;
                let idx =
                    list.iter().position(|d| d.id == *destination).ok_or(CommandError::NoDestination(*destination))?;
                let d = list.remove(idx);
                list.insert((*to).min(list.len()), d);
            }
            Command::SetDestinationEnabled { destination, enabled } => {
                workspace
                    .outputs
                    .destinations
                    .iter_mut()
                    .find(|d| d.id == *destination)
                    .ok_or(CommandError::NoDestination(*destination))?
                    .enabled = *enabled;
            }
            Command::SetDestinationCanvas { destination, canvas } => {
                if let Some(f) = canvas {
                    workspace.canvas(*f).ok_or(CommandError::NoCanvas(*f))?;
                }
                workspace
                    .outputs
                    .destinations
                    .iter_mut()
                    .find(|d| d.id == *destination)
                    .ok_or(CommandError::NoDestination(*destination))?
                    .canvas = *canvas;
            }
            Command::SetRecordingSettings { settings } => workspace.outputs.recording = settings.clone(),
            Command::UpdateDestination { destination } => {
                if let Some(f) = destination.canvas {
                    workspace.canvas(f).ok_or(CommandError::NoCanvas(f))?;
                }
                let d = workspace
                    .outputs
                    .destinations
                    .iter_mut()
                    .find(|d| d.id == destination.id)
                    .ok_or(CommandError::NoDestination(destination.id))?;
                *d = destination.clone();
            }
            Command::SetReconnect { reconnect } => {
                workspace.outputs.reconnect =
                    Reconnect { retries: reconnect.retries.min(100), delay_s: reconnect.delay_s.clamp(1, 60) }
            }
            Command::SetVirtualCamera { settings } => {
                if let Some(s) = settings.scene {
                    workspace.scene(s).ok_or(CommandError::NoScene(s))?;
                }
                if let Some(f) = settings.canvas {
                    workspace.canvas(f).ok_or(CommandError::NoCanvas(f))?;
                }
                workspace.outputs.virtual_camera = *settings;
            }
            Command::SetStreamBitrate { kbps } => workspace.outputs.stream_bitrate_kbps = (*kbps).clamp(500, 51_000),
            Command::SetClipSettings { settings } => {
                for f in settings.canvas.iter().chain(&settings.canvases) {
                    workspace.canvas(*f).ok_or(CommandError::NoCanvas(*f))?;
                }
                workspace.outputs.clips = ClipSettings { seconds: settings.seconds.clamp(10, 300), ..settings.clone() }
            }
            Command::SetSceneTransition { scene, transition } => {
                scene_mut(workspace, *scene)?.transition =
                    transition.map(|t| TransitionSettings { duration_ms: t.duration_ms.clamp(50, 5000), ..t })
            }
            Command::SetSceneStinger { scene, stinger } => scene_mut(workspace, *scene)?.stinger = stinger.clone(),
            Command::SetTransition { settings } => {
                workspace.transition =
                    TransitionSettings { duration_ms: settings.duration_ms.clamp(50, 5000), ..*settings }
            }
            Command::SetStinger { stinger } => workspace.stinger = stinger.clone(),
            Command::AddSourceEffect { source, effect, index } => {
                let i = Arc::make_mut(workspace.sources.get_mut(source).ok_or(CommandError::NoSource(*source))?);
                if i.effects.iter().any(|e| e.id == effect.id) {
                    return Err(CommandError::Duplicate(effect.id));
                }
                let at = index.unwrap_or(i.effects.len()).min(i.effects.len());
                i.effects.insert(at, effect.clone());
            }
            Command::RemoveSourceEffect { source, effect } => {
                let i = Arc::make_mut(workspace.sources.get_mut(source).ok_or(CommandError::NoSource(*source))?);
                let idx = i.effects.iter().position(|e| e.id == *effect).ok_or(CommandError::NoEffect(*effect))?;
                i.effects.remove(idx);
            }
            Command::SetSourceEffectEnabled { source, effect, enabled } => {
                source_effect_mut(workspace, *source, *effect)?.enabled = *enabled
            }
            Command::SetSourceEffectSettings { source, effect, settings } => {
                source_effect_mut(workspace, *source, *effect)?.settings = settings.clone()
            }
            Command::Batch { commands, .. } => {
                // Apply to a scratch copy so a failing step leaves the workspace untouched.
                let mut scratch = workspace.clone();
                for c in commands {
                    c.apply(&mut scratch)?;
                }
                *workspace = scratch;
            }
        }
        Ok(())
    }
}

/// A mixer strip's effect list.
fn strip_effects(workspace: &mut Workspace, strip: MixerStrip) -> Result<&mut Vec<Effect>, CommandError> {
    Ok(match strip {
        MixerStrip::Track(id) => &mut track_mut(workspace, id)?.effects,
        MixerStrip::Bus(id) => {
            &mut workspace.audio.buses.iter_mut().find(|b| b.id == id).ok_or(CommandError::NoBus(id))?.effects
        }
        MixerStrip::Mix(id) => {
            &mut workspace.audio.mixes.iter_mut().find(|m| m.id == id).ok_or(CommandError::NoMix(id))?.effects
        }
        MixerStrip::Master => &mut workspace.audio.master.effects,
    })
}

/// Routes go to buses that exist, without loops; sends go to mixes that exist.
fn check_audio(workspace: &Workspace) -> Result<(), CommandError> {
    let a = &workspace.audio;
    let strips = workspace
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

fn track_mut(workspace: &mut Workspace, id: Id) -> Result<&mut Track, CommandError> {
    let idx = workspace.track_index(id).ok_or(CommandError::NoTrack(id))?;
    Ok(Arc::make_mut(&mut workspace.tracks[idx]))
}

fn source_effect_mut(workspace: &mut Workspace, source: Id, effect: Id) -> Result<&mut Effect, CommandError> {
    Arc::make_mut(workspace.sources.get_mut(&source).ok_or(CommandError::NoSource(source))?)
        .effects
        .iter_mut()
        .find(|e| e.id == effect)
        .ok_or(CommandError::NoEffect(effect))
}

fn effect_mut(workspace: &mut Workspace, track: Id, effect: Id) -> Result<&mut Effect, CommandError> {
    track_mut(workspace, track)?.effects.iter_mut().find(|e| e.id == effect).ok_or(CommandError::NoEffect(effect))
}

fn check_object(workspace: &Workspace, object: &Object) -> Result<(), CommandError> {
    if let Some(source) = object.source
        && !workspace.sources.contains_key(&source)
    {
        return Err(CommandError::NoSource(source));
    }
    for canvas in object.layouts.keys() {
        workspace.canvas(*canvas).ok_or(CommandError::NoCanvas(*canvas))?;
    }
    Ok(())
}

fn scene_mut(workspace: &mut Workspace, id: Id) -> Result<&mut Scene, CommandError> {
    let idx = workspace.scene_index(id).ok_or(CommandError::NoScene(id))?;
    Ok(Arc::make_mut(&mut workspace.scenes[idx]))
}

fn object_mut(workspace: &mut Workspace, scene: Id, object: Id) -> Result<&mut Object, CommandError> {
    scene_mut(workspace, scene)?.objects.iter_mut().find(|l| l.id == object).ok_or(CommandError::NoObject(object))
}

impl Source {
    pub fn new(name: impl Into<String>, kind: impl Into<String>, settings: serde_json::Value) -> Self {
        Self { id: Id::new(), name: name.into(), kind: kind.into(), settings, effects: Vec::new() }
    }
}

impl Object {
    /// A object for `source`, laid out the same way in every given canvas.
    pub fn new(name: impl Into<String>, source: Id, canvases: impl IntoIterator<Item = (Id, Layout)>) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            source: Some(source),
            parent: None,
            prefab: None,
            prefab_id: None,
            layouts: canvases.into_iter().collect(),
            locked: false,
            link: None,
        }
    }

    /// An empty with its pivot at `at` in every given canvas.
    pub fn empty(name: impl Into<String>, canvases: impl IntoIterator<Item = (Id, (f32, f32))>) -> Self {
        Self {
            id: Id::new(),
            name: name.into(),
            source: None,
            parent: None,
            prefab: None,
            prefab_id: None,
            layouts: canvases.into_iter().map(|(f, (x, y))| (f, Layout::boxed(x, y, 0.0, 0.0))).collect(),
            locked: false,
            link: None,
        }
    }

    /// This object under `parent`.
    pub fn under(self, parent: Id) -> Self {
        Self { parent: Some(parent), ..self }
    }
}
