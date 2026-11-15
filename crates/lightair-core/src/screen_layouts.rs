//! Screen recording scenes (docs/plans/screen-studio.md, "Camera" and "Look and framing"): the
//! workspace's screen on a backdrop with rounded corners and a shadow, and the camera in the
//! layouts Screen Studio has: screen only, screen with the camera in a corner, side by side, and
//! the camera alone. Scenes switch with the Move transition, so the layout animates on air.
//!
//! Each layout has a backdrop of its own: its shadow sits under that layout's screen.

use serde_json::json;

use crate::{Command, Fit, Id, Source, Object, Layout, Workspace, Scene, Shape, ShapeKind, TransitionKind, TransitionSettings};

/// Space around the screen, as a fraction of the canvas's height.
const PADDING: f32 = 0.07;

/// The largest box of aspect `aspect` (w/h) in (x, y, w, h), centred.
fn fit(aspect: f32, x: f32, y: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
    let (bw, bh) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
    (x + (w - bw) / 2.0, y + (h - bh) / 2.0, bw, bh)
}

fn rounded(radius: f32) -> Shape {
    Shape { radius, ..Shape::new(ShapeKind::Rounded) }
}

/// What the scenes are built from.
pub struct Sources {
    pub screen: Id,
    /// The screen's aspect (w/h), so the box and its shadow fit the picture exactly.
    pub screen_aspect: f32,
    pub camera: Option<Id>,
    /// The backdrop's gradient (`lightair_engine::backdrop::PRESETS`).
    pub preset: i64,
}

/// The commands that add the layout scenes (and their backdrops), and Move as the transition.
pub fn scenes(workspace: &Workspace, s: &Sources) -> Vec<Command> {
    let f = workspace.primary_canvas();
    let (fid, w, h) = (f.id, f.width as f32, f.height as f32);
    let pad = h * PADDING;
    let mut commands = Vec::new();
    // every scene's backdrop is the same thing (each its own source, its shadow where its screen
    // is): the Move transition glides between them
    let backdrop_link = Id::new();
    let mut add = |name: &str, screen_box: Option<(f32, f32, f32, f32)>, camera: Option<(Layout, &str)>| {
        let mut scene = Scene::new(name);
        if let Some((x, y, bw, bh)) = screen_box {
            let backdrop = Source::new(
                format!("Backdrop · {name}"),
                "lightair_backdrop",
                json!({
                    "width": f.width, "height": f.height, "preset": s.preset,
                    "shadow": true, "shadow_x": x, "shadow_y": y, "shadow_w": bw, "shadow_h": bh,
                    "shadow_radius": bh.min(bw) * 0.025, "shadow_blur": h * 0.035, "shadow_opacity": 0.5,
                }),
            );
            scene.objects.push(Object { link: Some(backdrop_link), ..Object::new("Backdrop", backdrop.id, [(fid, Layout::boxed(0.0, 0.0, w, h))]) });
            commands.push(Command::AddSource { source: backdrop });
            let mut l = Layout::boxed(x, y, bw, bh);
            l.fit = Fit::Stretch;
            // corners about 1.5% of the short side
            l.shape = Some(rounded(0.05));
            scene.objects.push(Object::new("Screen", s.screen, [(fid, l)]));
        }
        if let (Some(cam), Some((layout, label))) = (s.camera, camera) {
            scene.objects.push(Object::new(label, cam, [(fid, layout)]));
        }
        commands.push(Command::AddScene { scene, index: None });
    };

    // screen only
    let full = fit(s.screen_aspect, pad, pad, w - 2.0 * pad, h - 2.0 * pad);
    add("Screen", Some(full), None);

    if s.camera.is_some() {
        // the camera in the bottom-right corner, over the screen
        let d = h * 0.26;
        let mut bubble = Layout::boxed(w - pad - d, h - pad - d, d, d);
        bubble.fit = Fit::Fill;
        bubble.shape = Some(Shape::new(ShapeKind::Circle));
        add("Screen + camera", Some(full), Some((bubble, "Camera")));

        // side by side: the screen on the left two thirds, the camera beside it
        let split_w = (w - 3.0 * pad) * 0.68;
        let screen = fit(s.screen_aspect, pad, pad, split_w, h - 2.0 * pad);
        let cam_x = screen.0 + screen.2 + pad;
        let mut side = Layout::boxed(cam_x, screen.1, w - pad - cam_x, screen.3);
        side.fit = Fit::Fill;
        side.shape = Some(rounded(0.08));
        add("Split", Some(screen), Some((side, "Camera")));

        // the camera alone
        let mut alone = Layout::boxed(0.0, 0.0, w, h);
        alone.fit = Fit::Fill;
        add("Camera", None, Some((alone, "Camera")));
    }
    // the layout animates between them
    let mut t = workspace.transition;
    if t.kind != TransitionKind::Move {
        t = TransitionSettings { kind: TransitionKind::Move, ..t };
        commands.push(Command::SetTransition { settings: t });
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_fit_the_screen_and_keep_the_shadow_under_it() {
        let mut p = Workspace::new("P");
        let screen = Source::new("Display", crate::kinds::SCREEN, json!({}));
        let cam = Source::new("Camera", crate::kinds::CAMERA, json!({}));
        let (sid, cid) = (screen.id, cam.id);
        for source in [screen, cam] {
            Command::AddSource { source }.apply(&mut p).unwrap();
        }
        let cmds = scenes(&p, &Sources { screen: sid, screen_aspect: 16.0 / 10.0, camera: Some(cid), preset: 0 });
        for c in &cmds {
            c.apply(&mut p).unwrap();
        }
        let names: Vec<_> = p.scenes.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Screen", "Screen + camera", "Split", "Camera"]);
        let f = p.primary_canvas().id;
        for scene in &p.scenes[..3] {
            let screen = scene.objects.iter().find(|l| l.source == Some(sid)).unwrap().layouts[&f];
            assert!((screen.width / screen.height - 1.6).abs() < 0.01, "the box has the screen's shape");
            let backdrop = p.sources.get(&scene.objects[0].source.unwrap()).unwrap();
            assert_eq!(backdrop.kind, "lightair_backdrop");
            assert_eq!(backdrop.settings["shadow_x"].as_f64().unwrap() as f32, screen.x, "the shadow is under it");
            assert_eq!(backdrop.settings["shadow_w"].as_f64().unwrap() as f32, screen.width);
        }
        assert_eq!(p.transition.kind, TransitionKind::Move);
    }
}
