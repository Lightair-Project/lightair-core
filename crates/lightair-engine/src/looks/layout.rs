//! The face layout: the face's landmarks and the reshape sliders turned into warp primitives and
//! region ellipses in output pixels, packed as the 192×1 RGBA32F data texture the main pass
//! reads, and the outlines the landmark mask is drawn from ([`face_ring`], [`lips_outer`],
//! [`mouth_opening`], [`fan`], [`band`]).
//!
//! Everything is defined in a "face frame": origin between the eye centres, x along the eye axis
//! (image-left eye → image-right eye), y perpendicular towards the chin, unit = inter-ocular
//! distance (IOD), as on the desktop.

use super::{Face, MAX_POINTS, Region};

/// Most warp primitives a layout holds.
pub const MAX_WARPS: usize = 64;
/// Most region ellipses a layout holds.
pub const MAX_REGIONS: usize = 32;
/// Texels of the data texture.
pub const DATA_TEX_WIDTH: usize = MAX_WARPS * 2 + MAX_REGIONS * 2;

/// Warp primitive kinds.
pub const WARP_TRANSLATE: i32 = 0;
pub const WARP_SCALE: i32 = 1;
pub const WARP_SCALE_DIR: i32 = 2;

/// Region ellipse slots.
pub const REGION_FACE: usize = 0;
pub const REGION_EYE_L: usize = 1;
pub const REGION_BROW_L: usize = 3;
pub const REGION_LIPS: usize = 5;
pub const REGION_MOUTH_INNER: usize = 6;
pub const REGION_UNDER_EYE_L: usize = 7;
pub const REGION_CHEEK_L: usize = 9;
pub const REGION_SHADOW_L: usize = 11;
pub const REGION_PERSON_HEAD: usize = 13;
pub const REGION_PERSON_BODY: usize = 14;

/// How many sticker anchors a layout holds.
pub const ANCHOR_COUNT: usize = 10;

/// Blush styles.
pub const BLUSH_LIFTED: i32 = 1;
pub const BLUSH_SUNKISSED: i32 = 2;

/// The reshape sliders: slider/100, 0..1 one-sided, −1..1 bipolar. Field order is the
/// desktop's.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Shape {
    pub face_natural: f32,
    pub face_slim: f32,
    pub face_v: f32,
    pub face_small: f32,
    pub face_narrow: f32,
    pub cheekbone: f32,
    pub jawbone: f32,
    pub chin: f32,
    pub forehead: f32,
    pub head: f32,
    pub eye_size: f32,
    pub eye_spacing: f32,
    pub eye_position: f32,
    pub eye_tilt: f32,
    pub eye_inner: f32,
    pub eye_outer: f32,
    pub eye_lower_lid: f32,
    pub eye_pupil: f32,
    pub nose_narrow: f32,
    pub nose_length: f32,
    pub nose_bridge: f32,
    pub nose_tip: f32,
    pub mouth_size: f32,
    pub mouth_position: f32,
    pub mouth_smile: f32,
    pub brow_thickness: f32,
    pub brow_position: f32,
}

impl Shape {
    /// The settings keys, in field order.
    pub const KEYS: [&'static str; 27] = [
        "face_natural",
        "face_slim",
        "face_v",
        "face_small",
        "face_narrow",
        "cheekbone",
        "jawbone",
        "chin",
        "forehead",
        "head",
        "eye_size",
        "eye_spacing",
        "eye_position",
        "eye_tilt",
        "eye_inner",
        "eye_outer",
        "eye_lower_lid",
        "eye_pupil",
        "nose_narrow",
        "nose_length",
        "nose_bridge",
        "nose_tip",
        "mouth_size",
        "mouth_position",
        "mouth_smile",
        "brow_thickness",
        "brow_position",
    ];

    pub fn fields_mut(&mut self) -> [&mut f32; 27] {
        [
            &mut self.face_natural,
            &mut self.face_slim,
            &mut self.face_v,
            &mut self.face_small,
            &mut self.face_narrow,
            &mut self.cheekbone,
            &mut self.jawbone,
            &mut self.chin,
            &mut self.forehead,
            &mut self.head,
            &mut self.eye_size,
            &mut self.eye_spacing,
            &mut self.eye_position,
            &mut self.eye_tilt,
            &mut self.eye_inner,
            &mut self.eye_outer,
            &mut self.eye_lower_lid,
            &mut self.eye_pupil,
            &mut self.nose_narrow,
            &mut self.nose_length,
            &mut self.nose_bridge,
            &mut self.nose_tip,
            &mut self.mouth_size,
            &mut self.mouth_position,
            &mut self.mouth_smile,
            &mut self.brow_thickness,
            &mut self.brow_position,
        ]
    }

    /// Any value away from 0.
    pub fn any(&self) -> bool {
        let mut s = *self;
        s.fields_mut().iter().any(|v| **v != 0.0)
    }
}

/// The face layout for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub warp_count: usize,
    /// RGBA32F texels: warps at [0, 2·MAX_WARPS), regions at [2·MAX_WARPS, …).
    pub data: Vec<f32>,
    /// Pixel rect containing every warp (x0, y0, x1, y1).
    pub bounds: [f32; 4],
    /// Inter-ocular distance in output pixels.
    pub iod_px: f32,
    /// Face frame origin (between the eyes) in pixels.
    pub origin: [f32; 2],
    /// Unit eye axis (image-left eye → image-right eye).
    pub axis_u: [f32; 2],
    /// Face units (x along the eyes, y down, IOD = 1).
    pub anchor: [[f32; 2]; ANCHOR_COUNT],
    pub has_face: bool,
}

