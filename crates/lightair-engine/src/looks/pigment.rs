//! Pigment calibration. In optical density (OD = −ln linear RGB) skin is
//! `m·mel + h·hem + s·(1,1,1)/√3 + bias`. From the user's own skin pixels the shading direction
//! is removed and a 2-D ICA in the remaining chromatic plane finds melanin and hemoglobin for
//! this camera and light, each clamped to within 15° of its physical prior and given the
//! prior's brightness component. At most every 2 s, blended 0.3; the priors until the first.

use super::Pigment;
use crate::feeds::Segmentation;

/// An analysed frame's pixels: RGBA8 (sRGB values, as the analysis frame's), rows y down, no
/// padding, upright as the segmentation is. The analysis frame is ≤ 720 px on the short side;
/// the sampling takes ~20k pixels whatever the size.
#[derive(Debug, Clone, Copy)]
pub struct Picture<'a> {
    pub rgba: &'a [u8],
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}

// The vector helpers.
impl std::ops::Add for V3 {
    type Output = V3;
    fn add(self, b: V3) -> V3 {
        V3 {
            x: self.x + b.x,
            y: self.y + b.y,
            z: self.z + b.z,
        }
    }
}

impl std::ops::Sub for V3 {
    type Output = V3;
    fn sub(self, b: V3) -> V3 {
        V3 {
            x: self.x - b.x,
            y: self.y - b.y,
            z: self.z - b.z,
        }
    }
}

impl std::ops::Mul<f64> for V3 {
    type Output = V3;
    fn mul(self, k: f64) -> V3 {
        V3 {
            x: self.x * k,
            y: self.y * k,
            z: self.z * k,
        }
    }
}

const fn v3(x: f64, y: f64, z: f64) -> V3 {
    V3 { x, y, z }
}

fn dot(a: V3, b: V3) -> f64 {
    a.x * b.x + a.y * b.y + a.z * b.z
}

fn cross(a: V3, b: V3) -> V3 {
    v3(
        a.y * b.z - a.z * b.y,
        a.z * b.x - a.x * b.z,
        a.x * b.y - a.y * b.x,
    )
}

fn unit(a: V3) -> V3 {
    let n = dot(a, a).sqrt();
    if n > 1e-12 { a * (1.0 / n) } else { a }
}

// The grey axis (U) and the pigments' physical priors.
const S3: f64 = 0.5773502691896258;
const U: V3 = v3(S3, S3, S3);
/// Melanin absorbs more at short wavelengths (B > G > R); hemoglobin peaks in green. R, G, B.
const PRIOR_MEL: V3 = v3(0.33, 0.51, 0.79);
const PRIOR_HEM: V3 = v3(0.16, 0.79, 0.59);
const MAX_DEG: f64 = 15.0;
/// The blend of each new calibration into the last.
pub const BLEND: f32 = 0.3;
/// The calibration interval.
pub const INTERVAL_NS: i64 = 2_000_000_000;

// `v` projected onto the chromatic plane (U removed).
fn in_plane(v: V3) -> V3 {
    v - U * dot(v, U)
}

// A chromatic-plane direction given the prior's
// brightness (along-U) component.
fn lift(dp: V3, prior: V3) -> V3 {
    let pp = in_plane(prior);
    unit(unit(dp) + U * (dot(prior, U) / dot(pp, pp).sqrt()))
}

// Within 15° of the prior, by slerp.
fn clamp_cone(d: V3, prior_p: V3) -> V3 {
    let ang = dot(d, prior_p).clamp(-1.0, 1.0).acos();
    let maxr = MAX_DEG * std::f64::consts::PI / 180.0;
    if ang <= maxr {
        return d;
    }
    let t = maxr / ang;
    unit(prior_p * (((1.0 - t) * ang).sin() / ang.sin()) + d * ((t * ang).sin() / ang.sin()))
}

// The inverse of [mel hem U] (columns), row-major.
fn build_inverse(b: &mut Pigment) -> bool {
    let (mel, hem) = (b.mel.map(f64::from), b.hem.map(f64::from));
    let m = [
        [mel[0], hem[0], S3],
        [mel[1], hem[1], S3],
        [mel[2], hem[2], S3],
    ];
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-6 {
        return false;
    }
    for r in 0..3 {
        for c in 0..3 {
            let (r1, r2, c1, c2) = ((c + 1) % 3, (c + 2) % 3, (r + 1) % 3, (r + 2) % 3);
            b.inv[r * 3 + c] = ((m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]) / det) as f32;
        }
    }
    true
}

fn set(v: V3) -> [f32; 3] {
    [v.x as f32, v.y as f32, v.z as f32]
}

fn get(v: [f32; 3]) -> V3 {
    v3(v[0] as f64, v[1] as f64, v[2] as f64)
}

