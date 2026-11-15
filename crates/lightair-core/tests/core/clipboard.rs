//! Copy, paste and duplicate: whole subtrees, new ids, content copied and devices shared, and
//! the copy lands where the original is.

use lightair_core::{Clip, Command, Document, Id, Source, Object, Layout, Workspace, Scene};

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

struct Setup {
    doc: Document,
    scene: Id,
    canvas: Id,
    camera: Id,
    text: Id,
}

fn setup() -> Setup {
    let workspace = Workspace::new("Show");
    let canvas = workspace.primary_canvas().id;
    let mut doc = Document::new(workspace);
    let camera = Source::new("Camera", lightair_core::kinds::CAMERA, serde_json::json!({}));
    let text = Source::new("Title", "text_ft2_source_v2", serde_json::json!({"text": "Hello"}));
    let (camera_id, text_id) = (camera.id, text.id);
    doc.apply(&Command::AddSource { source: camera }).unwrap();
    doc.apply(&Command::AddSource { source: text }).unwrap();
    let scene = Scene::new("Main");
    let scene_id = scene.id;
    doc.apply(&Command::AddScene { scene, index: None }).unwrap();
    Setup { doc, scene: scene_id, canvas, camera: camera_id, text: text_id }
}

impl Setup {
    fn add(&mut self, object: Object) -> Id {
        let id = object.id;
        self.doc.apply(&Command::AddObject { scene: self.scene, object, index: None }).unwrap();
        id
    }
    fn names(&self) -> Vec<String> {
        self.doc.workspace().scene(self.scene).unwrap().objects.iter().map(|l| l.name.clone()).collect()
    }
    fn paste(&mut self, clip: &Clip, parent: Option<Id>, index: Option<usize>) -> Vec<Id> {
        let (commands, roots) = clip.paste(self.doc.workspace(), self.scene, parent, index);
        self.doc.apply(&Command::Batch { label: "Paste".into(), commands }).unwrap();
        roots
    }
}

#[test]
fn a_copied_group_pastes_whole_with_new_ids() {
    let mut s = setup();
    let f = s.canvas;
    let mut group = Object::empty("Lower third", [(f, (300.0, 900.0))]);
    group.layouts.get_mut(&f).unwrap().rotation = 10.0;
    let gid = s.add(group);
    s.add(Object::new("Cam", s.camera, [(f, Layout::boxed(-100.0, -50.0, 200.0, 100.0))]).under(gid));
    s.add(Object::new("Name", s.text, [(f, Layout::boxed(120.0, -20.0, 300.0, 40.0))]).under(gid));
    s.add(Object::new("Front", s.text, [(f, Layout::boxed(0.0, 0.0, 10.0, 10.0))]));

    let clip = Clip::copy(s.doc.workspace(), s.scene, &[gid]).unwrap();
    assert_eq!(clip.objects.len(), 3, "the group with what it holds");
    // Round trip through clipboard text.
    let clip = Clip::from_text(&clip.to_text()).unwrap();
    assert!(Clip::from_text("just some text").is_none());

    // Duplicate: in front of the original, among its siblings.
    let roots = s.paste(&clip, None, Some(1));
    assert_eq!(s.names(), ["Lower third", "Cam", "Name", "Lower third", "Cam", "Name", "Front"]);
    let sc = s.doc.workspace().scene(s.scene).unwrap();
    assert!(sc.is_well_formed());
    let copy = roots[0];
    assert_ne!(copy, gid);
    let kids: Vec<&Object> = sc.children(Some(copy)).collect();
    assert_eq!(kids.len(), 2);
    // In place: same pivot, and its children land where the originals are.
    let (a, b) = (sc.world(gid, f).unwrap(), sc.world(copy, f).unwrap());
    assert!(close(a.x, b.x) && close(a.y, b.y) && close(a.rotation, b.rotation));
    let orig_name = sc.children(Some(gid)).nth(1).unwrap().id;
    let (a, b) = (sc.world(orig_name, f).unwrap(), sc.world(kids[1].id, f).unwrap());
    assert!(close(a.x, b.x) && close(a.y, b.y), "{a:?} vs {b:?}");
    // The camera is shared; the text is its own copy.
    assert_eq!(kids[0].source, Some(s.camera));
    assert_ne!(kids[1].source, Some(s.text));
    assert_eq!(s.doc.workspace().sources.len(), 3);
}

#[test]
fn pasting_into_a_parent_keeps_the_picture_in_place() {
    let mut s = setup();
    let f = s.canvas;
    let logo = s.add(Object::new("Logo", s.text, [(f, Layout::boxed(100.0, 100.0, 50.0, 50.0))]));
    let mut box_ = Object::empty("Box", [(f, (500.0, 500.0))]);
    box_.layouts.get_mut(&f).unwrap().scale_x = 2.0;
    box_.layouts.get_mut(&f).unwrap().scale_y = 2.0;
    let bid = s.add(box_);
    let clip = Clip::copy(s.doc.workspace(), s.scene, &[logo]).unwrap();
    let roots = s.paste(&clip, Some(bid), None);
    let sc = s.doc.workspace().scene(s.scene).unwrap();
    let w = sc.world(roots[0], f).unwrap();
    assert!(close(w.x, 100.0) && close(w.y, 100.0) && close(w.width, 50.0), "{w:?}");
    assert_eq!(sc.object(roots[0]).unwrap().parent, Some(bid));
}

