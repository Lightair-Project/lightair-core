//! The face tracker's work on the host's raw faces: region assembly, lid snapping, One-Euro,
//! frame matching, cheek colour, and the renderer's face-weight easer.
//!
//! After inference the tracker snaps the upper lids on the analysis frame, assembles the regions,
//! samples the cheeks, smooths in analysis-frame pixels, normalises and keeps eight results. The
//! host has run the models and pushes the 478 raw points ([`FacePush`]); [`Tracker::push`] does
//! the rest, and [`Tracker::get_at`] picks the face for the frame being drawn. Results are keyed
//! by the analysed frame's `pts_ns`, the frame period inferred from the pushes turning frame
//! counts into times.

use std::collections::VecDeque;
use std::f32::consts::PI;

use super::{Face, MAX_POINTS, MESH_POINTS, REGION_COUNT, Region};
use crate::feeds::{FACE_POINTS, FacePush};

/// How long a face counts as present after the last result that found one.
pub const PRESENT_TIMEOUT_NS: i64 = 350_000_000;
/// Results kept.
pub const HISTORY: usize = 8;
/// The widest gap, in frames, between two results that are interpolated.
pub const MAX_GAP_FRAMES: i64 = 12;
/// How far, in frames, the newest result is extrapolated.
pub const MAX_AHEAD_FRAMES: i64 = 3;
/// The analysis frame's short side: the smoothing's pixel units are those of a picture with
/// this short side.
pub const TRACK_SHORT_SIDE: f32 = 720.0;
/// The frame period assumed until two pushes have shown one (30 fps).
const DEFAULT_PERIOD_NS: i64 = 33_333_333;

/// A grey copy of the analysed frame, short side ≤ 720 px (the analysis frame), upright as the
/// points are, rows y down, no padding. Lid
/// snapping reads `bytes`; the cheek colour reads `cbcr` (skipped when it's empty); the mask's
/// tile motion ([`super::masks`]) reads `bytes`.
#[derive(Debug, Clone, Default)]
pub struct Luma {
    pub width: u32,
    pub height: u32,
    /// BT.601 luma, `0.299 R + 0.587 G + 0.114 B`, one byte a pixel.
    pub bytes: Vec<u8>,
    /// BT.601 full-range Cb, Cr, interleaved, one pair a pixel at the same size; or empty.
    pub cbcr: Vec<u8>,
}

impl Luma {
    /// From an RGBA8 picture (sRGB values, as the analysis frame's), with chroma.
    pub fn from_rgba(rgba: &[u8], width: u32, height: u32) -> Luma {
        let n = width as usize * height as usize;
        let mut bytes = Vec::with_capacity(n);
        let mut cbcr = Vec::with_capacity(n * 2);
        for px in rgba.as_chunks::<4>().0.iter().take(n) {
            let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
            // BT.601 luma and full-range Cb, Cr, in 0–255.
            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            let cb = 127.5 - 0.168736 * r - 0.331264 * g + 0.5 * b;
            let cr = 127.5 + 0.5 * r - 0.418688 * g - 0.081312 * b;
            bytes.push(y.round().clamp(0.0, 255.0) as u8);
            cbcr.push(cb.round().clamp(0.0, 255.0) as u8);
            cbcr.push(cr.round().clamp(0.0, 255.0) as u8);
        }
        Luma { width, height, bytes, cbcr }
    }

    fn usable(&self) -> bool {
        self.width >= 2 && self.height >= 2 && self.bytes.len() >= self.width as usize * self.height as usize
    }

    fn has_cbcr(&self) -> bool {
        self.cbcr.len() >= self.width as usize * self.height as usize * 2
    }

    // Bilinear at pixel-centre coordinates, clamped to the edge pixels, on the one luma channel.
    fn sample(&self, x: f32, y: f32) -> f32 {
        let (w, h) = (self.width as i32, self.height as i32);
        let mut x = x - 0.5;
        let mut y = y - 0.5;
        x = if x < 0.0 {
            0.0
        } else if x > w as f32 - 1.001 {
            w as f32 - 1.001
        } else {
            x
        };
        y = if y < 0.0 {
            0.0
        } else if y > h as f32 - 1.001 {
            h as f32 - 1.001
        } else {
            y
        };
        let (x0, y0) = (x as i32, y as i32);
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let i = y0 as usize * w as usize + x0 as usize;
        let p = |k: usize| self.bytes[k] as f32;
        let (p00, p01, p10, p11) = (p(i), p(i + 1), p(i + w as usize), p(i + w as usize + 1));
        let a = p00 + (p01 - p00) * fx;
        let b = p10 + (p11 - p10) * fx;
        a + (b - a) * fy
    }
}

