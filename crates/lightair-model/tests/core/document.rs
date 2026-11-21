use std::sync::Arc;

use lightair_model::{
    Command, CommandError, Document, Layout, Object, ObjectPatch, Patch, Scene, Source, Workspace, diff, diff_objects,
};
use proptest::prelude::*;

fn setup() -> (Document, lightair_model::Id, lightair_model::Id, lightair_model::Id) {
    let workspace = Workspace::new("Show");
    let canvas = workspace.primary_canvas().id;
    let mut doc = Document::new(workspace);
    let cam = Source::new("Camera", lightair_model::kinds::CAMERA, serde_json::json!({}));
    let cam_id = cam.id;
    doc.apply(&Command::AddSource { source: cam }).unwrap();
    let scene = Scene::new("Main");
    let scene_id = scene.id;
    doc.apply(&Command::AddScene { scene, index: None }).unwrap();
    let object = Object::new("Camera", cam_id, [(canvas, Layout::boxed(0.0, 0.0, 1920.0, 1080.0))]);
    let object_id = object.id;
    doc.apply(&Command::AddObject { scene: scene_id, object, index: None }).unwrap();
    (doc, scene_id, object_id, canvas)
}

#[test]
fn first_scene_goes_on_air() {
    let (doc, scene, ..) = setup();
    assert_eq!(doc.workspace().program, Some(scene));
}

#[test]
fn undo_and_redo_restore_snapshots() {
    let (mut doc, scene, object, canvas) = setup();
    let before = doc.workspace().clone();
    doc.apply(&Command::SetLayout { scene, object, canvas, layout: Layout::boxed(10.0, 20.0, 640.0, 360.0) }).unwrap();
    assert_eq!(doc.undo_label(), Some("Move object"));
    doc.undo().unwrap();
    assert_eq!(**doc.workspace(), *before);
    doc.redo().unwrap();
    assert_eq!(doc.workspace().scene(scene).unwrap().object(object).unwrap().layouts[&canvas].x, 10.0);
}

#[test]
fn a_drag_gesture_is_one_undo_step() {
    let (mut doc, scene, object, canvas) = setup();
    let steps = doc_undo_depth(&doc);
    doc.begin_gesture();
    for x in 0..50 {
        doc.apply(&Command::SetLayout { scene, object, canvas, layout: Layout::boxed(x as f32, 0.0, 100.0, 100.0) })
            .unwrap();
    }
    doc.end_gesture();
    assert_eq!(doc_undo_depth(&doc), steps + 1);
    doc.undo().unwrap();
    assert_eq!(doc.workspace().scene(scene).unwrap().object(object).unwrap().layouts[&canvas].x, 0.0);
    // The next drag is a separate step.
    doc.begin_gesture();
    doc.apply(&Command::SetLayout { scene, object, canvas, layout: Layout::boxed(5.0, 0.0, 100.0, 100.0) }).unwrap();
    doc.end_gesture();
    assert_eq!(doc_undo_depth(&doc), steps + 1);
}

#[test]
fn dragging_several_objects_is_one_undo_step() {
    let (mut doc, scene, first, canvas) = setup();
    let cam = doc.workspace().scene(scene).unwrap().object(first).unwrap().source.unwrap();
    let second = Object::new("Copy", cam, [(canvas, Layout::boxed(0.0, 0.0, 100.0, 100.0))]);
    let second_id = second.id;
    doc.apply(&Command::AddObject { scene, object: second, index: None }).unwrap();
    let steps = doc_undo_depth(&doc);
    doc.begin_gesture();
    for x in 1..20 {
        let commands = [first, second_id]
            .map(|object| Command::SetLayout {
                scene,
                object,
                canvas,
                layout: Layout::boxed(x as f32, 0.0, 100.0, 100.0),
            })
            .to_vec();
        doc.apply(&Command::Batch { label: "Move layers".into(), commands }).unwrap();
    }
    doc.end_gesture();
    assert_eq!(doc_undo_depth(&doc), steps + 1);
    doc.undo().unwrap();
    assert_eq!(doc.workspace().scene(scene).unwrap().object(second_id).unwrap().layouts[&canvas].x, 0.0);
}

