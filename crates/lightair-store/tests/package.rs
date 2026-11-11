use std::io::Write;

use lightair_core::{Command, Input, Layer, Layout, Project, Scene};
use lightair_store::{Package, StoreError, load_snapshot, save};
use serde_json::json;

fn sample() -> (Project, Vec<Command>) {
    let project = Project::new("Show");
    let format = project.primary_format().id;
    let cam = Input::new("Camera", "av_capture_input_v2", json!({"device": "abc"}));
    let scene = Scene::new("Main");
    let layer = Layer::new("Camera", cam.id, [(format, Layout::boxed(0.0, 0.0, 1920.0, 1080.0))]);
    let cmds = [Command::AddInput { input: cam }, Command::AddLayer { scene: scene.id, layer, index: None }];
    let mut cmds_full = vec![Command::AddScene { scene, index: None }];
    cmds_full.splice(0..0, cmds[..1].to_vec());
    cmds_full.push(cmds[1].clone());
    (project, cmds_full)
}

#[test]
fn save_and_load_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (mut project, cmds) = sample();
    for c in &cmds {
        c.apply(&mut project).unwrap();
    }
    save(dir.path(), &project).unwrap();
    assert_eq!(load_snapshot(dir.path()).unwrap(), project);
    assert!(dir.path().join("scenes").read_dir().unwrap().count() == 1);
}

#[test]
fn journal_replays_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let (initial, cmds) = sample();
    let mut expected = initial.clone();
    {
        let mut opened = Package::open(dir.path(), || initial.clone()).unwrap();
        for c in &cmds {
            c.apply(&mut opened.project).unwrap();
            c.apply(&mut expected).unwrap();
            opened.package.record(c).unwrap();
        }
        opened.package.flush().unwrap();
        // Simulate a crash mid-write: a torn final line.
        let mut log = std::fs::OpenOptions::new().append(true).open(dir.path().join("journal/current.log")).unwrap();
        log.write_all(b"{\"RenameScene\":{\"scene\"").unwrap();
        // No compaction: the process "dies" here.
    }
    let reopened = Package::open(dir.path(), || unreachable!("package exists")).unwrap();
    assert_eq!(reopened.recovered, 3);
    assert_eq!(reopened.project, expected);
    // Recovery compacts, so a second open replays nothing.
    drop(reopened);
    let again = Package::open(dir.path(), || unreachable!()).unwrap();
    assert_eq!(again.recovered, 0);
    assert_eq!(again.project, expected);
}

#[test]
fn removed_scenes_lose_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let (mut project, cmds) = sample();
    for c in &cmds {
        c.apply(&mut project).unwrap();
    }
    save(dir.path(), &project).unwrap();
    let id = project.scenes[0].id;
    Command::RemoveScene { scene: id }.apply(&mut project).unwrap();
    save(dir.path(), &project).unwrap();
    assert_eq!(dir.path().join("scenes").read_dir().unwrap().count(), 0);
}

#[test]
fn newer_formats_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (project, _) = sample();
    save(dir.path(), &project).unwrap();
    let path = dir.path().join("project.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["formatVersion"] = 99.into();
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(matches!(load_snapshot(dir.path()), Err(StoreError::TooNew { found: 99 })));
}
