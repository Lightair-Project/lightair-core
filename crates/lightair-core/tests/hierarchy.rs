//! Layers inside layers: empties, children placed in their parent's space, and the flat list
//! the engine draws.

use lightair_core::{Command, CommandError, Document, Id, Input, Layer, Layout, Project, Scene};

struct Setup {
    doc: Document,
    scene: Id,
    format: Id,
    input: Id,
}

fn setup() -> Setup {
    let project = Project::new("Show");
    let format = project.primary_format().id;
    let mut doc = Document::new(project);
    let input = Input::new("Logo", "image_source", serde_json::json!({}));
    let input_id = input.id;
    doc.apply(&Command::AddInput { input }).unwrap();
    let scene = Scene::new("Main");
    let scene_id = scene.id;
    doc.apply(&Command::AddScene { scene, index: None }).unwrap();
    Setup { doc, scene: scene_id, format, input: input_id }
}

impl Setup {
    fn add(&mut self, layer: Layer, index: Option<usize>) -> Id {
        let id = layer.id;
        self.doc.apply(&Command::AddLayer { scene: self.scene, layer, index }).unwrap();
        id
    }
    fn picture(&mut self, name: &str, at: (f32, f32, f32, f32), parent: Option<Id>) -> Id {
        let mut l = Layer::new(name, self.input, [(self.format, Layout::boxed(at.0, at.1, at.2, at.3))]);
        l.parent = parent;
        self.add(l, None)
    }
    fn empty(&mut self, name: &str, at: (f32, f32), parent: Option<Id>) -> Id {
        let mut l = Layer::empty(name, [(self.format, at)]);
        l.parent = parent;
        self.add(l, None)
    }
    fn scene(&self) -> &Scene {
        self.doc.project().scene(self.scene).unwrap()
    }
    fn names(&self) -> Vec<String> {
        self.scene().layers.iter().map(|l| l.name.clone()).collect()
    }
    fn world(&self, id: Id) -> Layout {
        self.scene().world(id, self.format).unwrap()
    }
    fn set(&mut self, id: Id, f: impl FnOnce(&mut Layout)) {
        let mut layout = self.scene().layer(id).unwrap().layouts[&self.format];
        f(&mut layout);
        let (scene, format) = (self.scene, self.format);
        self.doc.apply(&Command::SetLayout { scene, layer: id, format, layout }).unwrap();
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

#[test]
fn children_sit_after_their_parent_and_follow_it() {
    let mut s = setup();
    let back = s.picture("Back", (0.0, 0.0, 100.0, 100.0), None);
    let group = s.empty("Group", (500.0, 300.0), None);
    let a = s.picture("A", (-50.0, -25.0, 100.0, 50.0), Some(group));
    let front = s.picture("Front", (0.0, 0.0, 10.0, 10.0), None);
    let b = s.picture("B", (100.0, 0.0, 20.0, 20.0), Some(group));
    // B joins the group in front of A, before Front (the group's run stays together).
    assert_eq!(s.names(), ["Back", "Group", "A", "B", "Front"]);
    assert!(s.scene().is_well_formed());
    assert_eq!(s.scene().subtree(group), Some(1..4));
    assert_eq!(s.scene().children(Some(group)).map(|l| l.id).collect::<Vec<_>>(), [a, b]);
    let _ = (back, front);

    // A's box is centred on the group's pivot.
    let w = s.world(a);
    assert!(close(w.x, 450.0) && close(w.y, 275.0) && close(w.width, 100.0), "{w:?}");

    // Moving, turning and scaling the group carries A along.
    s.set(group, |l| {
        l.x = 600.0;
        l.rotation = 90.0;
        l.scale_x = 2.0;
        l.scale_y = 2.0;
    });
    let w = s.world(a);
    // Centre stays on the pivot (A's centre is the pivot), size doubles, turned 90°.
    assert!(close(w.x + w.width / 2.0, 600.0) && close(w.y + w.height / 2.0, 300.0), "{w:?}");
    assert!(close(w.width, 200.0) && close(w.height, 100.0) && close(w.rotation, 90.0), "{w:?}");
    // B's centre, (110, 10) from the pivot in the group's space: doubled and turned 90°
    // clockwise, (-20, 220).
    let w = s.world(b);
    assert!(close(w.x + w.width / 2.0, 580.0) && close(w.y + w.height / 2.0, 520.0), "{w:?}");
}

#[test]
fn reparenting_keeps_the_picture_in_place() {
    let mut s = setup();
    let group = s.empty("Group", (400.0, 400.0), None);
    s.set(group, |l| {
        l.rotation = 30.0;
        l.scale_x = 1.5;
        l.scale_y = 1.5;
    });
    let logo = s.picture("Logo", (100.0, 50.0, 200.0, 100.0), None);
    let before = s.world(logo);
    let (scene, format) = (s.scene, s.format);
    s.doc.apply(&Command::ReparentLayer { scene, layer: logo, parent: Some(group), index: None }).unwrap();
    assert_eq!(s.scene().layer(logo).unwrap().parent, Some(group));
    let inside = s.world(logo);
    assert!(close(inside.x, before.x) && close(inside.y, before.y), "{before:?} → {inside:?}");
    assert!(close(inside.width, before.width) && close(inside.rotation, before.rotation));
    // Stored in the group's space: unturned by it and unscaled.
    let local = s.scene().layer(logo).unwrap().layouts[&format];
    assert!(close(local.rotation, -30.0) && close(local.width, 200.0 / 1.5), "{local:?}");

    // And back out.
    s.doc.apply(&Command::ReparentLayer { scene, layer: logo, parent: None, index: None }).unwrap();
    let out = s.world(logo);
    assert!(close(out.x, before.x) && close(out.y, before.y) && close(out.rotation, 0.0), "{out:?}");
    assert!(s.scene().is_well_formed());
}

#[test]
fn a_layer_cant_go_inside_itself() {
    let mut s = setup();
    let outer = s.empty("Outer", (0.0, 0.0), None);
    let inner = s.empty("Inner", (0.0, 0.0), Some(outer));
    let (scene, _) = (s.scene, s.format);
    for (layer, parent) in [(outer, inner), (outer, outer)] {
        let err = s.doc.apply(&Command::ReparentLayer { scene, layer, parent: Some(parent), index: None });
        assert!(matches!(err, Err(CommandError::BadHierarchy)), "{err:?}");
    }
}

#[test]
fn moves_and_removals_take_the_whole_subtree() {
    let mut s = setup();
    let back = s.picture("Back", (0.0, 0.0, 10.0, 10.0), None);
    let group = s.empty("Group", (0.0, 0.0), None);
    s.picture("A", (0.0, 0.0, 10.0, 10.0), Some(group));
    let inner = s.empty("Inner", (0.0, 0.0), Some(group));
    s.picture("C", (0.0, 0.0, 10.0, 10.0), Some(inner));
    s.picture("Front", (0.0, 0.0, 10.0, 10.0), None);
    assert_eq!(s.names(), ["Back", "Group", "A", "Inner", "C", "Front"]);

    // The group to the back of the top level.
    let scene = s.scene;
    s.doc.apply(&Command::MoveLayer { scene, layer: group, to: 0 }).unwrap();
    assert_eq!(s.names(), ["Group", "A", "Inner", "C", "Back", "Front"]);
    // Inner behind A, inside the group.
    s.doc.apply(&Command::MoveLayer { scene, layer: inner, to: 0 }).unwrap();
    assert_eq!(s.names(), ["Group", "Inner", "C", "A", "Back", "Front"]);
    assert!(s.scene().is_well_formed());
    // Into Back (a picture can hold layers too), then removing the group takes its subtree.
    s.doc.apply(&Command::ReparentLayer { scene, layer: inner, parent: Some(back), index: None }).unwrap();
    assert_eq!(s.names(), ["Group", "A", "Back", "Inner", "C", "Front"]);
    s.doc.apply(&Command::RemoveLayer { scene, layer: group }).unwrap();
    assert_eq!(s.names(), ["Back", "Inner", "C", "Front"]);
    s.doc.apply(&Command::RemoveLayer { scene, layer: back }).unwrap();
    assert_eq!(s.names(), ["Front"]);
}

#[test]
fn flattening_gives_the_pictures_in_world_terms() {
    let mut s = setup();
    let group = s.empty("Group", (100.0, 100.0), None);
    let a = s.picture("A", (0.0, 0.0, 50.0, 50.0), Some(group));
    let b = s.picture("B", (10.0, 10.0, 50.0, 50.0), None);
    let flat = s.scene().flatten();
    assert_eq!(flat.layers.iter().map(|l| l.id).collect::<Vec<_>>(), [a, b], "empties aren't drawn");
    assert!(flat.layers.iter().all(|l| l.parent.is_none()));
    let fa = flat.layers[0].layouts[&s.format];
    assert!(close(fa.x, 100.0) && close(fa.y, 100.0), "{fa:?}");

    // A hidden or faded parent hides or fades its children; a lock passes down too.
    s.set(group, |l| l.opacity = 0.5);
    s.set(a, |l| l.opacity = 0.5);
    let fa = s.scene().flatten().layers[0].layouts[&s.format];
    assert!(close(fa.opacity, 0.25) && fa.visible);
    s.set(group, |l| l.visible = false);
    assert!(!s.scene().flatten().layers[0].layouts[&s.format].visible);
    let scene = s.scene;
    s.doc.apply(&Command::SetLayerLocked { scene, layer: group, locked: true }).unwrap();
    assert!(s.scene().flatten().layers[0].locked);

    // A scene without nesting flattens to itself.
    let mut flat_only = setup();
    flat_only.picture("Only", (0.0, 0.0, 10.0, 10.0), None);
    assert_eq!(flat_only.scene().flatten(), *flat_only.scene());
}

#[test]
fn older_projects_still_load() {
    // Before the hierarchy: the input was always there, no parents, no scale.
    let json = r#"{"id":"0190a8f0-0000-7000-8000-000000000001","name":"Cam",
        "input":"0190a8f0-0000-7000-8000-000000000002",
        "layouts":{"0190a8f0-0000-7000-8000-000000000003":{"x":0,"y":0,"width":10,"height":10,
          "rotation":0,"crop":{"left":0,"top":0,"right":0,"bottom":0},"fit":"Fit","visible":true,
          "opacity":1}},"locked":false}"#;
    let layer: Layer = serde_json::from_str(json).unwrap();
    assert!(layer.input.is_some() && layer.parent.is_none());
    let l = layer.layouts.values().next().unwrap();
    assert!(close(l.scale_x, 1.0) && close(l.scale_y, 1.0));
}