#[test]
fn pasting_into_another_workspace_brings_the_sources() {
    let mut s = setup();
    let f = s.canvas;
    let cam = s.add(Object::new("Cam", s.camera, [(f, Layout::boxed(0.0, 0.0, 640.0, 360.0))]));
    let clip = Clip::copy(s.doc.workspace(), s.scene, &[cam]).unwrap();

    let mut other = setup();
    // A different workspace: its camera is another source, so this one comes along.
    let roots = other.paste(&clip, None, None);
    let p = other.doc.workspace();
    let object = p.scene(other.scene).unwrap().object(roots[0]).unwrap();
    let source = object.source.and_then(|i| p.sources.get(&i)).expect("input brought along");
    assert_eq!(source.kind, lightair_core::kinds::CAMERA);
    // Its canvas ids differ: the layout lands in the primary canvas.
    assert!(object.layouts.contains_key(&p.primary_canvas().id));
}

#[test]
fn prefab_instances_link_and_rebuild_in_place() {
    let mut s = setup();
    let f = s.canvas;
    let gid = s.add(Object::empty("Badge", [(f, (100.0, 100.0))]));
    s.add(Object::new("Label", s.text, [(f, Layout::boxed(0.0, 0.0, 200.0, 40.0))]).under(gid));
    // The prefab's contents, as saved.
    let mut prefab = Clip::copy(s.doc.workspace(), s.scene, &[gid]).unwrap();
    prefab.objects[0].prefab = None;
    // its own id, in its file: what instances link by
    let prefab_id = lightair_core::Id::new();
    prefab.prefab_id = Some(prefab_id);

    // Place an instance elsewhere: its root is linked.
    let (commands, root) = prefab.instantiate(s.doc.workspace(), s.scene, "Badge.prefab", None, None);
    s.doc.apply(&Command::Batch { label: "Place".into(), commands }).unwrap();
    let root = root.unwrap();
    let sc = s.doc.workspace().scene(s.scene).unwrap();
    assert_eq!(sc.object(root).unwrap().prefab.as_deref(), Some("Badge.prefab"));
    assert_eq!(sc.object(root).unwrap().prefab_id, Some(prefab_id), "linked by the prefab's id");

    // Move and rename the instance, then change the prefab (a second label) and rebuild it.
    let mut layout = sc.object(root).unwrap().layouts[&f];
    layout.x = 700.0;
    let scene = s.scene;
    s.doc.apply(&Command::SetLayout { scene, object: root, canvas: f, layout }).unwrap();
    s.doc.apply(&Command::RenameObject { scene, object: root, name: "Guest badge".into() }).unwrap();
    let mut changed = prefab.clone();
    let mut second = changed.objects[1].clone();
    second.id = Id::new();
    second.name = "Subtitle".into();
    changed.objects.push(second);
    let (commands, rebuilt) = changed.replace_instance(s.doc.workspace(), scene, root);
    s.doc.apply(&Command::Batch { label: "Revert".into(), commands }).unwrap();
    let rebuilt = rebuilt.unwrap();
    let sc = s.doc.workspace().scene(scene).unwrap();
    assert!(sc.object(root).is_none(), "the old instance is gone");
    let r = sc.object(rebuilt).unwrap();
    assert_eq!(r.name, "Guest badge", "keeps its name");
    assert_eq!(r.prefab.as_deref(), Some("Badge.prefab"), "still linked");
    assert_eq!(r.prefab_id, Some(prefab_id), "still linked by its id");
    assert!(close(sc.world(rebuilt, f).unwrap().x, 700.0), "keeps its place");
    let kids: Vec<String> = sc.children(Some(rebuilt)).map(|l| l.name.clone()).collect();
    assert_eq!(kids, ["Label", "Subtitle"], "takes the prefab's contents");
    assert!(sc.is_well_formed());
    // The original it was saved from isn't an instance and is untouched.
    assert!(sc.object(gid).unwrap().prefab.is_none());
}

#[test]
fn a_scene_saved_as_an_asset_comes_back_as_a_new_scene() {
    use lightair_core::SceneAsset;
    let mut s = setup();
    let f = s.canvas;
    let cam = s.add(Object::new("Camera", s.camera, [(f, Layout::boxed(0.0, 0.0, 640.0, 360.0))]));
    let group = s.add(Object::empty("Lower third", [(f, (300.0, 900.0))]));
    s.add(Object::new("Title", s.text, [(f, Layout::boxed(10.0, 10.0, 300.0, 40.0))]).under(group));
    let before = s.names();

    // saved to a file and read back
    let asset = SceneAsset::of(s.doc.workspace(), s.scene).unwrap();
    let text = serde_json::to_string(&asset).unwrap();
    let asset: SceneAsset = serde_json::from_str(&text).unwrap();
    assert_eq!(asset.name, "Main");

    let (commands, id) = asset.add(s.doc.workspace(), None);
    s.doc.apply(&Command::Batch { label: "Add scene".into(), commands }).unwrap();
    let p = s.doc.workspace();
    let added = p.scene(id).expect("a new scene");
    assert_eq!(added.name, "Main");
    let names: Vec<String> = added.objects.iter().map(|l| l.name.clone()).collect();
    assert_eq!(names, before, "the same layers, in the same order");
    assert!(added.objects.iter().all(|l| l.id != cam && l.id != group), "new layers");
    let new_group = added.objects.iter().find(|l| l.name == "Lower third").unwrap().id;
    assert_eq!(added.objects.iter().find(|l| l.name == "Title").unwrap().parent, Some(new_group), "nesting kept");
    // the camera is shared, the text is its own copy
    assert_eq!(added.objects.iter().find(|l| l.name == "Camera").unwrap().source, Some(s.camera));
    assert_ne!(added.objects.iter().find(|l| l.name == "Title").unwrap().source, Some(s.text));
}