impl Layout {
    /// All zero, bounds reset.
    fn empty() -> Layout {
        Layout {
            warp_count: 0,
            data: vec![0.0; DATA_TEX_WIDTH * 4],
            bounds: [1e9, 1e9, -1e9, -1e9],
            iod_px: 0.0,
            origin: [0.0; 2],
            axis_u: [0.0; 2],
            anchor: [[0.0; 2]; ANCHOR_COUNT],
            has_face: false,
        }
    }

    /// A region's two texels (cx, cy, rx, ry), (cs, sn, 0, 0), as the shader reads them.
    pub fn region(&self, idx: usize) -> [f32; 8] {
        let o = (MAX_WARPS * 2 + idx * 2) * 4;
        self.data[o..o + 8].try_into().expect("8 floats")
    }

    /// The data texture's bytes (192×1 RGBA32F, little-endian).
    pub fn bytes(&self) -> Vec<u8> {
        self.data.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
}

/// Face units → output pixels.
pub fn map(l: &Layout, fx: f32, fy: f32) -> [f32; 2] {
    let (ux, uy) = (l.axis_u[0], l.axis_u[1]);
    let (vx, vy, d) = (-uy, ux, l.iod_px);
    [l.origin[0] + (ux * fx + vx * fy) * d, l.origin[1] + (uy * fx + vy * fy) * d]
}

#[derive(Debug, Clone, Copy, Default)]
struct V2 {
    x: f32,
    y: f32,
}

const fn v(x: f32, y: f32) -> V2 {
    V2 { x, y }
}

/// A region's point count, centroid and bounds.
#[derive(Debug, Clone, Copy)]
struct Stats {
    n: usize,
    c: V2,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
}

/// The layout being built and the face frame it's built in.
struct Ctx<'a> {
    out: &'a mut Layout,
    m: V2,
    u: V2,
    v: V2,
    d: f32,
    weight: f32,
    side_pts: [Vec<V2>; 2],
}

impl Ctx<'_> {
    /// Pixels → face units.
    fn to_face(&self, p: V2) -> V2 {
        let (dx, dy) = (p.x - self.m.x, p.y - self.m.y);
        v((dx * self.u.x + dy * self.u.y) / self.d, (dx * self.v.x + dy * self.v.y) / self.d)
    }

    /// Face units → pixels.
    fn face_to_px(&self, f: V2) -> V2 {
        v(self.m.x + (self.u.x * f.x + self.v.x * f.y) * self.d, self.m.y + (self.u.y * f.x + self.v.y * f.y) * self.d)
    }

    /// A face-frame direction in pixels.
    fn dir_px(&self, f: V2) -> V2 {
        v(self.u.x * f.x + self.v.x * f.y, self.u.y * f.x + self.v.y * f.y)
    }

    /// A region's point count, centroid and bounds, in face units.
    fn region_stats(&self, face: &Face, r: Region, w: f32, h: f32) -> Stats {
        let mut s = Stats { n: 0, c: v(0.0, 0.0), x0: 1e9, x1: -1e9, y0: 1e9, y1: -1e9 };
        for p in face.region(r) {
            let f = self.to_face(v(p[0] * w, p[1] * h));
            s.c.x += f.x;
            s.c.y += f.y;
            s.x0 = s.x0.min(f.x);
            s.x1 = s.x1.max(f.x);
            s.y0 = s.y0.min(f.y);
            s.y1 = s.y1.max(f.y);
            s.n += 1;
        }
        if s.n > 0 {
            s.c.x /= s.n as f32;
            s.c.y /= s.n as f32;
        }
        s
    }

    /// Face-frame x of the contour on one side
    /// (0 = image left, 1 = image right) at height y.
    fn contour_x(&self, side: usize, y: f32) -> f32 {
        let p = &self.side_pts[side];
        let n = p.len();
        if n < 2 {
            // template face: ~1.0 IOD half width at the eyes, narrowing to the chin
            let t = (y / 1.8).clamp(0.0, 1.0);
            return (if side == 1 { 1.0 } else { -1.0 }) * (1.0 - 0.75 * t * t);
        }
        if y <= p[0].y {
            return p[0].x;
        }
        for i in 1..n {
            if y <= p[i].y {
                let t = (y - p[i - 1].y) / (p[i].y - p[i - 1].y).max(1e-4);
                return p[i - 1].x + (p[i].x - p[i - 1].x) * t;
            }
        }
        p[n - 1].x
    }

    /// Grows the layout's bounds to hold a circle of radius `r` at `c`.
    fn grow_bounds(&mut self, c: V2, r: f32) {
        let b = &mut self.out.bounds;
        b[0] = b[0].min(c.x - r);
        b[1] = b[1].min(c.y - r);
        b[2] = b[2].max(c.x + r);
        b[3] = b[3].max(c.y + r);
    }

    /// Adds one warp primitive.
    fn emit(&mut self, kind: i32, center: V2, r_f: f32, vec_px: V2, s: f32) {
        if self.out.warp_count >= MAX_WARPS {
            return;
        }
        let cp = self.face_to_px(center);
        let r = r_f * self.d;
        let o = self.out.warp_count * 8;
        self.out.data[o..o + 8].copy_from_slice(&[cp.x, cp.y, r, kind as f32, vec_px.x, vec_px.y, s, 0.0]);
        self.out.warp_count += 1;
        self.grow_bounds(cp, r);
    }

    /// Moves content near `center` by `delta` (face units).
    fn translate(&mut self, center: V2, r: f32, mut delta: V2) {
        delta.x *= self.weight;
        delta.y *= self.weight;
        if delta.x.abs() + delta.y.abs() < 1e-4 {
            return;
        }
        let dp = self.dir_px(delta);
        let d = self.d;
        self.emit(WARP_TRANSLATE, center, r, v(dp.x * d, dp.y * d), 0.0);
    }

    /// Magnifies (s > 0) or shrinks (s < 0).
    fn scale(&mut self, center: V2, r: f32, mut s: f32) {
        s *= self.weight;
        if s.abs() < 1e-4 {
            return;
        }
        self.emit(WARP_SCALE, center, r, v(0.0, 0.0), s);
    }

    /// Scales along a face-frame unit direction.
    fn scale_dir(&mut self, center: V2, r: f32, dir: V2, mut s: f32) {
        s *= self.weight;
        if s.abs() < 1e-4 {
            return;
        }
        let d = self.dir_px(dir);
        self.emit(WARP_SCALE_DIR, center, r, d, s);
    }

    /// Sets a region ellipse; `axis` is the face-frame direction of the
    /// ellipse's y radius.
    fn region(&mut self, idx: usize, center: V2, rx: f32, ry: f32, axis: V2) {
        let cp = self.face_to_px(center);
        let mut a = self.dir_px(axis);
        let len = a.x.hypot(a.y);
        a.x /= len;
        a.y /= len;
        let o = (MAX_WARPS * 2 + idx * 2) * 4;
        // shader: local = (dot(q, (cs, sn)), dot(q, (-sn, cs))) -> local y runs along `a`
        self.out.data[o..o + 8].copy_from_slice(&[
            cp.x,
            cp.y,
            (rx * self.d).max(1.0),
            (ry * self.d).max(1.0),
            a.y,
            -a.x,
            0.0,
            0.0,
        ]);
    }
}