// An sRGB value (0–1) as linear light.
fn srgb_to_lin(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// The physical priors, uncalibrated.
pub fn prior() -> Pigment {
    let mut b = Pigment {
        calibrated: false,
        mel: set(unit(PRIOR_MEL)),
        hem: set(unit(PRIOR_HEM)),
        inv: [0.0; 9],
    };
    build_inverse(&mut b);
    b
}

// A calibration from the confident, unshiny, unclipped skin pixels of `frame` (skin from
// `mask`'s R, RGBA `mw`×`mh` covering the frame). None with fewer than 400 samples or a
// degenerate spread.
pub fn calibrate(frame: &Picture, mask: &[u8], mw: usize, mh: usize) -> Option<Pigment> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 || mw == 0 || mh == 0 || frame.rgba.len() < w * h * 4 {
        return None;
    }
    if mask.len() < mw * mh * 4 {
        return None;
    }
    // --- sample skin pixels (confident skin, not shiny, not clipped) ---
    let lut: [f64; 256] = std::array::from_fn(|i| -(srgb_to_lin(i as f64 / 255.0).max(1e-3)).ln());
    let mut od: Vec<V3> = Vec::with_capacity(16384);
    let step = 1usize.max(((w * h) as f64 / 20000.0).sqrt() as usize);
    for y in (0..h).step_by(step) {
        let my = (mh - 1).min(y * mh / h);
        let row = &frame.rgba[y * w * 4..(y + 1) * w * 4];
        for x in (0..w).step_by(step) {
            let mx = (mw - 1).min(x * mw / w);
            if mask[(my * mw + mx) * 4] < 217 {
                continue; // skin probability > 0.85
            }
            let (r, g, b) = (
                row[x * 4] as usize,
                row[x * 4 + 1] as usize,
                row[x * 4 + 2] as usize,
            );
            let (mxc, mnc) = (r.max(g).max(b), r.min(g).min(b));
            if !(12..=247).contains(&mxc) {
                continue;
            }
            if mxc > 190 && ((mxc - mnc) as f64) < 0.25 * mxc as f64 {
                continue; // shine
            }
            od.push(v3(lut[r], lut[g], lut[b]));
        }
    }
    if od.len() < 400 {
        return None;
    }
    let n = od.len() as f64;

    // --- centre, remove shading, 2-D coordinates in the chromatic plane ---
    let mut mean = v3(0.0, 0.0, 0.0);
    for v in &od {
        mean = mean + *v;
    }
    mean = mean * (1.0 / n);
    let e1 = unit(cross(U, v3(0.0, 0.0, 1.0)));
    let e2 = cross(U, e1);
    let mut px = Vec::with_capacity(od.len());
    let mut py = Vec::with_capacity(od.len());
    let (mut cxx, mut cxy, mut cyy) = (0.0f64, 0.0f64, 0.0f64);
    for v in &od {
        let d = *v - mean;
        let (x, y) = (dot(d, e1), dot(d, e2));
        px.push(x);
        py.push(y);
        cxx += x * x;
        cxy += x * y;
        cyy += y * y;
    }
    cxx /= n;
    cxy /= n;
    cyy /= n;

    // --- whitening (2×2 eigen-decomposition) ---
    let (tr, dt) = (cxx + cyy, cxx * cyy - cxy * cxy);
    let disc = (tr * tr / 4.0 - dt).max(0.0).sqrt();
    let (l1, l2) = (tr / 2.0 + disc, tr / 2.0 - disc);
    if l2 < 1e-10 {
        return None;
    }
    let ang0 = 0.5 * (2.0 * cxy).atan2(cxx - cyy); // eigenvector of l1
    let (c0, s0) = (ang0.cos(), ang0.sin());
    // whitened z = D^-1/2 E^T p
    let zx: Vec<f64> = (0..od.len())
        .map(|i| (c0 * px[i] + s0 * py[i]) / l1.sqrt())
        .collect();
    let zy: Vec<f64> = (0..od.len())
        .map(|i| (-s0 * px[i] + c0 * py[i]) / l2.sqrt())
        .collect();

    // --- 2-D ICA: in whitened space the unmixing is a rotation; pick the angle that maximises
    //     non-Gaussianity (log-cosh negentropy proxy), brute force over 90 degrees ---
    const G_GAUSS: f64 = 0.3745672; // E[log cosh(nu)] for a unit Gaussian
    let (mut best, mut best_t) = (-1.0f64, 0.0f64);
    for k in 0..90 {
        let t = k as f64 * std::f64::consts::PI / 180.0;
        let (ct, st) = (t.cos(), t.sin());
        let (mut g1, mut g2) = (0.0f64, 0.0f64);
        for i in 0..od.len() {
            let y1 = ct * zx[i] + st * zy[i];
            let y2 = -st * zx[i] + ct * zy[i];
            g1 += y1.cosh().ln();
            g2 += y2.cosh().ln();
        }
        g1 /= n;
        g2 /= n;
        let j = (g1 - G_GAUSS) * (g1 - G_GAUSS) + (g2 - G_GAUSS) * (g2 - G_GAUSS);
        if j > best {
            best = j;
            best_t = t;
        }
    }
    // mixing directions in the plane: columns of (R W)^-1 = W^-1 R^T
    let (ct, st) = (best_t.cos(), best_t.sin());
    let unwhite = |a: f64, b: f64| {
        // p = E D^1/2 z
        let (ux, uy) = (a * l1.sqrt(), b * l2.sqrt());
        v3(0.0, 0.0, 0.0) + e1 * (c0 * ux - s0 * uy) + e2 * (s0 * ux + c0 * uy)
    };
    let mut d1 = unit(unwhite(ct, st)); // source 1 direction
    let mut d2 = unit(unwhite(-st, ct)); // source 2 direction

    // --- label + orient with the priors, clamp, lift ---
    let (pm, ph) = (unit(in_plane(PRIOR_MEL)), unit(in_plane(PRIOR_HEM)));
    if dot(d1, pm + ph) < 0.0 {
        d1 = d1 * -1.0;
    }
    if dot(d2, pm + ph) < 0.0 {
        d2 = d2 * -1.0;
    }
    let d1_is_mel =
        (dot(d1, pm).abs() - dot(d1, ph).abs()) > (dot(d2, pm).abs() - dot(d2, ph).abs());
    let (mut mel_p, mut hem_p) = if d1_is_mel { (d1, d2) } else { (d2, d1) };
    if dot(mel_p, pm) < 0.0 {
        mel_p = mel_p * -1.0;
    }
    if dot(hem_p, ph) < 0.0 {
        hem_p = hem_p * -1.0;
    }
    mel_p = clamp_cone(mel_p, pm);
    hem_p = clamp_cone(hem_p, ph);

    let mut b = Pigment {
        calibrated: true,
        mel: set(lift(mel_p, PRIOR_MEL)),
        hem: set(lift(hem_p, PRIOR_HEM)),
        inv: [0.0; 9],
    };
    build_inverse(&mut b).then_some(b)
}

