//! The scene renderer: a lightair-model scene drawn as the desktop draws it. Needs a GPU.

use std::collections::HashMap;
use std::sync::Arc;

use lightair_model::{
    Canvas, Crop, Direction, Effect, Fit, Id, Layout, Object, Scene, Shape, ShapeKind, Source, TransitionKind,
    TransitionSettings, Workspace,
};
use lightair_render::scene::{Pictures, SceneRenderer, Shot, unsupported};
use serde_json::json;

const W: u32 = 320;
const H: u32 = 180;

/// Pictures by source name: "red", "blue", "split" (red left, blue right), each 100×100.
struct Test;

fn solid(rgb: [u8; 3]) -> Vec<u8> {
    [rgb[0], rgb[1], rgb[2], 255].repeat(100 * 100)
}

impl Pictures for Test {
    fn picture(&mut self, source: &Source) -> Option<Shot> {
        let px = match source.name.as_str() {
            "red" => solid([255, 0, 0]),
            "blue" => solid([0, 0, 255]),
            "white" => solid([255, 255, 255]),
            "split" => {
                (0..100 * 100).flat_map(|i| if i % 100 < 50 { [255, 0, 0, 255] } else { [0, 0, 255, 255] }).collect()
            }
            _ => return None,
        };
        Some(Shot::rgba(px, (100, 100)))
    }
}

struct Setup {
    ws: Workspace,
    canvas: Id,
    sources: HashMap<&'static str, Id>,
}

impl Setup {
    fn new() -> Setup {
        let mut ws = Workspace::new("Test");
        ws.canvases =
            vec![Canvas { id: Id::new(), name: "Small".into(), width: W, height: H, fps_num: 30, fps_den: 1 }];
        let canvas = ws.canvases[0].id;
        let mut sources = HashMap::new();
        for name in ["red", "blue", "white", "split", "gone"] {
            let s = Source {
                id: Id::new(),
                name: name.into(),
                kind: "image_source".into(),
                settings: json!({}),
                effects: vec![],
            };
            sources.insert(name, s.id);
            ws.sources.insert(s.id, Arc::new(s));
        }
        Setup { ws, canvas, sources }
    }

    fn object(&self, source: &str, layout: Layout) -> Object {
        Object::new(source, self.sources[source], [(self.canvas, layout)])
    }

    fn scene(&mut self, objects: Vec<Object>) -> Id {
        let mut s = Scene::new("Scene");
        s.objects = objects;
        let id = s.id;
        self.ws.scenes.push(Arc::new(s));
        id
    }

    fn effect(&mut self, source: &str, kind: &str, settings: serde_json::Value) {
        let id = self.sources[source];
        let mut s = (*self.ws.sources[&id]).clone();
        s.effects.push(Effect::new(kind, kind, settings));
        self.ws.sources.insert(id, Arc::new(s));
    }
}

fn renderer() -> Option<SceneRenderer> {
    match SceneRenderer::new((W, H)) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("no GPU here ({e}); skipping");
            None
        }
    }
}

fn px(img: &[u8], x: u32, y: u32) -> [i32; 3] {
    let i = ((y * W + x) * 4) as usize;
    [img[i] as i32, img[i + 1] as i32, img[i + 2] as i32]
}

fn is(img: &[u8], x: u32, y: u32, rgb: [i32; 3]) -> bool {
    let p = px(img, x, y);
    (0..3).all(|i| (p[i] - rgb[i]).abs() <= 24)
}

const RED: [i32; 3] = [255, 0, 0];
const BLUE: [i32; 3] = [0, 0, 255];
const BLACK: [i32; 3] = [0, 0, 0];