#[test]
fn the_virtual_camera_choice_is_kept_and_follows_removals() {
    let (mut doc, scene, _, _) = setup();
    let square = lightair_model::Canvas::square();
    let sq = square.id;
    doc.apply(&Command::AddCanvas { canvas: square }).unwrap();
    let settings = lightair_model::VirtualCameraSettings { scene: Some(scene), canvas: Some(sq) };
    doc.apply(&Command::SetVirtualCamera { settings }).unwrap();
    assert_eq!(doc.workspace().outputs.virtual_camera, settings, "kept in the workspace");
    // saved with the workspace
    let text = serde_json::to_string(&doc.workspace().outputs).unwrap();
    let back: lightair_model::OutputSettings = serde_json::from_str(&text).unwrap();
    assert_eq!(back.virtual_camera, settings);
    doc.undo();
    assert_eq!(doc.workspace().outputs.virtual_camera, Default::default(), "undo brings back the scene on air");
    doc.redo();
    let ghost = lightair_model::Id::new();
    assert!(
        doc.apply(&Command::SetVirtualCamera {
            settings: lightair_model::VirtualCameraSettings { scene: Some(ghost), canvas: None }
        })
        .is_err()
    );
    doc.apply(&Command::RemoveCanvas { canvas: sq }).unwrap();
    assert_eq!(doc.workspace().outputs.virtual_camera.canvas, None, "its canvas gone: the primary");
    doc.apply(&Command::RemoveScene { scene }).unwrap();
    assert_eq!(doc.workspace().outputs.virtual_camera.scene, None, "its scene gone: the scene on air");
}

#[test]
fn removing_a_canvas_retargets_outputs() {
    let (mut doc, scene, object, _) = setup();
    let square = lightair_model::Canvas::square();
    let sq = square.id;
    doc.apply(&Command::AddCanvas { canvas: square }).unwrap();
    let mut clips = doc.workspace().outputs.clips;
    clips.canvas = Some(sq);
    doc.apply(&Command::SetClipSettings { settings: clips }).unwrap();
    doc.apply(&Command::SetLayout { scene, object, canvas: sq, layout: Layout::boxed(0.0, 0.0, 1080.0, 1080.0) })
        .unwrap();
    doc.apply(&Command::RemoveCanvas { canvas: sq }).unwrap();
    assert_eq!(doc.workspace().outputs.clips.canvas, None, "clips fall back to the primary format");
    assert!(!doc.workspace().scene(scene).unwrap().object(object).unwrap().layouts.contains_key(&sq));
    let odd = lightair_model::Canvas::custom("Odd", 1001, 99999);
    assert_eq!((odd.width, odd.height), (1000, 7680));
}

#[test]
fn uncropped_box() {
    use lightair_model::Crop;
    // 1920×1080 source shown at half size with 200 px cropped off the left and 100 off the top.
    let l =
        Layout { crop: Crop { left: 200, top: 100, right: 0, bottom: 0 }, ..Layout::boxed(100.0, 50.0, 860.0, 490.0) };
    let u = l.uncropped((1920.0, 1080.0));
    assert!((u.x - 0.0).abs() < 0.01 && (u.y - 0.0).abs() < 0.01, "{u:?}");
    assert!((u.width - 960.0).abs() < 0.01 && (u.height - 540.0).abs() < 0.01, "{u:?}");
    assert_eq!(u.crop, Crop::default());
    // No crop: the same box.
    let plain = Layout::boxed(10.0, 20.0, 300.0, 200.0);
    let p = plain.uncropped((600.0, 400.0));
    assert_eq!((p.x, p.y, p.width, p.height), (10.0, 20.0, 300.0, 200.0));
    // Turned half a turn, the cropped-off left and top are on screen right and bottom: the box
    // keeps its top-left at (100, 50) and grows to (1060, 590).
    let r = Layout { rotation: 180.0, ..l };
    let ur = r.uncropped((1920.0, 1080.0));
    assert!((ur.x - 100.0).abs() < 0.05 && (ur.y - 50.0).abs() < 0.05, "{ur:?}");
}

