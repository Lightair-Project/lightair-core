use std::sync::Arc;

use lightair_core::{
    Command, CommandError, Document, Input, Layer, LayerPatch, Layout, Patch, Project, Scene, diff, diff_layers,
};
use proptest::prelude::*;

fn setup() -> (Document, lightair_core::Id, lightair_core::Id, lightair_core::Id) {
    let project = Project::new("Show");
    let format = project.primary_format().id;
    let mut doc = Document::new(project);
    let cam = Input::new("Camera", "av_capture_input_v2", serde_json::json!({}));
    let cam_id = cam.id;
    doc.apply(&Command::AddInput { input: cam }).unwrap();
    let scene = Scene::new("Main");
    let scene_id = scene.id;
    doc.apply(&Command::AddScene { scene, index: None }).unwrap();
    let layer = Layer::new("Camera", cam_id, [(format, Layout::boxed(0.0, 0.0, 1920.0, 1080.0))]);
    let layer_id = layer.id;
    doc.apply(&Command::AddLayer { scene: scene_id, layer, index: None }).unwrap();
    (doc, scene_id, layer_id, format)
}

#[test]
fn first_scene_goes_on_air() {
    let (doc, scene, ..) = setup();
    assert_eq!(doc.project().program, Some(scene));
}

#[test]
fn undo_and_redo_restore_snapshots() {
    let (mut doc, scene, layer, format) = setup();
    let before = doc.project().clone();
    doc.apply(&Command::SetLayout { scene, layer, format, layout: Layout::boxed(10.0, 20.0, 640.0, 360.0) }).unwrap();
    assert_eq!(doc.undo_label(), Some("Move layer"));
    doc.undo().unwrap();
    assert_eq!(**doc.project(), *before);
    doc.redo().unwrap();
    assert_eq!(doc.project().scene(scene).unwrap().layer(layer).unwrap().layouts[&format].x, 10.0);
}

#[test]
fn a_drag_gesture_is_one_undo_step() {
    let (mut doc, scene, layer, format) = setup();
    let steps = doc_undo_depth(&doc);
    doc.begin_gesture();
    for x in 0..50 {
        doc.apply(&Command::SetLayout { scene, layer, format, layout: Layout::boxed(x as f32, 0.0, 100.0, 100.0) })
            .unwrap();
    }
    doc.end_gesture();
    assert_eq!(doc_undo_depth(&doc), steps + 1);
    doc.undo().unwrap();
    assert_eq!(doc.project().scene(scene).unwrap().layer(layer).unwrap().layouts[&format].x, 0.0);
    // The next drag is a separate step.
    doc.begin_gesture();
    doc.apply(&Command::SetLayout { scene, layer, format, layout: Layout::boxed(5.0, 0.0, 100.0, 100.0) }).unwrap();
    doc.end_gesture();
    assert_eq!(doc_undo_depth(&doc), steps + 1);
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
    assert_eq!(doc.project().program, Some(brb_id), "undo must not change what's on air");
}

#[test]
fn failing_commands_change_nothing() {
    let (mut doc, scene, ..) = setup();
    let before = doc.project().clone();
    let ghost = lightair_core::Id::new();
    assert_eq!(doc.apply(&Command::RemoveLayer { scene, layer: ghost }).unwrap_err(), CommandError::NoLayer(ghost));
    let cam = *doc.project().inputs.keys().next().unwrap();
    assert!(matches!(doc.apply(&Command::RemoveInput { input: cam }), Err(CommandError::InputInUse(1))));
    // A batch that fails halfway leaves no trace.
    let batch = Command::Batch {
        label: "Rename and break".into(),
        commands: vec![Command::RenameScene { scene, name: "Renamed".into() }, Command::RemoveScene { scene: ghost }],
    };
    assert!(doc.apply(&batch).is_err());
    assert!(Arc::ptr_eq(doc.project(), &before) || **doc.project() == *before);
}

#[test]
fn unchanged_scenes_share_memory() {
    let (mut doc, scene, ..) = setup();
    let other = Scene::new("Other");
    let other_id = other.id;
    doc.apply(&Command::AddScene { scene: other, index: None }).unwrap();
    let before = doc.project().clone();
    doc.apply(&Command::RenameScene { scene, name: "Main 2".into() }).unwrap();
    let after = doc.project();
    assert!(Arc::ptr_eq(before.scene(other_id).unwrap(), after.scene(other_id).unwrap()));
}