fn cmp(a: f32, b: f32) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// A region's points in pixels, sorted by angle
/// around their centroid (a closed ring for a triangle fan); empty below 3 points.
pub fn face_ring(face: &Face, r: Region, w: f32, h: f32, max_points: usize) -> Vec<[f32; 2]> {
    let pts = face.region(r);
    let n = pts.len().min(max_points);
    if n < 3 {
        return Vec::new();
    }
    let mut ring: Vec<(f32, f32, f32)> = pts[..n].iter().map(|p| (0.0, p[0] * w, p[1] * h)).collect();
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    for p in &ring {
        cx += p.1;
        cy += p.2;
    }
    cx /= n as f32;
    cy /= n as f32;
    for p in &mut ring {
        p.0 = (p.2 - cy).atan2(p.1 - cx);
    }
    ring.sort_by(|a, b| cmp(a.0, b.0));
    ring.into_iter().map(|p| [p.1, p.2]).collect()
}

/// A closed Catmull-Rom spline through the
/// ring points, `sub` samples per segment.
fn spline_ring(pts: &[[f32; 2]], sub: usize, max_points: usize) -> Vec<[f32; 2]> {
    let n = pts.len();
    let mut out = Vec::new();
    for i in 0..n {
        if out.len() >= max_points {
            break;
        }
        let (p0, p1, p2, p3) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n], pts[(i + 2) % n]);
        for k in 0..sub {
            if out.len() >= max_points {
                break;
            }
            let t = k as f32 / sub as f32;
            let (t2, t3) = (t * t, t * t * t);
            let mut o = [0.0f32; 2];
            for a in 0..2 {
                o[a] = 0.5
                    * (2.0 * p1[a]
                        + (-p0[a] + p2[a]) * t
                        + (2.0 * p0[a] - 5.0 * p1[a] + 4.0 * p2[a] - p3[a]) * t2
                        + (-p0[a] + 3.0 * p1[a] - 3.0 * p2[a] + p3[a]) * t3);
            }
            out.push(o);
        }
    }
    out
}

/// Sorts points by angle around their centroid.
fn sort_ring(xy: &mut [[f32; 2]]) {
    let n = xy.len() as f32;
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    for p in xy.iter() {
        cx += p[0];
        cy += p[1];
    }
    cx /= n;
    cy /= n;
    xy.sort_by(|a, b| cmp((a[1] - cy).atan2(a[0] - cx), (b[1] - cy).atan2(b[0] - cx)));
}

/// Most points in a lip or mouth outline.
pub const LIP_MAX: usize = 160;

/// The outer lips' outline in pixels.
pub fn lips_outer(face: &Face, w: f32, h: f32, max_points: usize) -> Vec<[f32; 2]> {
    let ring = face_ring(face, Region::OuterLips, w, h, MAX_POINTS);
    if ring.len() < 3 {
        return Vec::new();
    }
    spline_ring(&ring, 6, max_points)
}

