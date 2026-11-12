//! Copy, paste and duplicate: whole subtrees, new ids, content copied and devices shared, and
//! the copy lands where the original is.

use lightair_core::{Clip, Command, Document, Id, Input, Layer, Layout, Project, Scene};

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

struct Setup {
    doc: Document,
    scene: Id,
    format: Id,
    camera: Id,
    text: Id,
}

fn setup() -> Setup {
    let project = Project::new("Show");
    let format = project.primary_format().id;
    let mut doc = Document::new(project);
    let camera = Input::new("Camera", "av_capture_input_v2", serde_json::json!({}));
    let text = Input::new("Title", "text_ft2_source_v2", serde_json::json!({"text": "Hello"}));
    let (camera_id, text_id) = (camera.id, text.id);
    doc.apply(&Command::AddInput { input: camera }).unwrap();
    doc.apply(&Command::AddInput { input: text }).unwrap();
    let scene = Scene::new("Main");
    let scene_id = scene.id;
    doc.apply(&Command::AddScene { scene, index: None }).unwrap();
    Setup { doc, scene: scene_id, format, camera: camera_id, text: text_id }
}

impl Setup {
    fn add(&mut self, layer: Layer) -> Id {
        let id = layer.id;
        self.doc.apply(&Command::AddLayer { scene: self.scene, layer, index: None }).unwrap();
        id
    }
    fn names(&self) -> Vec<String> {
        self.doc.project().scene(self.scene).unwrap().layers.iter().map(|l| l.name.clone()).collect()
    }
    fn paste(&mut self, clip: &Clip, parent: Option<Id>, index: Option<usize>) -> Vec<Id> {
        let (commands, roots) = clip.paste(self.doc.project(), self.scene, parent, index);
        self.doc.apply(&Command::Batch { label: "Paste".into(), commands }).unwrap();
        roots
    }
}

#[test]
fn a_copied_group_pastes_whole_with_new_ids() {
    let mut s = setup();
    let f = s.format;
    let mut group = Layer::empty("Lower third", [(f, (300.0, 900.0))]);
    group.layouts.get_mut(&f).unwrap().rotation = 10.0;
    let gid = s.add(group);
    s.add(Layer::new("Cam", s.camera, [(f, Layout::boxed(-100.0, -50.0, 200.0, 100.0))]).under(gid));
    s.add(Layer::new("Name", s.text, [(f, Layout::boxed(120.0, -20.0, 300.0, 40.0))]).under(gid));
    s.add(Layer::new("Front", s.text, [(f, Layout::boxed(0.0, 0.0, 10.0, 10.0))]));

    let clip = Clip::copy(s.doc.project(), s.scene, &[gid]).unwrap();
    assert_eq!(clip.layers.len(), 3, "the group with what it holds");
    // Round trip through clipboard text.
    let clip = Clip::from_text(&clip.to_text()).unwrap();
    assert!(Clip::from_text("just some text").is_none());

    // Duplicate: in front of the original, among its siblings.
    let roots = s.paste(&clip, None, Some(1));
    assert_eq!(s.names(), ["Lower third", "Cam", "Name", "Lower third", "Cam", "Name", "Front"]);
    let sc = s.doc.project().scene(s.scene).unwrap();
    assert!(sc.is_well_formed());
    let copy = roots[0];
    assert_ne!(copy, gid);
    let kids: Vec<&Layer> = sc.children(Some(copy)).collect();
    assert_eq!(kids.len(), 2);
    // In place: same pivot, and its children land where the originals are.
    let (a, b) = (sc.world(gid, f).unwrap(), sc.world(copy, f).unwrap());
    assert!(close(a.x, b.x) && close(a.y, b.y) && close(a.rotation, b.rotation));
    let orig_name = sc.children(Some(gid)).nth(1).unwrap().id;
    let (a, b) = (sc.world(orig_name, f).unwrap(), sc.world(kids[1].id, f).unwrap());
    assert!(close(a.x, b.x) && close(a.y, b.y), "{a:?} vs {b:?}");
    // The camera is shared; the text is its own copy.
    assert_eq!(kids[0].input, Some(s.camera));
    assert_ne!(kids[1].input, Some(s.text));
    assert_eq!(s.doc.project().inputs.len(), 3);
}

#[test]
fn pasting_into_a_parent_keeps_the_picture_in_place() {
    let mut s = setup();
    let f = s.format;
    let logo = s.add(Layer::new("Logo", s.text, [(f, Layout::boxed(100.0, 100.0, 50.0, 50.0))]));
    let mut box_ = Layer::empty("Box", [(f, (500.0, 500.0))]);
    box_.layouts.get_mut(&f).unwrap().scale_x = 2.0;
    box_.layouts.get_mut(&f).unwrap().scale_y = 2.0;
    let bid = s.add(box_);
    let clip = Clip::copy(s.doc.project(), s.scene, &[logo]).unwrap();
    let roots = s.paste(&clip, Some(bid), None);
    let sc = s.doc.project().scene(s.scene).unwrap();
    let w = sc.world(roots[0], f).unwrap();
    assert!(close(w.x, 100.0) && close(w.y, 100.0) && close(w.width, 50.0), "{w:?}");
    assert_eq!(sc.layer(roots[0]).unwrap().parent, Some(bid));
}

#[test]
fn pasting_into_another_project_brings_the_inputs() {
    let mut s = setup();
    let f = s.format;
    let cam = s.add(Layer::new("Cam", s.camera, [(f, Layout::boxed(0.0, 0.0, 640.0, 360.0))]));
    let clip = Clip::copy(s.doc.project(), s.scene, &[cam]).unwrap();

    let mut other = setup();
    // A different project: its camera is another input, so this one comes along.
    let roots = other.paste(&clip, None, None);
    let p = other.doc.project();
    let layer = p.scene(other.scene).unwrap().layer(roots[0]).unwrap();
    let input = layer.input.and_then(|i| p.inputs.get(&i)).expect("input brought along");
    assert_eq!(input.kind, "av_capture_input_v2");
    // Its format ids differ: the layout lands in the primary format.
    assert!(layer.layouts.contains_key(&p.primary_format().id));
}