// MediaPipe mesh indices per region, in `Region` order; `Region::Mesh` is the whole mesh in mesh
// order.
const REGION_TABLE: [&[usize]; REGION_COUNT - 1] = [
    // Contour (jaw line, temple to temple like Vision)
    &[356, 454, 323, 361, 288, 397, 365, 379, 378, 400, 377, 152, 148, 176, 149, 150, 136, 172, 58, 132, 93, 234, 127],
    // LeftEye / RightEye (subject's left = image right)
    &[263, 249, 390, 373, 374, 380, 381, 382, 362, 398, 384, 385, 386, 387, 388, 466],
    &[33, 7, 163, 144, 145, 153, 154, 155, 133, 173, 157, 158, 159, 160, 161, 246],
    // brows
    &[276, 283, 282, 295, 285, 300, 293, 334, 296, 336],
    &[46, 53, 52, 65, 55, 70, 63, 105, 66, 107],
    // Nose (wings + tip + lower bridge), NoseCrest, Median
    &[98, 327, 129, 358, 64, 294, 2, 94, 19, 1, 4, 5, 195],
    &[168, 6, 197, 195, 5, 4],
    &[10, 151, 9, 8, 168, 6, 197, 195, 5, 4, 1, 19, 94, 2, 164, 0, 11, 12, 13, 14, 15, 16, 17, 18, 200, 199, 175, 152],
    // lips (outer, inner incl. mouth corners 78 / 308)
    &[61, 146, 91, 181, 84, 17, 314, 405, 321, 375, 291, 409, 270, 269, 267, 0, 37, 39, 40, 185],
    &[78, 95, 88, 178, 87, 14, 317, 402, 318, 324, 308, 415, 310, 311, 312, 13, 82, 81, 80, 191],
    // iris centres
    &[473],
    &[468],
];

// Region assembly: the 478 points copied into region order (632 points).
pub fn assemble(mesh: &[[f32; 2]]) -> Face {
    let mut f = Face::empty();
    const ALL: [usize; MESH_POINTS] = {
        let mut v = [0; MESH_POINTS];
        let mut i = 0;
        while i < MESH_POINTS {
            v[i] = i;
            i += 1;
        }
        v
    };
    // the whole mesh, in mesh order, after the other regions
    let tables = REGION_TABLE.iter().copied().chain([&ALL[..]]);
    for (r, ids) in tables.enumerate() {
        f.region_start[r] = f.pts.len();
        for &idx in ids {
            if f.pts.len() >= MAX_POINTS {
                break;
            }
            f.pts.push(mesh[idx]);
        }
        f.region_count[r] = f.pts.len() - f.region_start[r];
    }
    f
}

// Lid snapping: each upper lid's movable points slid along the lid's normal to the lash line
// (the strongest bright-above, dark-below step), as one smooth correction; the corners stay.
// `mesh`: the 478 raw points in the luma's pixels.
pub fn snap_lids(mesh: &mut [[f32; 2]], luma: &Luma) {
    const LIDS: [[usize; 9]; 2] =
        [[133, 173, 157, 158, 159, 160, 161, 246, 33], [362, 398, 384, 385, 386, 387, 388, 466, 263]];
    const BROW: [usize; 2] = [105, 334];
    if mesh.len() < MESH_POINTS || !luma.usable() {
        return;
    }
    let lum = |x: f32, y: f32| luma.sample(x, y);
    let d = (mesh[263][0] - mesh[33][0]).hypot(mesh[263][1] - mesh[33][1]) / 1.45; // ~IOD
    if d < 12.0 {
        return; // face too small to see a lash line
    }
    let step = (0.012 * d).max(1.0);
    let half = (0.01 * d).max(1.0);
    for e in 0..2 {
        let (a, b) = (mesh[LIDS[e][0]], mesh[LIDS[e][8]]);
        let (mut tx, mut ty) = (b[0] - a[0], b[1] - a[1]);
        let tl = tx.hypot(ty);
        if tl < 4.0 {
            continue;
        }
        tx /= tl;
        ty /= tl;
        let (mut nx, mut ny) = (-ty, tx); // normal; make it point up, towards the brow
        let br = mesh[BROW[e]];
        if nx * (br[0] - a[0]) + ny * (br[1] - a[1]) < 0.0 {
            nx = -nx;
            ny = -ny;
        }
        let mut off = [0.0f32; 9];
        let mut wgt = [0.0f32; 9];
        for k in 1..8 {
            let p = mesh[LIDS[e][k]];
            let (mut best, mut best_g) = (0.0f32, 0.0f32);
            let mut s = -0.05 * d;
            while s <= 0.035 * d {
                let mut g = 0.0f32;
                for j in -1..=1 {
                    let x = p[0] + tx * j as f32 * half;
                    let y = p[1] + ty * j as f32 * half;
                    g += lum(x + nx * (s + step), y + ny * (s + step)) - lum(x + nx * (s - step), y + ny * (s - step));
                }
                g = g / 3.0 - 0.25 * s.abs(); // prefer the closest strong edge
                if g > best_g {
                    best_g = g;
                    best = s;
                }
                s += 0.25;
            }
            off[k] = best;
            wgt[k] = if best_g > 10.0 { best_g } else { 0.0 };
        }
        // one smooth correction along the lid (a parabola through the corners' zero offset,
        // fitted to the clear edges), so single points can't kink the line
        let (mut sxx, mut sxy) = (0.0f64, 0.0f64);
        for k in 1..8 {
            let u = k as f64 / 8.0;
            let basis = u * (1.0 - u) * 4.0; // 0 at both corners, 1 mid-lid
            sxx += wgt[k] as f64 * basis * basis;
            sxy += wgt[k] as f64 * basis * off[k] as f64;
        }
        if sxx < 30.0 {
            continue; // too few clear edges: leave the lid as tracked
        }
        let amp = sxy / sxx;
        let (mut sw, mut sr) = (0.0f64, 0.0f64); // plus a straight tilt term for the residual
        for k in 1..8 {
            let u = k as f64 / 8.0;
            sw += wgt[k] as f64 * (u - 0.5) * (u - 0.5);
            sr += wgt[k] as f64 * (u - 0.5) * (off[k] as f64 - amp * u * (1.0 - u) * 4.0);
        }
        let tilt = if sw > 1e-6 { sr / sw } else { 0.0 };
        for k in 1..8 {
            let u = k as f64 / 8.0;
            let o = (amp * u * (1.0 - u) * 4.0 + tilt * (u - 0.5) * u * (1.0 - u) * 4.0) as f32;
            mesh[LIDS[e][k]][0] += nx * o;
            mesh[LIDS[e][k]][1] += ny * o;
        }
    }
}