/// The inner lips (plus the mouth corners when the inner ring doesn't reach them), dilated a
/// little, splined.
pub fn mouth_opening(face: &Face, w: f32, h: f32, max_points: usize) -> Vec<[f32; 2]> {
    let outer = face_ring(face, Region::OuterLips, w, h, MAX_POINTS);
    let mut ring = face_ring(face, Region::InnerLips, w, h, MAX_POINTS);
    let (le, re) = (face.region(Region::LeftEye), face.region(Region::RightEye));
    if outer.len() < 3 || ring.len() < 3 || le.is_empty() || re.is_empty() {
        return Vec::new();
    }
    // mouth axis = eye axis
    let (mut ex, mut ey) = ([0.0f32; 2], [0.0f32; 2]);
    for (s, pts) in [le, re].into_iter().enumerate() {
        for p in pts {
            ex[s] += p[0] * w;
            ey[s] += p[1] * h;
        }
        ex[s] /= pts.len() as f32;
        ey[s] /= pts.len() as f32;
    }
    let (mut ux, mut uy) = (ex[1] - ex[0], ey[1] - ey[0]);
    let ul = ux.hypot(uy);
    if ul < 1.0 {
        return Vec::new();
    }
    ux /= ul;
    uy /= ul;

    // corners = outer-lip extremes along the mouth axis, nudged towards the opening
    let along = |p: [f32; 2]| p[0] * ux + p[1] * uy;
    let (mut lo, mut hi) = (0, 0);
    for i in 1..outer.len() {
        let d = along(outer[i]);
        if d < along(outer[lo]) {
            lo = i;
        }
        if d > along(outer[hi]) {
            hi = i;
        }
    }
    let ni = ring.len() as f32;
    let (mut icx, mut icy) = (0.0f32, 0.0f32);
    for p in &ring {
        icx += p[0];
        icy += p[1];
    }
    icx /= ni;
    icy /= ni;
    // only when the inner outline doesn't already reach the corners
    let (mut in_lo, mut in_hi) = (1e9f32, -1e9f32);
    for p in &ring {
        let d = along(*p);
        in_lo = in_lo.min(d);
        in_hi = in_hi.max(d);
    }
    let (out_lo, out_hi) = (along(outer[lo]), along(outer[hi]));
    if (in_hi - in_lo) < 0.85 * (out_hi - out_lo) {
        for k in [lo, hi] {
            let [px, py] = outer[k];
            ring.push([px + (icx - px) * 0.06, py + (icy - py) * 0.06]);
        }
    }
    sort_ring(&mut ring);

    // tiny dilation so the blurred mask edge stays on the lips, not on the teeth
    let mw = (outer[hi][0] - outer[lo][0]).hypot(outer[hi][1] - outer[lo][1]);
    for p in &mut ring {
        let (dx, dy) = (p[0] - icx, p[1] - icy);
        let l = dx.hypot(dy);
        if l > 1e-3 {
            p[0] += dx / l * mw * 0.02;
            p[1] += dy / l * mw * 0.02;
        }
    }
    spline_ring(&ring, 6, max_points)
}

/// A closed ring as triangles from its centroid.
pub fn fan(xy: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let n = xy.len();
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    for p in xy {
        cx += p[0];
        cy += p[1];
    }
    cx /= n as f32;
    cy /= n as f32;
    let mut out = Vec::with_capacity(n * 3);
    for i in 0..n {
        out.extend([[cx, cy], xy[i], xy[(i + 1) % n]]);
    }
    out
}

/// A band of half-width `hw` px through the
/// points, sorted along the eye axis, as triangles.
pub fn band(l: &Layout, xy: &[[f32; 2]], hw: f32) -> Vec<[f32; 2]> {
    let (ux, uy) = (l.axis_u[0], l.axis_u[1]);
    let (vx, vy) = (-uy, ux);
    let mut pts: Vec<(f32, f32, f32)> = xy.iter().map(|p| (p[0] * ux + p[1] * uy, p[0], p[1])).collect();
    pts.sort_by(|a, b| cmp(a.0, b.0));
    let mut out = Vec::new();
    for i in 0..pts.len().saturating_sub(1) {
        let (ax, ay, bx, by) = (pts[i].1, pts[i].2, pts[i + 1].1, pts[i + 1].2);
        out.extend([
            [ax - vx * hw, ay - vy * hw],
            [bx - vx * hw, by - vy * hw],
            [ax + vx * hw, ay + vy * hw],
            [bx - vx * hw, by - vy * hw],
            [bx + vx * hw, by + vy * hw],
            [ax + vx * hw, ay + vy * hw],
        ]);
    }
    out
}

/// The channel one of the landmark mask's draws writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    R,
    B,
    A,
}

/// One draw of the landmark mask: triangles (full-frame pixels) written with `value` into one
/// channel, blending off.
#[derive(Debug, Clone, PartialEq)]
pub struct MaskDraw {
    pub channel: Channel,
    pub value: f32,
    pub triangles: Vec<[f32; 2]>,
}

/// The landmark mask's geometry, in draw order: R = the
/// outer lips minus the mouth opening, B = the mouth opening and both eye interiors, A = a band
/// along each brow. `None` when there's no mask to draw (no face, weight < 0.01, or never seen).
pub fn mask_draws(face: Option<&Face>, l: &Layout, face_weight: f32, w: f32, h: f32) -> Option<Vec<MaskDraw>> {
    let face = face.filter(|f| l.has_face && face_weight >= 0.01 && f.valid)?;
    let mut draws = Vec::new();
    let mut put = |channel, value, triangles| draws.push(MaskDraw { channel, value, triangles });
    // R: lips = outer polygon minus inner (open mouth) polygon
    let lips = lips_outer(face, w, h, LIP_MAX);
    if lips.len() >= 3 {
        put(Channel::R, 1.0, fan(&lips));
        let open = mouth_opening(face, w, h, LIP_MAX);
        if open.len() >= 3 {
            put(Channel::R, 0.0, fan(&open));
            // B: the mouth opening is a protected zone too (teeth, tongue)
            put(Channel::B, 1.0, fan(&open));
        }
    }
    // B: eye interiors (keeps eyeshadow and eyeliner off the eyeball)
    for r in [Region::LeftEye, Region::RightEye] {
        let ring = face_ring(face, r, w, h, MAX_POINTS);
        if ring.len() >= 3 {
            put(Channel::B, 1.0, fan(&ring));
        }
    }
    // A: eyebrows (a band along each brow) so smoothing and tone keep brow hair crisp
    for r in [Region::LeftBrow, Region::RightBrow] {
        let ring = face_ring(face, r, w, h, MAX_POINTS);
        if ring.len() >= 2 {
            put(Channel::A, 1.0, band(l, &ring, l.iod_px * 0.05));
        }
    }
    Some(draws)
}

