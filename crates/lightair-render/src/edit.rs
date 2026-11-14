//! The edit document (`edit.json`): what the Editor changes about a take or recording. Times are
//! in milliseconds of the source (the take), not of the result: cuts and speed decide the result
//! (`timeline`).

use serde::{Deserialize, Serialize};

/// A kept stretch of the source, played at `speed`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub start_ms: u64,
    pub end_ms: u64,
    #[serde(default = "one")]
    pub speed: f32,
}

fn one() -> f32 {
    1.0
}

/// Where a zoom looks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Focus {
    /// Follows the pointer.
    Pointer,
    /// A fixed point, as fractions of the screen (0..1).
    Point { x: f32, y: f32 },
}

/// A zoom: from `start_ms` to `end_ms` (source time) the screen is magnified `scale` times
/// around `focus`; easing in and out is the zoom engine's.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Zoom {
    pub start_ms: u64,
    pub end_ms: u64,
    pub scale: f32,
    pub focus: Focus,
    /// Made by the automatic zoom (re-made when it runs again); `false` once the user edits it.
    #[serde(default)]
    pub auto: bool,
}

/// How the pointer is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pointer {
    /// 0 pointer, 1 dot, 2 ring (as the live effect).
    pub style: i64,
    pub size: f32,
    /// 0 exact .. 1 a slow glide.
    pub smoothing: f32,
    pub hidden: bool,
    /// Clicks: 0 none, 1 circle, 2 ripple.
    pub clicks: i64,
}

impl Default for Pointer {
    fn default() -> Self {
        Pointer { style: 0, size: 1.5, smoothing: 0.5, hidden: false, clicks: 2 }
    }
}

/// The frame around the screen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Look {
    /// A backdrop gradient (`lightair_engine::backdrop::PRESETS`).
    pub backdrop: i64,
    /// Space around the screen, as a fraction of the result's height.
    pub padding: f32,
    /// Corner radius, as a fraction of the screen's short side.
    pub radius: f32,
    pub shadow: f32,
}

impl Default for Look {
    fn default() -> Self {
        Look { backdrop: 0, padding: 0.07, radius: 0.015, shadow: 0.5 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub version: u32,
    /// The kept stretches, in order. Empty: the whole source.
    #[serde(default)]
    pub clips: Vec<Clip>,
    #[serde(default)]
    pub zooms: Vec<Zoom>,
    #[serde(default)]
    pub pointer: Pointer,
    #[serde(default)]
    pub look: Look,
    /// The result's size (width, height).
    pub size: (u32, u32),
}

impl Edit {
    /// A new edit of a source `duration_ms` long: all of it, at 1080p, automatic zooms to come.
    pub fn new(duration_ms: u64) -> Edit {
        Edit {
            version: 1,
            clips: vec![Clip { start_ms: 0, end_ms: duration_ms, speed: 1.0 }],
            zooms: Vec::new(),
            pointer: Pointer::default(),
            look: Look::default(),
            size: (1920, 1080),
        }
    }

    pub fn load(path: &std::path::Path) -> std::io::Result<Edit> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path)
    }

    /// Cuts the source at `at_ms` (source time): the clip there becomes two.
    pub fn split(&mut self, at_ms: u64) {
        if let Some(i) = self.clips.iter().position(|c| c.start_ms < at_ms && at_ms < c.end_ms) {
            let c = self.clips[i];
            self.clips[i].end_ms = at_ms;
            self.clips.insert(i + 1, Clip { start_ms: at_ms, ..c });
        }
    }

    /// Removes the clip holding `at_ms` (source time).
    pub fn remove_clip_at(&mut self, at_ms: u64) {
        self.clips.retain(|c| !(c.start_ms <= at_ms && at_ms < c.end_ms));
    }
}
