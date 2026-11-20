//! A Studio studio_take's scene (`scene.json` in the studio_take's folder): every object of the program scene
//! as it was when the studio_take began, in drawing order (bottom first), so the Editor can rebuild it:
//! the recorded screens and cameras where they were, and what wasn't recorded (images, text,
//! colours, video files) as objects of their own. Images are copied into the studio_take's `objects`
//! folder, so a studio_take carries what it needs.

use std::path::{Path, PathBuf};

use lightair_model::{Id, Workspace, kinds};
use serde::{Deserialize, Serialize};

pub const FILE: &str = "scene.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioTakeScene {
    /// The canvas's size (pixels).
    pub canvas: (u32, u32),
    #[serde(alias = "layers")]
    pub objects: Vec<SceneObject>,
}

/// A object: where it was (fractions of the canvas: its box's top-left and size), and what it
/// showed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneObject {
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Degrees clockwise.
    #[serde(default)]
    pub rotation: f32,
    #[serde(default = "one")]
    pub opacity: f32,
    pub what: What,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum What {
    /// A recorded screen or camera (its source: studio_take.json's video entry of that source).
    Recorded {
        #[serde(alias = "input")]
        source: Id,
    },
    /// A picture copied into the take (relative to its folder).
    Image { file: String },
    /// A video file, where it is.
    Video { path: PathBuf },
    /// Text: its words, size (canvas pixels), colour (0xRRGGBB) and weight.
    Text { text: String, size: f32, color: u32, bold: bool },
    /// A solid colour (0xRRGGBB and its opacity).
    Colour { color: u32, alpha: f32 },
    /// Something the Editor can't rebuild (a browser, a timer…): named, left out.
    Other { kind: String },
}

/// OBS's 0xAABBGGRR as 0xRRGGBB and an opacity.
fn obs_colour(c: u64) -> (u32, f32) {
    let c = c as u32;
    ((c & 0xFF) << 16 | (c & 0xFF00) | ((c >> 16) & 0xFF), ((c >> 24) & 0xFF) as f32 / 255.0)
}

impl StudioTakeScene {
    /// The program scene of `workspace` as it is, its pictures copied into `folder` (the studio_take's).
    pub fn of(workspace: &Workspace, folder: &Path) -> Option<StudioTakeScene> {
        let scene = workspace.program.and_then(|p| workspace.scene(p))?.flatten();
        let canvas = workspace.primary_canvas();
        let (cw, ch) = (canvas.width.max(1) as f32, canvas.height.max(1) as f32);
        let mut objects = Vec::new();
        for (n, object) in scene.objects.iter().enumerate() {
            let Some(layout) = object.layouts.get(&canvas.id).filter(|l| l.visible) else { continue };
            let Some(source) = object.source.and_then(|i| workspace.sources.get(&i)) else { continue };
            let s = &source.settings;
            let what = match source.kind.as_str() {
                kinds::SCREEN | kinds::CAMERA => What::Recorded { source: source.id },
                kinds::MIC | kinds::DESKTOP_AUDIO | kinds::APP_AUDIO => continue,
                "image_source" => {
                    let Some(src) = s["file"].as_str().map(PathBuf::from).filter(|p| p.is_file()) else { continue };
                    let name = format!("{n}-{}", src.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "image".into()));
                    let rel = format!("layers/{name}");
                    let copied = std::fs::create_dir_all(folder.join("layers")).and_then(|_| std::fs::copy(&src, folder.join(&rel)));
                    if copied.is_err() {
                        continue;
                    }
                    What::Image { file: rel }
                }
                "ffmpeg_source" => match s["local_file"].as_str() {
                    Some(p) => What::Video { path: PathBuf::from(p) },
                    None => What::Other { kind: source.kind.clone() },
                },
                k if k.starts_with("text_") => {
                    let (color, _) = obs_colour(s["color1"].as_u64().unwrap_or(0xFFFF_FFFF));
                    What::Text {
                        text: s["text"].as_str().unwrap_or("").to_string(),
                        size: s["font"]["size"].as_f64().unwrap_or(64.0) as f32,
                        color,
                        bold: s["font"]["flags"].as_u64().is_some_and(|f| f & 1 != 0),
                    }
                }
                k if k.starts_with("color_source") => {
                    let (color, alpha) = obs_colour(s["color"].as_u64().unwrap_or(0xFFFF_FFFF));
                    What::Colour { color, alpha }
                }
                k => What::Other { kind: k.to_string() },
            };
            objects.push(SceneObject {
                name: object.name.clone(),
                x: layout.x / cw,
                y: layout.y / ch,
                w: layout.width / cw,
                h: layout.height / ch,
                rotation: layout.rotation,
                opacity: layout.opacity,
                what,
            });
        }
        Some(StudioTakeScene { canvas: (canvas.width, canvas.height), objects })
    }

    pub fn read(folder: &Path) -> Option<StudioTakeScene> {
        serde_json::from_slice(&std::fs::read(folder.join(FILE)).ok()?).ok()
    }

    pub fn write(&self, folder: &Path) -> std::io::Result<()> {
        std::fs::write(folder.join(FILE), serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obs_colours_read_as_rgb() {
        assert_eq!(obs_colour(0xFF0000FF), (0xFF0000, 1.0));
        assert_eq!(obs_colour(0x80FF0000).0, 0x0000FF);
    }

    #[test]
    fn a_scene_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = StudioTakeScene {
            canvas: (1920, 1080),
            objects: vec![SceneObject { name: "Logo".into(), x: 0.1, y: 0.1, w: 0.2, h: 0.1, rotation: 0.0, opacity: 1.0, what: What::Text { text: "Hi".into(), size: 64.0, color: 0xFFFFFF, bold: true } }],
        };
        s.write(dir.path()).unwrap();
        assert_eq!(StudioTakeScene::read(dir.path()), Some(s));
    }
}