/// Warp primitives + region ellipses
/// in output pixels; `weight` (0..1) fades everything in and out with the face.
pub fn build(face: Option<&Face>, sh: &Shape, w: f32, h: f32, weight: f32, blush_style: i32) -> Layout {
    let mut out = Layout::empty();
    let Some(face) = face else { return out };
    if !face.valid || face.region(Region::LeftEye).is_empty() || face.region(Region::RightEye).is_empty() {
        return out;
    }

    // --- face frame from the eye anchors (eye corners with the mesh: steady through blinks) ---
    let mut e = [v(0.0, 0.0); 2];
    for (s, es) in e.iter_mut().enumerate() {
        let a = face.eye_anchor(s);
        *es = v(a[0] * w, a[1] * h);
    }
    if e[0].x > e[1].x {
        e.swap(0, 1);
    }
    let m = v((e[0].x + e[1].x) * 0.5, (e[0].y + e[1].y) * 0.5);
    let d = (e[1].x - e[0].x).hypot(e[1].y - e[0].y);
    if d < 4.0 {
        return out;
    }
    let u = v((e[1].x - e[0].x) / d, (e[1].y - e[0].y) / d);
    out.iod_px = d;
    out.origin = [m.x, m.y];
    out.axis_u = [u.x, u.y];
    out.has_face = true;
    let mut c = Ctx { out: &mut out, m, u, v: v(-u.y, u.x), d, weight, side_pts: [Vec::new(), Vec::new()] };

    // --- contour, split into image-left / image-right halves ---
    let (mut chin_y, mut chin_x, mut half_w) = (1.75f32, 0.0f32, 1.0f32);
    let contour = face.region(Region::Contour);
    for (i, p) in contour.iter().enumerate() {
        let f = c.to_face(v(p[0] * w, p[1] * h));
        if i == 0 || f.y > chin_y {
            chin_y = f.y;
            chin_x = f.x;
        }
        let side = usize::from(f.x >= 0.0);
        if c.side_pts[side].len() < MAX_POINTS {
            c.side_pts[side].push(f);
        }
    }
    if contour.len() < 5 {
        c.side_pts = [Vec::new(), Vec::new()];
        chin_y = 1.75;
        chin_x = 0.0;
    } else {
        c.side_pts[0].sort_by(|a, b| cmp(a.y, b.y));
        c.side_pts[1].sort_by(|a, b| cmp(a.y, b.y));
        half_w = c.contour_x(0, 0.3).abs().max(c.contour_x(1, 0.3).abs());
    }
    chin_y = chin_y.clamp(1.2, 2.4);

    // --- feature stats (face frame) ---
    let (eye, brow) = {
        let a = c.region_stats(face, Region::LeftEye, w, h);
        let b = c.region_stats(face, Region::RightEye, w, h);
        let eye = if a.c.x < b.c.x { [a, b] } else { [b, a] };
        let a = c.region_stats(face, Region::LeftBrow, w, h);
        let b = c.region_stats(face, Region::RightBrow, w, h);
        let brow = if a.c.x < b.c.x { [a, b] } else { [b, a] };
        (eye, brow)
    };
    let nose = c.region_stats(face, Region::Nose, w, h);
    let lips = c.region_stats(face, Region::OuterLips, w, h);
    let inner = c.region_stats(face, Region::InnerLips, w, h);

    let mut pupil = [eye[0].c, eye[1].c];
    for r in [Region::LeftPupil, Region::RightPupil] {
        if let Some(p) = face.region(r).first() {
            let f = c.to_face(v(p[0] * w, p[1] * h));
            pupil[usize::from(f.x >= 0.0)] = f;
        }
    }

    let nose_cx = if nose.n > 0 { nose.c.x } else { 0.0 };
    let tip_y = if nose.n > 0 { nose.y1 } else { 0.75 };
    let nose_w = if nose.n > 0 { (nose.x1 - nose.x0).max(0.25) } else { 0.4 };
    let mc = if lips.n > 0 { lips.c } else { v(0.0, 1.15) };
    let mw = if lips.n > 0 { (lips.x1 - lips.x0).max(0.3) } else { 0.75 };
    let mh = if lips.n > 0 { (lips.y1 - lips.y0).max(0.1) } else { 0.3 };
    let corner = [v(mc.x - mw * 0.5, mc.y), v(mc.x + mw * 0.5, mc.y)];
    let face_c = v(0.0, chin_y * 0.45);

    // ================= FACE =================
    let slim = sh.face_slim + sh.face_natural * 0.5;
    let small = sh.face_small + sh.face_natural * 0.5;
    let vface = sh.face_v + sh.face_natural * 0.3;

    for s in 0..2 {
        let inw = if s == 1 { -1.0 } else { 1.0 }; // +x moves the left side inward
        for i in 0..4 {
            let y = 0.35 + (chin_y * 0.85 - 0.35) * (i as f32 / 3.0);
            c.translate(v(c.contour_x(s, y), y), 0.5, v(inw * slim * 0.07, 0.0));
        }
        for i in 0..5 {
            let y = chin_y * 0.92 * (i as f32 / 4.0);
            let p = v(c.contour_x(s, y), y);
            c.translate(p, 0.55, v((face_c.x - p.x) * small * 0.10, (face_c.y - p.y) * small * 0.10));
        }
        c.translate(v(c.contour_x(s, chin_y * 0.78), chin_y * 0.78), 0.42, v(inw * vface * 0.09, -vface * 0.02));
        c.translate(v(c.contour_x(s, chin_y * 0.9), chin_y * 0.9), 0.38, v(inw * vface * 0.07, -vface * 0.02));

        c.translate(v(c.contour_x(s, 0.3), 0.3), 0.4, v(inw * sh.cheekbone * 0.07, 0.0));
        c.translate(v(c.contour_x(s, 0.5), 0.5), 0.35, v(inw * sh.cheekbone * 0.04, 0.0));

        let jaw_y = (mc.y + 0.2).min(chin_y - 0.25);
        c.translate(v(c.contour_x(s, jaw_y), jaw_y), 0.45, v(inw * sh.jawbone * 0.08, 0.0));
    }
    c.translate(v(chin_x, chin_y), 0.6, v(0.0, -small * 0.08));
    c.scale_dir(face_c, 1.5, v(1.0, 0.0), -sh.face_narrow * 0.10);
    c.translate(v(chin_x, chin_y), 0.55, v(0.0, sh.chin * 0.12));
    c.translate(v(0.0, -0.95), 0.75, v(0.0, -sh.forehead * 0.10));
    c.scale(v(0.0, chin_y * 0.3), 2.4, -sh.head * 0.10);

    // ================= EYES =================
    for s in 0..2 {
        let out_dir = if s == 1 { 1.0 } else { -1.0 }; // away from the nose
        let ec = eye[s].c;
        let ew = if eye[s].n > 0 { (eye[s].x1 - eye[s].x0).max(0.2) } else { 0.45 };
        let eh = if eye[s].n > 0 { (eye[s].y1 - eye[s].y0).max(0.05) } else { 0.15 };
        let outer = v(ec.x + out_dir * ew * 0.5, ec.y);
        let innr = v(ec.x - out_dir * ew * 0.5, ec.y);

        c.scale(ec, ew * 1.15, sh.eye_size * 0.20);
        c.translate(ec, 0.45, v(out_dir * sh.eye_spacing * 0.05, 0.0));
        c.translate(ec, 0.45, v(0.0, -sh.eye_position * 0.05));
        c.translate(outer, 0.22, v(0.0, -sh.eye_tilt * 0.035));
        c.translate(innr, 0.20, v(0.0, sh.eye_tilt * 0.02));
        c.translate(innr, 0.16, v(-out_dir * sh.eye_inner * 0.035, 0.0));
        c.translate(outer, 0.18, v(out_dir * sh.eye_outer * 0.035, 0.0));
        c.translate(v(ec.x, ec.y + eh * 0.5), 0.20, v(0.0, sh.eye_lower_lid * 0.03));
        c.scale(pupil[s], 0.14, sh.eye_pupil * 0.22);
    }

    // ================= NOSE =================
    c.scale_dir(v(nose_cx, tip_y - 0.08), (nose_w * 0.9).max(0.35), v(1.0, 0.0), -sh.nose_narrow * 0.22);
    c.translate(v(nose_cx, tip_y), 0.30, v(0.0, -sh.nose_length * 0.06));
    c.scale_dir(v(nose_cx, tip_y * 0.5), 0.28, v(1.0, 0.0), -sh.nose_bridge * 0.22);
    c.scale(v(nose_cx, tip_y - 0.04), 0.16, -sh.nose_tip * 0.22);

    // ================= MOUTH =================
    c.scale(mc, mw * 0.85, sh.mouth_size * 0.15);
    c.translate(mc, 0.5, v(0.0, -sh.mouth_position * 0.06));
    for (s, k) in corner.iter().enumerate() {
        let side = if s == 1 { 1.0 } else { -1.0 };
        c.translate(*k, 0.15, v(side * sh.mouth_smile * 0.01, -sh.mouth_smile * 0.035));
    }

    // ================= BROWS =================
    for b in &brow {
        if b.n == 0 {
            continue;
        }
        let bw = (b.x1 - b.x0).max(0.3);
        c.scale_dir(b.c, bw * 0.6, v(0.0, 1.0), sh.brow_thickness * 0.30);
        c.translate(b.c, bw * 0.7, v(0.0, -sh.brow_position * 0.05));
    }

    // ================= REGIONS (for colour work) =================
    let up = v(0.0, 1.0);
    c.region(REGION_FACE, v(0.0, (chin_y - 1.05) * 0.5), half_w * 1.05, (chin_y + 1.05) * 0.5, up);
    for s in 0..2 {
        let side = if s == 1 { 1.0 } else { -1.0 };
        let ew = if eye[s].n > 0 { (eye[s].x1 - eye[s].x0).max(0.2) } else { 0.45 };
        let eh = if eye[s].n > 0 { (eye[s].y1 - eye[s].y0).max(0.05) } else { 0.15 };
        c.region(REGION_EYE_L + s, eye[s].c, ew * 0.62, (eh * 0.9).max(0.07) + 0.04, up);
        if brow[s].n > 0 {
            c.region(REGION_BROW_L + s, brow[s].c, (brow[s].x1 - brow[s].x0).max(0.3) * 0.58, 0.10, up);
        }
        // dark circles: the whole eye surround, centred below the eye
        c.region(REGION_UNDER_EYE_L + s, v(eye[s].c.x + side * 0.02, eye[s].c.y + 0.10), ew * 0.85 + 0.06, 0.34, up);

        // ---- makeup ----
        match blush_style {
            // swept up towards the temple
            BLUSH_LIFTED => c.region(
                REGION_CHEEK_L + s,
                v(eye[s].c.x + side * 0.22, eye[s].c.y + 0.42),
                0.16,
                0.30,
                v(side * 0.8, -0.6),
            ),
            // band across the nose and cheeks
            BLUSH_SUNKISSED => {
                c.region(REGION_CHEEK_L + s, v(eye[s].c.x * 0.8, eye[s].c.y + 0.48), 0.20, 0.40, v(1.0, 0.0))
            }
            // apples of the cheeks
            _ => c.region(REGION_CHEEK_L + s, v(eye[s].c.x + side * 0.08, eye[s].c.y + 0.55), 0.24, 0.20, up),
        }
        c.region(REGION_SHADOW_L + s, v(eye[s].c.x + side * 0.04, eye[s].c.y - eh * 0.5 - 0.06), ew * 0.72, 0.13, up);
    }
    // rough person silhouette: head with hair, then shoulders / torso
    c.region(REGION_PERSON_HEAD, v(0.0, (chin_y - 1.6) * 0.5), half_w * 1.55, (chin_y + 1.6) * 0.5 * 1.1, up);
    c.region(REGION_PERSON_BODY, v(0.0, chin_y + 1.9), half_w * 2.9, 2.2, up);

    // sticker anchors
    c.out.anchor = [
        [0.0, 0.0],
        [eye[0].c.x, eye[0].c.y],
        [eye[1].c.x, eye[1].c.y],
        [nose_cx, tip_y],
        [mc.x, mc.y],
        [chin_x, chin_y],
        [0.0, -0.75],
        [0.0, -1.45],
        [eye[0].c.x - 0.05, eye[0].c.y + 0.55],
        [eye[1].c.x + 0.05, eye[1].c.y + 0.55],
    ];

    c.region(REGION_LIPS, mc, mw * 0.58, mh * 0.62 + 0.02, up);
    if inner.n > 0 {
        c.region(
            REGION_MOUTH_INNER,
            v(mc.x, inner.c.y),
            (inner.x1 - inner.x0).max(mw * 0.85).max(0.1) * 0.55,
            ((inner.y1 - inner.y0) * 0.62).max(0.02),
            up,
        );
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::REGION_COUNT;
    use super::*;

    /// A synthetic frontal face (no mesh, Vision-style regions): eyes 0.2 apart at y 0.4 in a
    /// 1000×1000 picture, contour, brows, nose, lips, inner lips, pupils.
    pub(crate) fn synthetic_face() -> Face {
        let mut regions: Vec<Vec<[f32; 2]>> = vec![Vec::new(); REGION_COUNT];
        let ring = |cx: f32, cy: f32, rx: f32, ry: f32, n: usize| -> Vec<[f32; 2]> {
            (0..n)
                .map(|i| {
                    let a = i as f32 / n as f32 * std::f32::consts::TAU;
                    [cx + rx * a.cos(), cy + ry * a.sin()]
                })
                .collect()
        };
        // contour: a U from temple to temple through the chin at y 0.75
        regions[Region::Contour as usize] = (0..17)
            .map(|i| {
                let a = std::f32::consts::PI * (i as f32 / 16.0);
                [0.5 + 0.2 * a.cos(), 0.4 + 0.35 * a.sin()]
            })
            .collect();
        regions[Region::LeftEye as usize] = ring(0.6, 0.4, 0.04, 0.015, 16);
        regions[Region::RightEye as usize] = ring(0.4, 0.4, 0.04, 0.015, 16);
        regions[Region::LeftBrow as usize] = (0..10).map(|i| [0.55 + 0.01 * i as f32, 0.35]).collect();
        regions[Region::RightBrow as usize] = (0..10).map(|i| [0.36 + 0.01 * i as f32, 0.35]).collect();
        regions[Region::Nose as usize] = ring(0.5, 0.5, 0.03, 0.04, 13);
        regions[Region::OuterLips as usize] = ring(0.5, 0.62, 0.07, 0.03, 20);
        regions[Region::InnerLips as usize] = ring(0.5, 0.62, 0.05, 0.01, 20);
        regions[Region::LeftPupil as usize] = vec![[0.6, 0.4]];
        regions[Region::RightPupil as usize] = vec![[0.4, 0.4]];
        let mut face = Face {
            valid: true,
            present: true,
            pts_ns: 0,
            region_start: [0; REGION_COUNT],
            region_count: [0; REGION_COUNT],
            pts: Vec::new(),
            skin_cbcr: None,
        };
        for (r, pts) in regions.into_iter().enumerate() {
            face.region_start[r] = face.pts.len();
            face.region_count[r] = pts.len();
            face.pts.extend(pts);
        }
        face
    }

    #[test]
    fn a_frontal_face_lays_out_its_regions_around_the_eyes() {
        let face = synthetic_face();
        let (w, h) = (1000.0, 1000.0);
        let l = build(Some(&face), &Shape::default(), w, h, 1.0, 0);
        assert!(l.has_face);
        assert!((l.iod_px - 200.0).abs() < 0.01, "{}", l.iod_px);
        assert!((l.origin[0] - 500.0).abs() < 0.01 && (l.origin[1] - 400.0).abs() < 0.01);
        assert!((l.axis_u[0] - 1.0).abs() < 1e-6 && l.axis_u[1].abs() < 1e-6);
        assert_eq!(l.warp_count, 0, "no slider moved: no warps");
        assert_eq!(l.bounds, [1e9, 1e9, -1e9, -1e9]);

        // the chin is the lowest contour point: y = 0.75 → 1.75 IOD below the eyes
        let chin_y = 1.75f32;
        // the contour at y 0.3 IOD: cos(asin(0.3 / 1.75)) IOD out, interpolated between points
        let half_w = (1.0f32 - (0.3f32 / 1.75).powi(2)).sqrt();
        let f = l.region(REGION_FACE);
        assert!((f[0] - 500.0).abs() < 0.01);
        assert!((f[1] - (400.0 + (chin_y - 1.05) * 0.5 * 200.0)).abs() < 0.5);
        assert!((f[3] - (chin_y + 1.05) * 0.5 * 200.0).abs() < 0.5);
        assert!((f[2] - half_w * 1.05 * 200.0).abs() < 1.0, "{}", f[2]);
        // axis straight down: (cs, sn) = (a.y, -a.x) = (1, 0)
        assert_eq!((f[4], f[5]), (1.0, -0.0));

        // the image-left eye is region 1, the right one region 2, each at its eye
        let e = l.region(REGION_EYE_L);
        assert!((e[0] - 400.0).abs() < 0.01 && (e[1] - 400.0).abs() < 0.01);
        let ew = 0.08 * 1000.0 / 200.0; // 0.4 IOD
        assert!((e[2] - ew * 0.62 * 200.0).abs() < 0.5);
        let e = l.region(REGION_EYE_L + 1);
        assert!((e[0] - 600.0).abs() < 0.01);

        // mouth inner, lips, head and body are written; unused slots stay 0
        for r in [
            REGION_LIPS,
            REGION_MOUTH_INNER,
            REGION_UNDER_EYE_L,
            REGION_CHEEK_L,
            REGION_SHADOW_L,
            REGION_PERSON_HEAD,
            REGION_PERSON_BODY,
        ] {
            assert!(l.region(r)[2] >= 1.0, "region {r}");
        }
        assert_eq!(l.region(15)[2], 0.0);
        assert_eq!(l.bytes().len(), DATA_TEX_WIDTH * 16);
    }

    #[test]
    fn sliders_become_warps_faded_by_the_weight() {
        let face = synthetic_face();
        let shape = Shape { eye_size: 0.5, face_slim: 1.0, ..Default::default() };
        let l = build(Some(&face), &shape, 1000.0, 1000.0, 1.0, 0);
        assert!(l.warp_count > 0 && l.bounds[0] < l.bounds[2]);
        let moved = l.data[4];
        assert!(moved > 0.0, "the slim moves the side in");
        let faded = build(Some(&face), &shape, 1000.0, 1000.0, 0.5, 0);
        assert!((faded.data[4] - moved * 0.5).abs() < 1e-3, "half the weight, half the move");
        let gone = build(Some(&face), &shape, 1000.0, 1000.0, 0.0, 0);
        assert_eq!(gone.warp_count, 0);
        assert!(gone.has_face, "the regions stay");
    }

    #[test]
    fn no_face_and_close_eyes_leave_the_layout_empty() {
        let l = build(None, &Shape::default(), 640.0, 480.0, 1.0, 0);
        assert!(!l.has_face);
        assert!(l.data.iter().all(|v| *v == 0.0));
        let mut face = synthetic_face();
        for p in &mut face.pts {
            p[0] *= 0.01; // eyes 2 px apart
        }
        assert!(!build(Some(&face), &Shape::default(), 1000.0, 1000.0, 1.0, 0).has_face);
    }

    #[test]
    fn the_mask_cuts_the_mouth_from_the_lips() {
        let face = synthetic_face();
        let l = build(Some(&face), &Shape::default(), 1000.0, 1000.0, 1.0, 0);
        let draws = mask_draws(Some(&face), &l, 1.0, 1000.0, 1000.0).expect("a face");
        let kinds: Vec<_> = draws.iter().map(|d| (d.channel, d.value)).collect();
        assert_eq!(
            kinds,
            [
                (Channel::R, 1.0),
                (Channel::R, 0.0),
                (Channel::B, 1.0),
                (Channel::B, 1.0),
                (Channel::B, 1.0),
                (Channel::A, 1.0),
                (Channel::A, 1.0),
            ]
        );
        // 20 outer-lip points, 6 spline samples each, one triangle per sample
        assert_eq!(draws[0].triangles.len(), 120 * 3);
        // a brow of 10 points: 9 quads
        assert_eq!(draws[5].triangles.len(), 9 * 6);
        assert!(mask_draws(Some(&face), &l, 0.005, 1000.0, 1000.0).is_none());
        assert!(mask_draws(None, &l, 1.0, 1000.0, 1000.0).is_none());
    }
}