// The mean of one region's points.
fn region_centroid(f: &Face, r: Region) -> [f32; 2] {
    let pts = f.region(r);
    let mut c = [0.0f32, 0.0];
    for p in pts {
        c[0] += p[0];
        c[1] += p[1];
    }
    if !pts.is_empty() {
        c[0] /= pts.len() as f32;
        c[1] /= pts.len() as f32;
    }
    c
}

// The mean of all the face's points.
fn all_centroid(f: &Face) -> [f32; 2] {
    let mut c = [0.0f32, 0.0];
    for p in &f.pts {
        c[0] += p[0];
        c[1] += p[1];
    }
    if !f.pts.is_empty() {
        c[0] /= f.pts.len() as f32;
        c[1] /= f.pts.len() as f32;
    }
    c
}

// Mean Cb, Cr of two cheek patches; `f`'s points in the luma's pixels. Reads the luma and
// chroma planes.
fn sample_skin(f: &Face, luma: &Luma) -> Option<[f32; 2]> {
    if f.region(Region::LeftEye).is_empty()
        || f.region(Region::RightEye).is_empty()
        || f.region(Region::OuterLips).is_empty()
        || !luma.usable()
        || !luma.has_cbcr()
    {
        return None;
    }
    let (w, h) = (luma.width as i32, luma.height as i32);
    let e1 = region_centroid(f, Region::LeftEye);
    let e2 = region_centroid(f, Region::RightEye);
    let m = region_centroid(f, Region::OuterLips);
    let mid = [(e1[0] + e2[0]) * 0.5, (e1[1] + e2[1]) * 0.5];
    let iod = (e2[0] - e1[0]).hypot(e2[1] - e1[1]);
    let half = (iod * 0.06).max(1.0) as i32;
    let (mut sum_cb, mut sum_cr) = (0.0f64, 0.0f64);
    let mut count = 0;
    for e in [e1, e2] {
        let px = (e[0] + (m[0] - mid[0]) * 0.55 + (e[0] - mid[0]) * 0.2) as i32;
        let py = (e[1] + (m[1] - mid[1]) * 0.55 + (e[1] - mid[1]) * 0.2) as i32;
        for y in py - half..=py + half {
            if y < 0 || y >= h {
                continue;
            }
            for x in px - half..=px + half {
                if x < 0 || x >= w {
                    continue;
                }
                let i = y as usize * w as usize + x as usize;
                let l = luma.bytes[i] as f32 / 255.0;
                if !(0.06..=0.97).contains(&l) {
                    continue;
                }
                sum_cb += luma.cbcr[i * 2] as f64 / 255.0;
                sum_cr += luma.cbcr[i * 2 + 1] as f64 / 255.0;
                count += 1;
            }
        }
    }
    if count < 8 {
        return None;
    }
    Some([(sum_cb / count as f64) as f32, (sum_cr / count as f64) as f32])
}

/// One-Euro filter state: the filtered value and its speed.
#[derive(Debug, Clone, Copy, Default)]
struct Euro {
    x: f32,
    dx: f32,
}

/// One-Euro filter settings.
#[derive(Debug, Clone, Copy)]
struct EuroCfg {
    min_cutoff: f32,
    beta: f32,
    /// how quickly the speed estimate follows
    d_cutoff: f32,
}