fn doc_undo_depth(doc: &Document) -> usize {
    let mut d = doc.clone();
    let mut n = 0;
    while d.undo().is_some() {
        n += 1;
    }
    n
}

#[test]
fn switching_scenes_is_not_undone() {
    let (mut doc, main, ..) = setup();
    let brb = Scene::new("BRB");
    let brb_id = brb.id;
    doc.apply(&Command::AddScene { scene: brb, index: None }).unwrap();
    doc.apply(&Command::SetProgram { scene: Some(brb_id) }).unwrap();
    assert_eq!(doc.undo_label(), Some("Add scene"));
    doc.apply(&Command::RenameScene { scene: main, name: "Gameplay".into() }).unwrap();
    doc.undo().unwrap();
    assert_eq!(doc.workspace().program, Some(brb_id), "undo must not change what's on air");
}

#[test]
fn failing_commands_change_nothing() {
    let (mut doc, scene, ..) = setup();
    let before = doc.workspace().clone();
    let ghost = lightair_model::Id::new();
    assert_eq!(doc.apply(&Command::RemoveObject { scene, object: ghost }).unwrap_err(), CommandError::NoObject(ghost));
    let cam = *doc.workspace().sources.keys().next().unwrap();
    assert!(matches!(doc.apply(&Command::RemoveSource { source: cam }), Err(CommandError::SourceInUse(1))));
    // A batch that fails halfway leaves no trace.
    let batch = Command::Batch {
        label: "Rename and break".into(),
        commands: vec![Command::RenameScene { scene, name: "Renamed".into() }, Command::RemoveScene { scene: ghost }],
    };
    assert!(doc.apply(&batch).is_err());
    assert!(Arc::ptr_eq(doc.workspace(), &before) || **doc.workspace() == *before);
}

#[test]
fn unchanged_scenes_share_memory() {
    let (mut doc, scene, ..) = setup();
    let other = Scene::new("Other");
    let other_id = other.id;
    doc.apply(&Command::AddScene { scene: other, index: None }).unwrap();
    let before = doc.workspace().clone();
    doc.apply(&Command::RenameScene { scene, name: "Main 2".into() }).unwrap();
    let after = doc.workspace();
    assert!(Arc::ptr_eq(before.scene(other_id).unwrap(), after.scene(other_id).unwrap()));
}

