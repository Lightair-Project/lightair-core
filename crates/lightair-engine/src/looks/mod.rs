//! The camera looks, the same as the desktop's, drawn by the live engine, so a project
//! looks the same on both. The host runs the models and pushes raw faces and the multiclass mask
//! (`feeds::FacePush`, `feeds::Segmentation`); this module tracks and smooths them ([`face`],
//! [`masks`], [`pigment`]) and draws the looks' passes ([`render`]).
//!
//! The types here mirror the desktop's face tracker's.

pub mod face;
pub mod grade;
pub mod layout;
pub mod masks;
pub mod mesh;
pub mod pigment;
pub mod render;

/// Most points one face can carry.
pub const MAX_POINTS: usize = 720;
/// Points in the MediaPipe face mesh.
pub const MESH_POINTS: usize = 468;

/// The face's regions, in the order their points are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Region {
    Contour,
    LeftEye,
    RightEye,
    LeftBrow,
    RightBrow,
    Nose,
    NoseCrest,
    Median,
    OuterLips,
    InnerLips,
    LeftPupil,
    RightPupil,
    Mesh,
}

pub const REGION_COUNT: usize = 13;

/// One face for the frame being drawn, smoothed,
/// region-ordered (632 points with the mesh), normalised 0–1 in the upright picture, y down.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    /// A face was seen at some point.
    pub valid: bool,
    /// A face was found in the last 350 ms.
    pub present: bool,
    /// The analysed frame's time (host clock) these points belong to (or were interpolated to).
    pub pts_ns: i64,
    pub region_start: [usize; REGION_COUNT],
    pub region_count: [usize; REGION_COUNT],
    pub pts: Vec<[f32; 2]>,
    /// Mean cheek chroma (BT.601 full range, 0–1), when known.
    pub skin_cbcr: Option<[f32; 2]>,
}

impl Face {
    pub fn region(&self, r: Region) -> &[[f32; 2]] {
        let (s, n) = (self.region_start[r as usize], self.region_count[r as usize]);
        self.pts.get(s..s + n).unwrap_or(&[])
    }

    /// An eye's anchor point: eye 0 the subject's left (`Region::LeftEye`), 1 the right.
    pub fn eye_anchor(&self, eye: usize) -> [f32; 2] {
        let mesh = self.region(Region::Mesh);
        if mesh.len() >= MESH_POINTS {
            let (a, b) = if eye == 1 {
                (mesh[33], mesh[133])
            } else {
                (mesh[263], mesh[362])
            };
            return [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5];
        }
        let pts = self.region(if eye == 1 {
            Region::RightEye
        } else {
            Region::LeftEye
        });
        if pts.is_empty() {
            return [0.0, 0.0];
        }
        let n = pts.len() as f32;
        let s = pts
            .iter()
            .fold([0.0, 0.0], |a, p| [a[0] + p[0], a[1] + p[1]]);
        [s[0] / n, s[1] / n]
    }
}

/// The skin pigment basis in optical density,
/// melanin and hemoglobin unit directions and the inverse of [mel hem (1,1,1)/√3] (row-major;
/// row 0 gives melanin, row 1 hemoglobin). Physical priors until calibrated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pigment {
    pub calibrated: bool,
    pub mel: [f32; 3],
    pub hem: [f32; 3],
    pub inv: [f32; 9],
}

/// A segmentation mask: RGBA bytes, its size, its serial.
pub type SegMask = (std::sync::Arc<Vec<u8>>, (u32, u32), u64);

/// The masks for the frame being drawn, temporally smoothed.
#[derive(Debug, Clone)]
pub struct Masks {
    /// RGBA 8-bit, R skin, G face skin, B hair, A person, whole picture upright; and its serial
    /// (changes when it's a new one, so it's uploaded only then).
    pub seg: Option<SegMask>,
}

// ------------------------------------------------------------------- the Looks screen's status

/// What the looks see of a camera's face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceState {
    /// A face look draws the camera but no face analysis has come from the host yet.
    Warming,
    Found,
    /// Analysed, and no face in the picture now.
    None,
    /// No face look is drawing this camera (or it isn't on the program).
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaceStatus {
    pub state: FaceState,
    /// x, y, w, h, 0–1 of the camera's picture as drawn, while a face is found.
    pub face_box: Option<[f32; 4]>,
    /// The phone is hot enough for the governor to lighten costly effects (its step 3).
    pub lighter: bool,
}

/// The governor's step from which costly effects are lightened (step 3, Serious).
pub const LIGHTER_STEP: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    Ok,
    TooSlow,
}

/// Which looks a phone can draw now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LookSupport {
    pub skin: Support,
    pub shape: Support,
    pub makeup: Support,
    pub light: Support,
    pub grade: Support,
}

impl LookSupport {
    /// By the phone's class and the governor's step. The mesh-driven parts (Shape's warp,
    /// Makeup's paint) need the face mesh drawn every frame, too much for a class C phone;
    /// from the governor's step 3 (Serious) every face part is too slow, and Light and the
    /// Colour grade (one pass each) stay.
    pub fn of(class: crate::device::Class, governor_step: u32) -> LookSupport {
        use Support::*;
        let hot = governor_step >= LIGHTER_STEP;
        let small = class == crate::device::Class::C;
        let face = if hot { TooSlow } else { Ok };
        let mesh = if hot || small { TooSlow } else { Ok };
        LookSupport {
            skin: face,
            shape: mesh,
            makeup: mesh,
            light: Ok,
            grade: Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Class;

    /// The Looks screen offers what the phone can draw: a hot phone none of the face parts, a
    /// small one not the mesh-driven parts; Light and the Colour grade always.
    #[test]
    fn looks_a_phone_cant_keep_up_with_are_too_slow() {
        let cool = LookSupport::of(Class::A, 0);
        assert!(
            [cool.skin, cool.shape, cool.makeup, cool.light, cool.grade]
                .iter()
                .all(|s| *s == Support::Ok)
        );
        let small = LookSupport::of(Class::C, 0);
        assert_eq!(
            (small.skin, small.shape, small.makeup),
            (Support::Ok, Support::TooSlow, Support::TooSlow)
        );
        let hot = LookSupport::of(Class::A, LIGHTER_STEP);
        assert_eq!(
            (hot.skin, hot.shape, hot.makeup),
            (Support::TooSlow, Support::TooSlow, Support::TooSlow)
        );
        assert_eq!(
            (hot.light, hot.grade),
            (Support::Ok, Support::Ok),
            "one pass each: they stay"
        );
    }
}