// `cur` towards `next` by `k`, the inverse rebuilt (`cur` kept if it can't be).
pub fn blend(cur: &mut Pigment, next: &Pigment, k: f32) {
    let k1 = (1.0 - k) as f64;
    let k = k as f64;
    let m = unit(get(cur.mel) * k1 + get(next.mel) * k);
    let h = unit(get(cur.hem) * k1 + get(next.hem) * k);
    let mut b = Pigment {
        calibrated: cur.calibrated,
        mel: set(m),
        hem: set(h),
        inv: [0.0; 9],
    };
    if build_inverse(&mut b) {
        *cur = b;
    }
}

/// The pigment basis over time, for one feed.
#[derive(Debug, Clone)]
pub struct Calibrator {
    pigment: Pigment,
    next_ns: i64,
}

impl Default for Calibrator {
    fn default() -> Self {
        Self::new()
    }
}

impl Calibrator {
    pub fn new() -> Calibrator {
        Calibrator {
            pigment: prior(),
            next_ns: i64::MIN,
        }
    }

    /// A segmentation (raw, as pushed) and the frame it was computed from, at `now_ns`:
    /// calibrates when 2 s have passed since the last try. Returns whether it calibrated.
    pub fn push(&mut self, seg: &Segmentation, frame: &Picture, now_ns: i64) -> bool {
        let (mw, mh) = (seg.size.0 as usize, seg.size.1 as usize);
        if mw == 0 || mh == 0 || seg.rgba.len() != mw * mh * 4 || now_ns < self.next_ns {
            return false;
        }
        let done = match calibrate(frame, &seg.rgba, mw, mh) {
            Some(b) => {
                if self.pigment.calibrated {
                    blend(&mut self.pigment, &b, BLEND);
                } else {
                    self.pigment = b;
                }
                self.pigment.calibrated = true;
                true
            }
            None => false,
        };
        self.next_ns = now_ns.saturating_add(INTERVAL_NS);
        done
    }