#[test]
fn diff_orders_and_detects_changes() {
    let (doc, scene, object, canvas) = setup();
    let empty = doc.workspace().empty_like();
    let patches = diff(&empty, doc.workspace());
    let kinds: Vec<&str> = patches
        .iter()
        .map(|p| match p {
            Patch::SourceAdded(_) => "input+",
            Patch::SceneAdded(_) => "scene+",
            Patch::ProgramChanged(_) => "program",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["input+", "scene+", "program"]);

    let mut doc2 = doc.clone();
    doc2.apply(&Command::SetLayout { scene, object, canvas, layout: Layout::boxed(1.0, 2.0, 3.0, 4.0) }).unwrap();
    let patches = diff(doc.workspace(), doc2.workspace());
    let Patch::SceneChanged { before, after } = &patches[0] else { panic!("{patches:?}") };
    assert!(matches!(diff_objects(before, after)[..], [ObjectPatch::Changed { .. }]));
    assert_eq!(patches.len(), 1);
}

#[test]
fn object_reorder_is_one_order_patch() {
    let (mut doc, scene, _object, canvas) = setup();
    let cam = *doc.workspace().sources.keys().next().unwrap();
    let top = Object::new("Top", cam, [(canvas, Layout::boxed(0.0, 0.0, 10.0, 10.0))]);
    let top_id = top.id;
    doc.apply(&Command::AddObject { scene, object: top, index: None }).unwrap();
    let before = doc.workspace().scene(scene).unwrap().clone();
    doc.apply(&Command::MoveObject { scene, object: top_id, to: 0 }).unwrap();
    let after = doc.workspace().scene(scene).unwrap().clone();
    assert!(matches!(diff_objects(&before, &after)[..], [ObjectPatch::Order(_)]));
}

#[test]
fn workspace_round_trips_through_json() {
    let (doc, ..) = setup();
    let json = serde_json::to_string(&**doc.workspace()).unwrap();
    let back: Workspace = serde_json::from_str(&json).unwrap();
    assert_eq!(back, **doc.workspace());
}

// Random command sequences: undoing everything returns to the start, redoing everything
// returns to the end, and diff(start, end) is empty only when they are equal.
#[derive(Debug, Clone)]
enum Op {
    AddScene,
    RenameFirst(u8),
    AddObjectToFirst,
    MoveFirstObject(f32),
    RemoveLastScene,
    ReorderObjects,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::AddScene),
        any::<u8>().prop_map(Op::RenameFirst),
        Just(Op::AddObjectToFirst),
        (0f32..1000.0).prop_map(Op::MoveFirstObject),
        Just(Op::RemoveLastScene),
        Just(Op::ReorderObjects),
    ]
}

proptest! {
    #[test]
    fn undo_redo_round_trip(ops in proptest::collection::vec(op(), 1..40)) {
        let (setup_doc, ..) = setup();
        // History starts after setup, so "undo everything" returns exactly to `start`.
        let mut doc = Document::new((**setup_doc.workspace()).clone());
        let start = doc.workspace().clone();
        let canvas = start.primary_canvas().id;
        let cam = *start.sources.keys().next().unwrap();
        for op in ops {
            let p = doc.workspace().clone();
            let cmd = match op {
                Op::AddScene => Some(Command::AddScene { scene: Scene::new("S"), index: None }),
                Op::RenameFirst(n) => p.scenes.first().map(|s| Command::RenameScene { scene: s.id, name: format!("S{n}") }),
                Op::AddObjectToFirst => p.scenes.first().map(|s| Command::AddObject {
                    scene: s.id, object: Object::new("L", cam, [(canvas, Layout::boxed(0.0, 0.0, 1.0, 1.0))]), index: None }),
                Op::MoveFirstObject(x) => p.scenes.first().and_then(|s| s.objects.first().map(|l| Command::SetLayout {
                    scene: s.id, object: l.id, canvas, layout: Layout::boxed(x, 0.0, 1.0, 1.0) })),
                Op::RemoveLastScene => p.scenes.last().map(|s| Command::RemoveScene { scene: s.id }),
                Op::ReorderObjects => p.scenes.first().and_then(|s| s.objects.last().map(|l| Command::MoveObject {
                    scene: s.id, object: l.id, to: 0 })),
            };
            if let Some(cmd) = cmd { let _ = doc.apply(&cmd); }
        }
        let end = doc.workspace().clone();
        while doc.undo().is_some() {}
        prop_assert_eq!(&doc.workspace().scenes, &start.scenes);
        prop_assert_eq!(&doc.workspace().sources, &start.sources);
        while doc.redo().is_some() {}
        prop_assert_eq!(&doc.workspace().scenes, &end.scenes);
        prop_assert_eq!(diff(&end, doc.workspace()).is_empty(), true);
    }
}

