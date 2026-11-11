//! OBS scene-collection import against a synthetic collection.

use lightair_core::{Command, Document, Fit, Project, TransitionKind};
use lightair_store::import_obs_collection;

const FIXTURE: &str = include_str!("fixtures/obs-collection.json");

fn imported() -> (Project, lightair_store::ImportReport) {
    let mut doc = Document::new(Project::new("Mine"));
    let (cmd, report) = import_obs_collection(FIXTURE, doc.project()).unwrap();
    assert!(matches!(&cmd, Command::Batch { label, .. } if label == "Import Test Show"));
    doc.apply(&cmd).expect("batch applies");
    (doc.project().as_ref().clone(), report)
}

#[test]
fn scenes_inputs_and_order() {
    let (p, _) = imported();
    let names: Vec<_> = p.scenes.iter().map(|s| s.name.as_str()).collect();
    assert!(names.ends_with(&["Live", "Break"]), "{names:?}");
    let live = p.scenes.iter().find(|s| s.name == "Live").unwrap();
    assert_eq!(p.program, Some(live.id), "program is OBS's program scene");
    assert_eq!(live.layers.len(), 3);
    let cam = p.inputs.values().find(|i| i.name == "Camera").unwrap();
    assert_eq!(cam.kind, "macos-avcapture");
    assert_eq!(cam.settings["device"], "cam-1");
    let bg = p.inputs.values().find(|i| i.name == "Backdrop").unwrap();
    assert_eq!(bg.kind, "color_source_v3", "versioned id wins");
    // The nested scene item in Break is skipped.
    assert_eq!(p.scenes.iter().find(|s| s.name == "Break").unwrap().layers.len(), 1);
}

#[test]
fn geometry() {
    let (p, _) = imported();
    let f = p.primary_format().id;
    let live = p.scenes.iter().find(|s| s.name == "Live").unwrap();
    let bg = &live.layers[0].layouts[&f];
    // Centre-aligned at (960,540), 1920x1080 at half scale.
    assert_eq!((bg.x, bg.y, bg.width, bg.height), (480.0, 270.0, 960.0, 540.0));
    assert_eq!((bg.crop.left, bg.crop.bottom), (10, 20));
    // Bottom-right aligned bounds box ending at (1900,1060).
    let cam = &live.layers[1].layouts[&f];
    assert_eq!((cam.x, cam.y, cam.width, cam.height), (1260.0, 700.0, 640.0, 360.0));
    assert_eq!(cam.fit, Fit::Fit);
    assert!(!cam.visible);
    assert!(p.scenes.iter().find(|s| s.name == "Break").unwrap().layers[0].locked);
}

#[test]
fn effects_audio_and_transition() {
    let (p, report) = imported();
    let cam = p.inputs.values().find(|i| i.name == "Camera").unwrap();
    let kinds: Vec<_> = cam.effects.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["lightair_camera_studio", "chroma_key_filter_v2", "shaderfilter"]);
    assert_eq!(cam.effects[0].name, "Camera Studio");
    assert_eq!(cam.effects[0].settings["smooth"], 0.4, "Dewy settings carry over");
    assert!(!cam.effects[1].enabled);

    let desk = p.tracks.iter().find(|t| t.name == "Desktop Audio").unwrap();
    assert!((desk.volume_db + 6.0206).abs() < 0.01, "{}", desk.volume_db);
    assert!(desk.muted);
    let mic = p.tracks.iter().find(|t| t.name == "Mic/Aux").unwrap();
    assert!(mic.cue);
    let mic_input = p.inputs.values().find(|i| i.id == mic.input).unwrap();
    assert_eq!(mic_input.effects[0].kind, "noise_suppress_filter_v2");

    assert_eq!(p.transition.kind, TransitionKind::Fade);
    assert_eq!(p.transition.duration_ms, 450);

    eprintln!("{report:#?}");
    assert_eq!(report.replaced.len(), 1, "Dewy → Camera Studio");
    assert!(report.kept_plugin.iter().any(|s| s.contains("spectralizer")));
    assert!(report.kept_plugin.iter().any(|s| s.contains("shaderfilter")));
    assert!(report.attention.iter().any(|s| s.contains("Background removal")));
    assert!(report.attention.iter().any(|s| s.contains("nested")));
    assert!(report.attention.iter().any(|s| s.contains("Viz") && s.contains("estimated")));
}

#[test]
fn names_stay_unique_and_import_is_one_undo_step() {
    let mut doc = Document::new(Project::new("Mine"));
    for _ in 0..2 {
        let (cmd, _) = import_obs_collection(FIXTURE, doc.project()).unwrap();
        doc.apply(&cmd).unwrap();
    }
    let names: Vec<_> = doc.project().scenes.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"Live (2)".to_string()), "{names:?}");
    let before = doc.project().scenes.len();
    doc.undo();
    assert_eq!(doc.project().scenes.len(), before - 2);
}

#[test]
fn rejects_garbage() {
    assert!(import_obs_collection("{}", &Project::new("x")).is_err());
    assert!(import_obs_collection("nope", &Project::new("x")).is_err());
}