#[test]
fn diff_orders_and_detects_changes() {
    let (doc, scene, layer, format) = setup();
    let empty = doc.project().empty_like();
    let patches = diff(&empty, doc.project());
    let kinds: Vec<&str> = patches
        .iter()
        .map(|p| match p {
            Patch::InputAdded(_) => "input+",
            Patch::SceneAdded(_) => "scene+",
            Patch::ProgramChanged(_) => "program",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["input+", "scene+", "program"]);

    let mut doc2 = doc.clone();
    doc2.apply(&Command::SetLayout { scene, layer, format, layout: Layout::boxed(1.0, 2.0, 3.0, 4.0) }).unwrap();
    let patches = diff(doc.project(), doc2.project());
    let Patch::SceneChanged { before, after } = &patches[0] else { panic!("{patches:?}") };
    assert!(matches!(diff_layers(before, after)[..], [LayerPatch::Changed { .. }]));
    assert_eq!(patches.len(), 1);
}

#[test]
fn layer_reorder_is_one_order_patch() {
    let (mut doc, scene, _layer, format) = setup();
    let cam = *doc.project().inputs.keys().next().unwrap();
    let top = Layer::new("Top", cam, [(format, Layout::boxed(0.0, 0.0, 10.0, 10.0))]);
    let top_id = top.id;
    doc.apply(&Command::AddLayer { scene, layer: top, index: None }).unwrap();
    let before = doc.project().scene(scene).unwrap().clone();
    doc.apply(&Command::MoveLayer { scene, layer: top_id, to: 0 }).unwrap();
    let after = doc.project().scene(scene).unwrap().clone();
    assert!(matches!(diff_layers(&before, &after)[..], [LayerPatch::Order(_)]));
}

#[test]
fn project_round_trips_through_json() {
    let (doc, ..) = setup();
    let json = serde_json::to_string(&**doc.project()).unwrap();
    let back: Project = serde_json::from_str(&json).unwrap();
    assert_eq!(back, **doc.project());
}

// Random command sequences: undoing everything returns to the start, redoing everything
// returns to the end, and diff(start, end) is empty only when they are equal.
#[derive(Debug, Clone)]
enum Op {
    AddScene,
    RenameFirst(u8),
    AddLayerToFirst,
    MoveFirstLayer(f32),
    RemoveLastScene,
    ReorderLayers,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::AddScene),
        any::<u8>().prop_map(Op::RenameFirst),
        Just(Op::AddLayerToFirst),
        (0f32..1000.0).prop_map(Op::MoveFirstLayer),
        Just(Op::RemoveLastScene),
        Just(Op::ReorderLayers),
    ]
}

proptest! {
    #[test]
    fn undo_redo_round_trip(ops in proptest::collection::vec(op(), 1..40)) {
        let (setup_doc, ..) = setup();
        // History starts after setup, so "undo everything" returns exactly to `start`.
        let mut doc = Document::new((**setup_doc.project()).clone());
        let start = doc.project().clone();
        let format = start.primary_format().id;
        let cam = *start.inputs.keys().next().unwrap();
        for op in ops {
            let p = doc.project().clone();
            let cmd = match op {
                Op::AddScene => Some(Command::AddScene { scene: Scene::new("S"), index: None }),
                Op::RenameFirst(n) => p.scenes.first().map(|s| Command::RenameScene { scene: s.id, name: format!("S{n}") }),
                Op::AddLayerToFirst => p.scenes.first().map(|s| Command::AddLayer {
                    scene: s.id, layer: Layer::new("L", cam, [(format, Layout::boxed(0.0, 0.0, 1.0, 1.0))]), index: None }),
                Op::MoveFirstLayer(x) => p.scenes.first().and_then(|s| s.layers.first().map(|l| Command::SetLayout {
                    scene: s.id, layer: l.id, format, layout: Layout::boxed(x, 0.0, 1.0, 1.0) })),
                Op::RemoveLastScene => p.scenes.last().map(|s| Command::RemoveScene { scene: s.id }),
                Op::ReorderLayers => p.scenes.first().and_then(|s| s.layers.last().map(|l| Command::MoveLayer {
                    scene: s.id, layer: l.id, to: 0 })),
            };
            if let Some(cmd) = cmd { let _ = doc.apply(&cmd); }
        }
        let end = doc.project().clone();
        while doc.undo().is_some() {}
        prop_assert_eq!(&doc.project().scenes, &start.scenes);
        prop_assert_eq!(&doc.project().inputs, &start.inputs);
        while doc.redo().is_some() {}
        prop_assert_eq!(&doc.project().scenes, &end.scenes);
        prop_assert_eq!(diff(&end, doc.project()).is_empty(), true);
    }
}