// Tuned for low jitter at rest and low lag in motion (analysis px, radians, IOD units).
const CFG_CENTER: EuroCfg = EuroCfg { min_cutoff: 1.5, beta: 0.20, d_cutoff: 1.0 };
const CFG_ANGLE: EuroCfg = EuroCfg { min_cutoff: 1.0, beta: 1.0, d_cutoff: 1.0 };
const CFG_SCALE: EuroCfg = EuroCfg { min_cutoff: 0.8, beta: 0.05, d_cutoff: 1.0 };
const CFG_SHAPE: EuroCfg = EuroCfg { min_cutoff: 0.6, beta: 0.35, d_cutoff: 1.0 };
/// Eyelids and lips: a blink closes the lid in ~100 ms and makeup sits right on these edges, so
/// they must follow at once; at rest they're still held at ~1.5 Hz.
const CFG_FEATURE: EuroCfg = EuroCfg { min_cutoff: 1.5, beta: 6.0, d_cutoff: 8.0 };

// The smoothing factor of a low-pass at `cutoff` Hz over `dt` seconds.
fn euro_alpha(cutoff: f32, dt: f32) -> f32 {
    let tau = 1.0 / (2.0 * PI * cutoff);
    1.0 / (1.0 + tau / dt)
}

// One One-Euro step: the cutoff rises with the filtered speed.
fn euro_step(s: &mut Euro, c: EuroCfg, x: f32, dt: f32) -> f32 {
    let dx = (x - s.x) / dt;
    s.dx += euro_alpha(c.d_cutoff, dt) * (dx - s.dx);
    let cutoff = c.min_cutoff + c.beta * s.dx.abs();
    s.x += euro_alpha(cutoff, dt) * (x - s.x);
    s.x
}

// The mesh points smoothed as features: the eyelid outlines, the rings around them and the lip
// outlines.
fn mesh_feature_point(i: usize) -> bool {
    // eyes: lid outlines, then the ring outside each
    const EYES: [usize; 64] = [
        33, 7, 163, 144, 145, 153, 154, 155, 133, 173, 157, 158, 159, 160, 161, 246, 263, 249, 390, 373, 374, 380, 381,
        382, 362, 398, 384, 385, 386, 387, 388, 466, 130, 25, 110, 24, 23, 22, 26, 112, 243, 190, 56, 28, 27, 29, 30,
        247, 359, 255, 339, 254, 253, 252, 256, 341, 463, 414, 286, 258, 257, 259, 260, 467,
    ];
    // lips: outer and inner outlines
    const LIPS: [usize; 40] = [
        61, 146, 91, 181, 84, 17, 314, 405, 321, 375, 291, 409, 270, 269, 267, 0, 37, 39, 40, 185, 78, 95, 88, 178, 87,
        14, 317, 402, 318, 324, 308, 415, 310, 311, 312, 13, 82, 81, 80, 191,
    ];
    i < MESH_POINTS && (EYES.contains(&i) || LIPS.contains(&i))
}

impl Face {
    /// No face: not valid, not present, no points.
    pub fn empty() -> Face {
        Face {
            valid: false,
            present: false,
            pts_ns: 0,
            region_start: [0; REGION_COUNT],
            region_count: [0; REGION_COUNT],
            pts: Vec::new(),
            skin_cbcr: None,
        }
    }
}