    /// The basis to draw with: the priors until the first calibration.
    pub fn pigment(&self) -> Pigment {
        self.pigment
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn angle(a: [f32; 3], b: V3) -> f64 {
        let a = unit(get(a));
        dot(a, unit(b)).clamp(-1.0, 1.0).acos().to_degrees()
    }

    fn check_inverse(p: &Pigment) {
        let cols = [p.mel, p.hem, [S3 as f32; 3]];
        for r in 0..3 {
            for (c, col) in cols.iter().enumerate() {
                let v: f32 = (0..3).map(|k| p.inv[r * 3 + k] * col[k]).sum();
                let want = if r == c { 1.0 } else { 0.0 };
                assert!((v - want).abs() < 1e-4, "inv·basis [{r}][{c}] = {v}");
            }
        }
    }

    #[test]
    fn priors_before_calibration() {
        let c = Calibrator::new();
        let p = c.pigment();
        assert!(!p.calibrated);
        assert!(angle(p.mel, PRIOR_MEL) < 1e-4);
        assert!(angle(p.hem, PRIOR_HEM) < 1e-4);
        check_inverse(&p);
    }

    /// A skin picture made from known pigment directions (each a few degrees off its prior in
    /// the chromatic plane), with uniform (non-Gaussian) melanin and hemoglobin amounts.
    fn skin(mel: V3, hem: V3) -> (Vec<u8>, u32, u32) {
        let (w, h) = (160u32, 125u32);
        let bias = v3(0.55, 1.2, 1.6);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        let mut seed = 0x1234_5678u64;
        let mut rnd = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        for _ in 0..w * h {
            let od = bias + mel * (0.5 * rnd()) + hem * (0.5 * rnd()) + U * (0.3 * rnd());
            for c in [od.x, od.y, od.z] {
                let lin = (-c).exp();
                let s = if lin <= 0.0031308 {
                    lin * 12.92
                } else {
                    1.055 * lin.powf(1.0 / 2.4) - 0.055
                };
                rgba.push((s * 255.0).round().clamp(0.0, 255.0) as u8);
            }
            rgba.push(255);
        }
        (rgba, w, h)
    }

    /// `prior` turned by `deg` in the chromatic plane (about U), with its own along-U part.
    fn turned(prior: V3, deg: f64) -> V3 {
        let p = in_plane(prior);
        let q = cross(U, p);
        let t = deg.to_radians();
        unit(p * t.cos() + q * t.sin() + U * dot(prior, U))
    }

    #[test]
    fn calibrates_from_synthetic_skin() {
        let (mel, hem) = (turned(PRIOR_MEL, 6.0), turned(PRIOR_HEM, -5.0));
        let (rgba, w, h) = skin(mel, hem);
        let seg = Segmentation {
            pts_ns: 0,
            rgba: [255u8, 255, 0, 255].repeat(64 * 64),
            size: (64, 64),
            serial: 1,
        };
        let frame = Picture {
            rgba: &rgba,
            width: w,
            height: h,
        };
        let mut c = Calibrator::new();
        assert!(c.push(&seg, &frame, 0));
        let p = c.pigment();
        assert!(p.calibrated);
        check_inverse(&p);
        // in-plane, the found directions are the true ones (the along-U part is the prior's)
        let ip = |v: [f32; 3]| unit(in_plane(get(v)));
        let deg = |a: V3, b: V3| dot(a, b).clamp(-1.0, 1.0).acos().to_degrees();
        let mel_err = deg(ip(p.mel), unit(in_plane(mel)));
        let hem_err = deg(ip(p.hem), unit(in_plane(hem)));
        assert!(mel_err < 1.5, "melanin off by {mel_err}°");
        assert!(hem_err < 1.5, "hemoglobin off by {hem_err}°");
        // not again within 2 s; then blended 0.3 towards the next
        assert!(!c.push(&seg, &frame, INTERVAL_NS - 1));
        assert!(c.push(&seg, &frame, INTERVAL_NS));
        assert!(c.pigment().calibrated);
    }

    #[test]
    fn clamped_within_15_degrees_of_the_prior() {
        let (mel, hem) = (turned(PRIOR_MEL, 35.0), turned(PRIOR_HEM, -5.0));
        let (rgba, w, h) = skin(mel, hem);
        let frame = Picture {
            rgba: &rgba,
            width: w,
            height: h,
        };
        let mask = [255u8, 255, 0, 255].repeat(16 * 16);
        let p = calibrate(&frame, &mask, 16, 16).unwrap();
        let ip = |v: [f32; 3]| unit(in_plane(get(v)));
        let off = dot(ip(p.mel), unit(in_plane(PRIOR_MEL)))
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees();
        assert!((off - 15.0).abs() < 0.01, "melanin {off}° from its prior");
    }

    #[test]
    fn no_skin_no_calibration() {
        let rgba = [200u8, 150, 120, 255].repeat(100 * 100);
        let frame = Picture {
            rgba: &rgba,
            width: 100,
            height: 100,
        };
        let mask = [100u8, 0, 0, 255].repeat(16 * 16);
        assert!(calibrate(&frame, &mask, 16, 16).is_none());
    }
}