#[test]
fn journal_entries_from_before_the_renames_still_read() {
    use lightair_model::Command;
    // a canvas was a "format": AddFormat { format }, SetLayout { …, format }
    let id = lightair_model::Id::new();
    let old = format!(r#"{{"RemoveFormat":{{"format":"{id}"}}}}"#);
    let cmd: Command = serde_json::from_str(&old).unwrap();
    assert!(matches!(cmd, Command::RemoveCanvas { canvas } if canvas == id));
}

#[test]
fn journal_entries_from_before_sources_were_sources_still_read() {
    use lightair_model::Command;
    let id = lightair_model::Id::new();
    let old = format!(r#"{{"RemoveInput":{{"input":"{id}"}}}}"#);
    let cmd: Command = serde_json::from_str(&old).unwrap();
    assert!(matches!(cmd, Command::RemoveSource { source } if source == id));
}

#[test]
fn removing_a_mix_takes_it_off_destinations() {
    let (mut doc, ..) = setup();
    let vod = lightair_model::AudioMix::new("VOD (no music)");
    let mix = vod.id;
    doc.apply(&Command::AddMix { mix: vod }).unwrap();
    let destination = lightair_model::Destination {
        id: lightair_model::Id::new(),
        name: "Twitch".into(),
        server: "rtmp://live.twitch.tv/app".into(),
        enabled: true,
        canvas: None,
        platform: Some("twitch".into()),
        encoding: None,
        audio_mix: Some(mix),
        vod_mix: Some(mix),
        chat: None,
        options: Default::default(),
    };
    doc.apply(&Command::AddDestination { destination }).unwrap();
    doc.apply(&Command::RemoveMix { mix }).unwrap();
    let d = &doc.workspace().outputs.destinations[0];
    assert_eq!((d.audio_mix, d.vod_mix), (None, None), "back to the master, no VOD track");
}

#[test]
fn a_scene_can_have_its_own_transition() {
    let (mut doc, scene, ..) = setup();
    let own = lightair_model::TransitionSettings {
        kind: lightair_model::TransitionKind::Dip,
        duration_ms: 99_999,
        ..doc.workspace().transition
    };
    doc.apply(&Command::SetSceneTransition { scene, transition: Some(own) }).unwrap();
    let t = doc.workspace().scene(scene).unwrap().transition.unwrap();
    assert_eq!(t.kind, lightair_model::TransitionKind::Dip);
    assert_eq!(t.duration_ms, 5000, "clamped like the default's");
    // kept with the scene
    let json = serde_json::to_string(&**doc.workspace().scene(scene).unwrap()).unwrap();
    let back: lightair_model::Scene = serde_json::from_str(&json).unwrap();
    assert_eq!(back.transition, Some(t));
    // its own stinger video, kept with it
    let st = lightair_model::Stinger { path: "/videos/intro.mov".into(), cut_ms: 700 };
    doc.apply(&Command::SetSceneStinger { scene, stinger: Some(st.clone()) }).unwrap();
    assert_eq!(doc.workspace().scene(scene).unwrap().stinger, Some(st));
    doc.apply(&Command::SetSceneTransition { scene, transition: None }).unwrap();
    assert_eq!(doc.workspace().scene(scene).unwrap().transition, None, "back to the default");
}

#[test]
fn a_destination_from_before_options_is_adaptive() {
    // Old workspaces have no `options`: they get the defaults, and defaults aren't written back.
    let json = r#"{"id":"00000000-0000-0000-0000-000000000001","name":"Twitch","server":"rtmp://x/app","enabled":true}"#;
    let d: lightair_model::Destination = serde_json::from_str(json).unwrap();
    assert!(d.options.adaptive_bitrate);
    assert!(!serde_json::to_string(&d).unwrap().contains("options"));
    let mut d = d;
    d.options.adaptive_bitrate = false;
    d.options.reconnect = Some(lightair_model::Reconnect { retries: 3, delay_s: 7 });
    let back: lightair_model::Destination = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert!(!back.options.adaptive_bitrate);
    assert_eq!(back.options.reconnect, Some(lightair_model::Reconnect { retries: 3, delay_s: 7 }));
}