/// The tracker's state after inference (its results and smoothing), for one feed.
#[derive(Debug, Clone)]
pub struct Tracker {
    /// Results, oldest first.
    hist: VecDeque<Face>,
    last_seen_ns: Option<i64>,
    e_cx: Euro,
    e_cy: Euro,
    e_ang: Euro,
    e_scale: Euro,
    e_shape: Vec<Euro>,
    e_cbcr: [Euro; 2],
    euro_count: usize,
    euro_ts: Option<i64>,
    skin_valid: bool,
    last_serial: Option<u64>,
    last_push_ns: Option<i64>,
    period_ns: i64,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker {
            hist: VecDeque::with_capacity(HISTORY),
            last_seen_ns: None,
            e_cx: Euro::default(),
            e_cy: Euro::default(),
            e_ang: Euro::default(),
            e_scale: Euro::default(),
            e_shape: vec![Euro::default(); MAX_POINTS * 2],
            e_cbcr: [Euro::default(); 2],
            euro_count: 0,
            euro_ts: None,
            skin_valid: false,
            last_serial: None,
            last_push_ns: None,
            period_ns: DEFAULT_PERIOD_NS,
        }
    }

    /// The serial of the last push taken: push only newer ones.
    pub fn last_serial(&self) -> Option<u64> {
        self.last_serial
    }

    /// The camera's frame period as inferred from the pushes (it turns frame counts into times).
    pub fn period_ns(&self) -> i64 {
        self.period_ns
    }

    // The smallest gap between pushes, rising slowly so a lower frame rate is followed, falling
    // at once.
    fn note_period(&mut self, pts_ns: i64) {
        if let Some(last) = self.last_push_ns {
            let d = pts_ns - last;
            if d > 0 {
                if d < self.period_ns {
                    self.period_ns = d;
                } else {
                    self.period_ns += (d - self.period_ns) / 32;
                }
            }
        }
        self.last_push_ns = Some(pts_ns);
    }

    /// One analysed frame, in push order. `luma` is that frame's grey copy (lid snapping and the
    /// cheek colour are skipped without it). A miss stores nothing. Returns false for a
    /// push already taken (its serial not newer than the last).
    // The post-inference part of landmark tracking, with lid snapping and region assembly.
    pub fn push(&mut self, f: &FacePush, luma: Option<&Luma>) -> bool {
        if let Some(last) = self.last_serial
            && f.serial <= last
        {
            return false;
        }
        self.last_serial = Some(f.serial);
        self.note_period(f.pts_ns);
        if !f.present || f.points.len() < FACE_POINTS {
            return true;
        }
        let luma = luma.filter(|l| l.usable());
        let mut mesh: Vec<[f32; 2]> = f.points[..FACE_POINTS].to_vec();
        if let Some(l) = luma {
            let (lw, lh) = (l.width as f32, l.height as f32);
            for p in mesh.iter_mut() {
                *p = [p[0] * lw, p[1] * lh];
            }
            snap_lids(&mut mesh, l);
            for p in mesh.iter_mut() {
                *p = [p[0] / lw, p[1] / lh];
            }
        }
        let mut face = assemble(&mesh);
        if face.pts.len() <= 10 || face.region(Region::LeftEye).is_empty() || face.region(Region::RightEye).is_empty() {
            return true;
        }
        let skin = luma.and_then(|l| {
            let (lw, lh) = (l.width as f32, l.height as f32);
            let mut px = face.clone();
            for p in px.pts.iter_mut() {
                *p = [p[0] * lw, p[1] * lh];
            }
            sample_skin(&px, l)
        });

        let (uw, uh) = units(f.size, luma);
        for p in face.pts.iter_mut() {
            *p = [p[0] * uw, p[1] * uh];
        }
        self.smooth(&mut face, f.pts_ns);
        for p in face.pts.iter_mut() {
            *p = [p[0] / uw, p[1] / uh];
        }
        // skin colour drifts slowly; heavy smoothing avoids mask flicker
        if let Some(s) = skin {
            if !self.skin_valid {
                self.e_cbcr[0].x = s[0];
                self.e_cbcr[1].x = s[1];
            } else {
                self.e_cbcr[0].x += 0.08 * (s[0] - self.e_cbcr[0].x);
                self.e_cbcr[1].x += 0.08 * (s[1] - self.e_cbcr[1].x);
            }
            self.skin_valid = true;
        }
        face.skin_cbcr = self.skin_valid.then_some([self.e_cbcr[0].x, self.e_cbcr[1].x]);
        face.valid = true;
        face.pts_ns = f.pts_ns;
        if self.hist.len() == HISTORY {
            self.hist.pop_front();
        }
        self.hist.push_back(face);
        // the analysed frame's time, same clock as `get_at`'s `now_ns`
        self.last_seen_ns = Some(f.pts_ns);
        true
    }

    // Rigid (centre, angle, scale) and shape One-Euro, in pixels of a ≤720 short side frame.
    fn smooth(&mut self, f: &mut Face, ts: i64) {
        let n = f.pts.len();
        let mut c = all_centroid(f);
        let (mut e1, mut e2) = (f.eye_anchor(0), f.eye_anchor(1));
        if e1[0] > e2[0] {
            std::mem::swap(&mut e1, &mut e2);
        }
        let mut scale = (e2[0] - e1[0]).hypot(e2[1] - e1[1]).max(1.0);
        let mut ang = (e2[1] - e1[1]).atan2(e2[0] - e1[0]);
        let dt = self.euro_ts.map_or(0.0, |t| ((ts - t) as f64 / 1e9) as f32);
        let reset = self.euro_count != n || dt <= 0.0 || dt > 0.5;

        if reset {
            self.e_cx = Euro { x: c[0], dx: 0.0 };
            self.e_cy = Euro { x: c[1], dx: 0.0 };
            self.e_ang = Euro { x: ang, dx: 0.0 };
            self.e_scale = Euro { x: scale, dx: 0.0 };
        } else {
            while ang - self.e_ang.x > PI {
                ang -= 2.0 * PI;
            }
            while ang - self.e_ang.x < -PI {
                ang += 2.0 * PI;
            }
        }
        let (mut cs, mut sn) = (ang.cos(), ang.sin());
        let mut sx = vec![0.0f32; n];
        let mut sy = vec![0.0f32; n];
        for i in 0..n {
            let (dx, dy) = (f.pts[i][0] - c[0], f.pts[i][1] - c[1]);
            sx[i] = (dx * cs + dy * sn) / scale;
            sy[i] = (-dx * sn + dy * cs) / scale;
        }
        if reset {
            for i in 0..n {
                self.e_shape[i * 2] = Euro { x: sx[i], dx: 0.0 };
                self.e_shape[i * 2 + 1] = Euro { x: sy[i], dx: 0.0 };
            }
        } else {
            c[0] = euro_step(&mut self.e_cx, CFG_CENTER, c[0], dt);
            c[1] = euro_step(&mut self.e_cy, CFG_CENTER, c[1], dt);
            ang = euro_step(&mut self.e_ang, CFG_ANGLE, ang, dt);
            scale = euro_step(&mut self.e_scale, CFG_SCALE, scale, dt);
            let mut feature = vec![false; n];
            for r in [Region::LeftEye, Region::RightEye, Region::OuterLips, Region::InnerLips] {
                let s = f.region_start[r as usize];
                for i in 0..f.region_count[r as usize] {
                    feature[s + i] = true;
                }
            }
            let ms = f.region_start[Region::Mesh as usize];
            for i in 0..f.region_count[Region::Mesh as usize] {
                feature[ms + i] = mesh_feature_point(i);
            }
            for i in 0..n {
                let cfg = if feature[i] { CFG_FEATURE } else { CFG_SHAPE };
                sx[i] = euro_step(&mut self.e_shape[i * 2], cfg, sx[i], dt);
                sy[i] = euro_step(&mut self.e_shape[i * 2 + 1], cfg, sy[i], dt);
            }
            cs = ang.cos();
            sn = ang.sin();
        }
        for i in 0..n {
            f.pts[i][0] = c[0] + (sx[i] * cs - sy[i] * sn) * scale;
            f.pts[i][1] = c[1] + (sx[i] * sn + sy[i] * cs) * scale;
        }
        self.euro_count = n;
        self.euro_ts = Some(ts);
    }

    fn frames(&self, dt_ns: i64) -> i64 {
        (dt_ns as f64 / self.period_ns.max(1) as f64).round() as i64
    }

    /// The face for the frame captured at `pts_ns`, drawn at `now_ns` (same clock): exact when
    /// that frame was analysed, else interpolated between results ≤ 12 frames apart, else
    /// extrapolated ≤ 3 frames past the newest (damped 0.8), else the nearest. `present` when a
    /// face was found in the last 350 ms; `valid` once one ever was.
    // Frame counts turned into times by the inferred period.
    pub fn get_at(&self, pts_ns: i64, now_ns: i64) -> Face {
        let (mut lo, mut hi, mut lo2) = (None, None, None);
        for (i, f) in self.hist.iter().enumerate().rev() {
            // newest first
            if f.pts_ns >= pts_ns {
                hi = Some(i);
            }
            if f.pts_ns <= pts_ns {
                if lo.is_none() {
                    lo = Some(i);
                } else if lo2.is_none() {
                    lo2 = Some(i);
                }
            }
        }
        let h = |i: usize| &self.hist[i];
        let mut out = match (lo, hi, lo2) {
            (Some(l), Some(u), _)
                if l != u
                    && h(l).pts.len() == h(u).pts.len()
                    && self.frames(h(u).pts_ns - h(l).pts_ns) <= MAX_GAP_FRAMES =>
            {
                let k = (pts_ns - h(l).pts_ns) as f64 / (h(u).pts_ns - h(l).pts_ns) as f64;
                lerp_face(h(l), h(u), k as f32)
            }
            (Some(l), None, Some(l2))
                if h(l2).pts.len() == h(l).pts.len()
                    && h(l).pts_ns > h(l2).pts_ns
                    && self.frames(h(l).pts_ns - h(l2).pts_ns) <= MAX_GAP_FRAMES
                    && self.frames(pts_ns - h(l).pts_ns) <= MAX_AHEAD_FRAMES =>
            {
                let k = 0.8 * (pts_ns - h(l).pts_ns) as f64 / (h(l).pts_ns - h(l2).pts_ns) as f64;
                lerp_face(h(l), h(l2), -k as f32)
            }
            (Some(l), _, _) => h(l).clone(),
            (None, Some(u), _) => h(u).clone(),
            _ => Face::empty(),
        };
        out.pts_ns = pts_ns;
        out.present = out.valid && self.last_seen_ns.is_some_and(|t| now_ns.saturating_sub(t) < PRESENT_TIMEOUT_NS);
        out
    }
}