#[test]
fn objects_are_fitted_cropped_flipped_and_turned() {
    let Some(mut r) = renderer() else { return };
    let mut s = Setup::new();
    // fit: a square picture in a 200×100 box at the left: 100×100 in the box's middle
    let fit = s.object("red", Layout::boxed(0.0, 0.0, 200.0, 100.0));
    let scene = s.scene(vec![fit]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 100, 50, RED) && is(&f, 20, 50, BLACK) && is(&f, 100, 150, BLACK));

    // stretch and crop: the right half of "split" cut away, the red left half fills the box
    let mut l = Layout::boxed(0.0, 0.0, 160.0, 90.0);
    l.fit = Fit::Stretch;
    l.crop = Crop { left: 0, top: 0, right: 50, bottom: 0 };
    let scene = s.scene(vec![s.object("split", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 10, 45, RED) && is(&f, 150, 45, RED), "{:?}", px(&f, 150, 45));

    // flipped: blue on the left
    let mut l = Layout::boxed(0.0, 0.0, 160.0, 90.0);
    l.fit = Fit::Stretch;
    l.flip_h = true;
    let scene = s.scene(vec![s.object("split", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 10, 45, BLUE) && is(&f, 150, 45, RED));

    // fill: covers the box, the overflow cut evenly (the split's middle shows)
    let mut l = Layout::boxed(0.0, 0.0, 300.0, 50.0);
    l.fit = Fit::Fill;
    let scene = s.scene(vec![s.object("split", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 5, 25, RED) && is(&f, 295, 25, BLUE) && is(&f, 150, 100, BLACK));

    // turned a quarter: a wide box stands up
    let mut l = Layout::boxed(110.0, 80.0, 100.0, 20.0);
    l.fit = Fit::Stretch;
    l.rotation = 90.0;
    let scene = s.scene(vec![s.object("red", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 160, 50, RED) && is(&f, 160, 130, RED) && is(&f, 200, 90, BLACK));

    // half see-through (blended in linear light, as libobs: half of white is 188)
    let mut l = Layout::boxed(0.0, 0.0, W as f32, H as f32);
    l.fit = Fit::Stretch;
    l.opacity = 0.5;
    let scene = s.scene(vec![s.object("white", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 160, 90, [188, 188, 188]), "{:?}", px(&f, 160, 90));

    // a source with no picture isn't drawn
    let scene = s.scene(vec![s.object("gone", Layout::boxed(0.0, 0.0, W as f32, H as f32))]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 160, 90, BLACK));
}

#[test]
fn a_child_sits_in_its_parents_space() {
    let Some(mut r) = renderer() else { return };
    let mut s = Setup::new();
    // an empty at (200, 90), scaled 2×: its child's 20×20 box, its centre 10 px right of the
    // empty, lands 40×40 with its centre at (220, 90)
    let mut parent = Object::new("holder", Id::new(), [(s.canvas, Layout::boxed(200.0, 90.0, 0.0, 0.0))]);
    parent.source = None;
    parent.layouts.get_mut(&s.canvas).unwrap().scale_x = 2.0;
    parent.layouts.get_mut(&s.canvas).unwrap().scale_y = 2.0;
    let mut l = Layout::boxed(0.0, -10.0, 20.0, 20.0);
    l.fit = Fit::Stretch;
    let mut child = s.object("red", l);
    child.parent = Some(parent.id);
    let scene = s.scene(vec![parent, child]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 220, 90, RED) && is(&f, 204, 74, RED) && is(&f, 236, 106, RED));
    assert!(is(&f, 197, 90, BLACK) && is(&f, 243, 90, BLACK) && is(&f, 220, 67, BLACK));
}

#[test]
fn shapes_cut_out_and_tilts_recede() {
    let Some(mut r) = renderer() else { return };
    let mut s = Setup::new();
    // a circle: the corners gone, the middle and the edge midpoints there
    let mut l = Layout::boxed(110.0, 40.0, 100.0, 100.0);
    l.fit = Fit::Stretch;
    l.shape = Some(Shape::new(ShapeKind::Circle));
    let scene = s.scene(vec![s.object("red", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 160, 90, RED) && is(&f, 160, 44, RED) && is(&f, 114, 90, RED));
    assert!(is(&f, 114, 44, BLACK) && is(&f, 205, 135, BLACK));

    // a border: the outermost pixels in its colour (0xAABBGGRR: opaque blue)
    let mut shape = Shape::new(ShapeKind::Rounded);
    shape.radius = 0.0;
    shape.border = 10.0;
    shape.border_color = 0xFFFF0000;
    l.shape = Some(shape);
    let scene = s.scene(vec![s.object("red", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 112, 90, BLUE) && is(&f, 160, 90, RED), "{:?} {:?}", px(&f, 112, 90), px(&f, 160, 90));

    // tilted around the vertical axis: the left side recedes (shorter), the right comes near
    let mut l = Layout::boxed(60.0, 40.0, 200.0, 100.0);
    l.fit = Fit::Stretch;
    l.tilt_y = 40.0;
    let scene = s.scene(vec![s.object("red", l)]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    let tall = |x: u32| (0..H).filter(|&y| is(&f, x, y, RED)).count();
    let (left, right) = (tall(100), tall(240));
    assert!(left > 30 && right > left + 10, "left {left}, right {right}");
    // what grows past the box is cut off
    assert!(right <= 101, "{right}");
}

#[test]
fn effects_are_drawn_and_the_rest_listed() {
    let Some(mut r) = renderer() else { return };
    let mut s = Setup::new();
    s.effect("red", "fx.invert", json!({}));
    s.effect("blue", "color_filter_v2", json!({"brightness": 0.0, "opacity": 0.5}));
    s.effect("white", "chroma_key_filter_v2", json!({}));
    s.effect("white", "audio.gain", json!({}));
    let mut a = Layout::boxed(0.0, 0.0, 160.0, H as f32);
    a.fit = Fit::Stretch;
    let mut b = a;
    b.x = 160.0;
    let scene =
        s.scene(vec![s.object("red", a), s.object("blue", b), s.object("white", Layout::boxed(0.0, 0.0, 1.0, 1.0))]);
    let f = r.frame(&s.ws, scene, s.canvas, &mut Test, 0.0);
    assert!(is(&f, 80, 90, [0, 255, 255]), "inverted: {:?}", px(&f, 80, 90));
    assert!(is(&f, 240, 90, [0, 0, 188]), "half see-through: {:?}", px(&f, 240, 90));
    let left = unsupported(&s.ws, scene);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].effect, "chroma_key_filter_v2");
}

#[test]
fn transitions_between_two_scenes() {
    let Some(mut r) = renderer() else { return };
    let mut s = Setup::new();
    let full = || {
        let mut l = Layout::boxed(0.0, 0.0, W as f32, H as f32);
        l.fit = Fit::Stretch;
        l
    };
    let a = s.scene(vec![s.object("red", full())]);
    let b = s.scene(vec![s.object("blue", full())]);
    let t = |kind, direction| TransitionSettings { kind, duration_ms: 500, direction };
    let mut at = |kind, direction, p: f32| r.transition(&s.ws, a, b, s.canvas, &t(kind, direction), p, &mut Test, 0.0);

    assert!(is(&at(TransitionKind::Cut, Direction::Left, 0.5), 160, 90, BLUE));
    assert!(is(&at(TransitionKind::Fade, Direction::Left, 0.5), 160, 90, [188, 0, 188]));
    assert!(is(&at(TransitionKind::Fade, Direction::Left, 1.0), 160, 90, BLUE));
    let dip = at(TransitionKind::Dip, Direction::Left, 0.5);
    assert!(is(&dip, 160, 90, BLACK));
    assert!(is(&at(TransitionKind::Dip, Direction::Left, 0.25), 160, 90, [188, 0, 0]));
    // halfway (eased: half): the old scene in the left half, the new one in the right
    let slide = at(TransitionKind::Slide, Direction::Left, 0.5);
    assert!(is(&slide, 80, 90, RED) && is(&slide, 240, 90, BLUE));
    let wipe = at(TransitionKind::Wipe, Direction::Left, 0.25);
    assert!(is(&wipe, 160, 90, RED) && is(&wipe, 300, 90, BLUE));
    let wipe = at(TransitionKind::Wipe, Direction::Down, 0.5);
    assert!(is(&wipe, 160, 30, BLUE) && is(&wipe, 160, 150, RED));

    // Move: the same picture glides between its two places
    let mut s = Setup::new();
    let box_at = |x: f32| {
        let mut l = Layout::boxed(x, 70.0, 40.0, 40.0);
        l.fit = Fit::Stretch;
        l
    };
    let a = s.scene(vec![s.object("red", box_at(0.0))]);
    let b = s.scene(vec![s.object("red", box_at(280.0))]);
    let f = r.transition(&s.ws, a, b, s.canvas, &t(TransitionKind::Move, Direction::Left), 0.5, &mut Test, 0.0);
    assert!(is(&f, 160, 90, RED) && is(&f, 20, 90, BLACK) && is(&f, 300, 90, BLACK));
}