// Between two faces: only the points move; the rest is a's.
fn lerp_face(a: &Face, b: &Face, k: f32) -> Face {
    let mut out = a.clone();
    for (o, (p, q)) in out.pts.iter_mut().zip(a.pts.iter().zip(&b.pts)) {
        *o = [p[0] + (q[0] - p[0]) * k, p[1] + (q[1] - p[1]) * k];
    }
    out
}

/// The smoothing's units: the picture's aspect at a 720 px short side (the analysis frame; the
/// filter constants are tuned in those pixels).
fn units(size: (u32, u32), luma: Option<&Luma>) -> (f32, f32) {
    let (w, h) = match (size, luma) {
        ((w, h), _) if w > 0 && h > 0 => (w as f32, h as f32),
        (_, Some(l)) => (l.width as f32, l.height as f32),
        _ => (1.0, 1.0),
    };
    let k = TRACK_SHORT_SIDE / w.min(h);
    (w * k, h * k)
}

/// The renderer's face weight: eases to 1 while a face is present and to 0 when not.
#[derive(Debug, Clone, Copy, Default)]
pub struct FaceWeight {
    pub weight: f32,
    last_ns: Option<i64>,
}

impl FaceWeight {
    // `w += (target - w) * min(1, 6 dt)`, dt 0 the first time.
    pub fn update(&mut self, present: bool, now_ns: i64) -> f32 {
        let dt = self.last_ns.map_or(0.0, |t| ((now_ns - t) as f64 / 1e9) as f32);
        self.last_ns = Some(now_ns);
        let target = if present { 1.0 } else { 0.0 };
        // a clock that steps back gives a negative dt: take the target at once
        let k = if dt < 0.0 { 1.0 } else { (dt * 6.0).min(1.0) };
        self.weight += (target - self.weight) * k;
        self.weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// A plausible 478-point face, normalised, eyes level, in a 1280×720 picture.
    fn mesh(dx: f32) -> Vec<[f32; 2]> {
        let mut m: Vec<[f32; 2]> = (0..FACE_POINTS)
            .map(|i| {
                let a = i as f32 * 0.7;
                [0.5 + 0.08 * a.cos() + dx, 0.5 + 0.12 * a.sin()]
            })
            .collect();
        // eye corners: image-left eye 33 / 133, image-right 362 / 263
        m[33] = [0.42 + dx, 0.45];
        m[133] = [0.47 + dx, 0.45];
        m[362] = [0.53 + dx, 0.45];
        m[263] = [0.58 + dx, 0.45];
        m
    }

    fn push(serial: u64, pts_ns: i64, points: Vec<[f32; 2]>) -> FacePush {
        FacePush { pts_ns, present: true, score: 0.9, points, size: (1280, 720), serial }
    }

    fn miss(serial: u64, pts_ns: i64) -> FacePush {
        FacePush { pts_ns, present: false, score: 0.0, points: Vec::new(), size: (1280, 720), serial }
    }

    #[test]
    fn regions_lie_end_to_end_and_take_their_landmarks() {
        let f = assemble(&mesh(0.0));
        // the regions lie end to end and cover the points: what the shader indexes by
        for i in 1..f.region_start.len() {
            assert_eq!(f.region_start[i], f.region_start[i - 1] + f.region_count[i - 1], "region {i}");
        }
        let last = f.region_start.len() - 1;
        assert_eq!(f.region_start[last] + f.region_count[last], f.pts.len());
        let m = mesh(0.0);
        assert_eq!(f.region(Region::LeftPupil)[0], m[473]);
        assert_eq!(f.region(Region::RightPupil)[0], m[468]);
        assert_eq!(f.region(Region::Mesh)[263], m[263]);
        assert_eq!(f.region(Region::LeftEye)[0], m[263]);
    }

    #[test]
    fn one_euro_converges_on_a_still_face() {
        let mut t = Tracker::new();
        let period = 33 * MS;
        t.push(&push(1, 0, mesh(0.0)), None);
        // the face moves 5 px (of 1280) and stays there
        let dx = 5.0 / 1280.0;
        let target = assemble(&mesh(dx));
        let mut first_step = None;
        for k in 1..=90 {
            t.push(&push(1 + k as u64, k * period, mesh(dx)), None);
            let f = t.get_at(k * period, k * period);
            let err = (f.pts[0][0] - target.pts[0][0]).abs() * 1280.0;
            if k == 1 {
                first_step = Some(err);
            }
        }
        let first = first_step.unwrap();
        assert!(first > 0.5 && first < 5.0, "first step smoothed: {first}");
        let f = t.get_at(90 * period, 90 * period);
        for (p, q) in f.pts.iter().zip(&target.pts) {
            assert!((p[0] - q[0]).abs() * 1280.0 < 0.05, "{p:?} {q:?}");
            assert!((p[1] - q[1]).abs() * 720.0 < 0.05, "{p:?} {q:?}");
        }
    }

    #[test]
    fn resets_after_half_a_second() {
        let mut t = Tracker::new();
        t.push(&push(1, 0, mesh(0.0)), None);
        let target = assemble(&mesh(0.05));
        t.push(&push(2, 600 * MS, mesh(0.05)), None);
        let f = t.get_at(600 * MS, 600 * MS);
        assert!((f.pts[0][0] - target.pts[0][0]).abs() < 1e-5);
    }

    #[test]
    fn interpolates_between_two_pushes() {
        let mut t = Tracker::new();
        let p = 33 * MS;
        t.push(&push(1, 0, mesh(0.0)), None);
        t.push(&miss(2, p), None);
        t.push(&push(3, 2 * p, mesh(0.01)), None);
        assert_eq!(t.period_ns(), p);
        let (a, b) = (t.get_at(0, 0), t.get_at(2 * p, 2 * p));
        let m = t.get_at(p, p);
        assert_eq!(m.pts_ns, p);
        for i in 0..m.pts.len() {
            let want = (a.pts[i][0] + b.pts[i][0]) * 0.5;
            assert!((m.pts[i][0] - want).abs() < 1e-6);
        }
        // past the newest: extrapolated, damped
        let e = t.get_at(3 * p, 3 * p);
        let want = b.pts[0][0] + (b.pts[0][0] - a.pts[0][0]) * 0.8 * 0.5;
        assert!((e.pts[0][0] - want).abs() < 1e-6);
        // too far ahead: the newest as it is
        let far = t.get_at(7 * p, 7 * p);
        assert_eq!(far.pts, b.pts);
    }

    #[test]
    fn presence_drops_350_ms_after_the_last_hit() {
        let mut t = Tracker::new();
        assert!(!t.get_at(0, 0).valid);
        t.push(&push(1, 1000 * MS, mesh(0.0)), None);
        t.push(&miss(2, 1100 * MS), None);
        assert!(t.get_at(1100 * MS, 1349 * MS).present);
        let late = t.get_at(1100 * MS, 1351 * MS);
        assert!(!late.present);
        assert!(late.valid);
        assert_eq!(late.pts.len(), 632);
    }

    #[test]
    fn old_serials_are_ignored() {
        let mut t = Tracker::new();
        assert!(t.push(&push(5, 0, mesh(0.0)), None));
        assert!(!t.push(&push(5, 0, mesh(0.0)), None));
        assert!(!t.push(&push(4, 0, mesh(0.0)), None));
    }

    #[test]
    fn face_weight_eases() {
        let mut w = FaceWeight::default();
        assert_eq!(w.update(true, 0), 0.0);
        let v = w.update(true, 100 * MS);
        assert!(v > 0.0 && v < 1.0, "on its way in: {v}");
        assert_eq!(w.update(true, 400 * MS), 1.0);
    }

    /// A synthetic eye in a 400×300 luma: lids level at y 150, the lash line (bright above, dark
    /// below, a soft step) curving up to 3 px above them mid-lid.
    #[test]
    fn lid_snapping_finds_the_lash_line() {
        let (w, h) = (400u32, 300u32);
        let mut m = vec![[0.0f32, 0.0]; FACE_POINTS];
        const RIGHT: [usize; 9] = [133, 173, 157, 158, 159, 160, 161, 246, 33];
        const LEFT: [usize; 9] = [362, 398, 384, 385, 386, 387, 388, 466, 263];
        // corners: image-left eye 33 (x 60) .. 133 (x 140); image-right eye 260 .. 340
        let lid_y = 150.0;
        for k in 0..9 {
            let u = k as f32 / 8.0;
            m[RIGHT[k]] = [140.0 - 80.0 * u, lid_y];
            m[LEFT[k]] = [260.0 + 80.0 * u, lid_y];
        }
        m[105] = [100.0, 110.0];
        m[334] = [300.0, 110.0];
        let edge = |x: f32| {
            let u = if x < 200.0 { (140.0 - x) / 80.0 } else { (x - 260.0) / 80.0 };
            let u = u.clamp(0.0, 1.0);
            lid_y - 3.0 * 4.0 * u * (1.0 - u)
        };
        let mut bytes = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = ((fy - edge(fx)) / 1.0).tanh(); // -1 above, 1 below
                bytes[(y * w + x) as usize] = (130.0 - 90.0 * t).round() as u8;
            }
        }
        let luma = Luma { width: w, height: h, bytes, cbcr: Vec::new() };
        let before = m.clone();
        snap_lids(&mut m, &luma);
        for lid in [RIGHT, LEFT] {
            assert_eq!(m[lid[0]], before[lid[0]]);
            assert_eq!(m[lid[8]], before[lid[8]]);
            for &i in &lid[1..8] {
                let want = edge(before[i][0]);
                assert!((m[i][1] - want).abs() < 0.6, "point {i}: {} vs edge {want}", m[i][1]);
                assert!((m[i][0] - before[i][0]).abs() < 1e-4);
            }
        }
        // a flat picture: nothing moves
        let flat = Luma { width: w, height: h, bytes: vec![128; (w * h) as usize], cbcr: Vec::new() };
        let mut n = before.clone();
        snap_lids(&mut n, &flat);
        assert_eq!(n, before);
    }

    #[test]
    fn cheek_colour_is_sampled_and_eased() {
        let (w, h) = (1280u32, 720u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for px in rgba.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[200, 150, 120, 255]);
        }
        let luma = Luma::from_rgba(&rgba, w, h);
        let mut t = Tracker::new();
        t.push(&push(1, 0, mesh(0.0)), Some(&luma));
        let (r, g, b) = (200.0 / 255.0, 150.0 / 255.0, 120.0 / 255.0);
        let cb = 0.5 - 0.168736 * r - 0.331264 * g + 0.5 * b;
        let cr = 0.5 + 0.5 * r - 0.418688 * g - 0.081312 * b;
        let s = t.get_at(0, 0).skin_cbcr.unwrap();
        assert!((s[0] - cb).abs() < 0.003 && (s[1] - cr).abs() < 0.003);
    }
}
