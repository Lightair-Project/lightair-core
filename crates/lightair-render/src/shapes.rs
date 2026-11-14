//! Lightair's own pointer shapes (docs/plans/screen-studio.md, "Cursor"), drawn here at a high
//! resolution so they stay sharp at any size: the live filter (`lightair_engine::cursor`) and
//! the renderer draw the same pixels.

/// Texture size of a shape (a 32-point pointer at 4×).
pub const RES: usize = 128;


/// A shape as premultiplied RGBA, RES×RES, and its hot spot as a fraction of the size.
pub struct Bitmap {
    pub rgba: Vec<u8>,
    pub hot: (f32, f32),
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

fn inside(p: (f32, f32), poly: &[(f32, f32)]) -> bool {
    let mut c = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            c = !c;
        }
        j = i;
    }
    c
}

/// Rasterises with 4×4 samples per pixel: `colour(x, y)` (in 0..1 units) gives RGBA or nothing.
fn raster(hot: (f32, f32), colour: impl Fn((f32, f32)) -> Option<[f32; 4]>) -> Bitmap {
    let mut rgba = vec![0u8; RES * RES * 4];
    for y in 0..RES {
        for x in 0..RES {
            let mut acc = [0f32; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let p = ((x as f32 + (sx as f32 + 0.5) / 4.0) / RES as f32, (y as f32 + (sy as f32 + 0.5) / 4.0) / RES as f32);
                    if let Some(c) = colour(p) {
                        // premultiplied
                        acc[0] += c[0] * c[3];
                        acc[1] += c[1] * c[3];
                        acc[2] += c[2] * c[3];
                        acc[3] += c[3];
                    }
                }
            }
            let i = (y * RES + x) * 4;
            for k in 0..4 {
                rgba[i + k] = (acc[k] / 16.0 * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Bitmap { rgba, hot }
}

/// A shape with a white body and a dark edge (each platform's pointers look like this).
fn outlined(poly: &[(f32, f32)], hot: (f32, f32)) -> Bitmap {
    let edge = 0.035;
    raster(hot, |p| {
        let near = (0..poly.len()).map(|i| segment_distance(p, poly[i], poly[(i + 1) % poly.len()])).fold(f32::MAX, f32::min);
        if inside(p, poly) {
            Some(if near < edge { [0.0, 0.0, 0.0, 1.0] } else { [1.0, 1.0, 1.0, 1.0] })
        } else if near < edge * 0.6 {
            Some([0.0, 0.0, 0.0, 1.0])
        } else if near < edge * 2.2 {
            // a soft shadow
            Some([0.0, 0.0, 0.0, 0.18 * (1.0 - near / (edge * 2.2))])
        } else {
            None
        }
    })
}

pub fn arrow() -> Bitmap {
    let s = 0.86;
    let poly: Vec<(f32, f32)> = [(0.0, 0.0), (0.0, 0.84), (0.22, 0.65), (0.36, 0.98), (0.49, 0.92), (0.35, 0.61), (0.62, 0.61)]
        .iter()
        .map(|(x, y)| (0.08 + x * s, 0.04 + y * s))
        .collect();
    outlined(&poly, (0.08, 0.04))
}

pub fn ibeam() -> Bitmap {
    // a stem with serifs, as one outline
    let poly = [
        (0.36, 0.08),
        (0.64, 0.08),
        (0.64, 0.15),
        (0.54, 0.15),
        (0.54, 0.85),
        (0.64, 0.85),
        (0.64, 0.92),
        (0.36, 0.92),
        (0.36, 0.85),
        (0.46, 0.85),
        (0.46, 0.15),
        (0.36, 0.15),
    ];
    outlined(&poly, (0.5, 0.5))
}

pub fn crosshair() -> Bitmap {
    let poly = [
        (0.46, 0.1),
        (0.54, 0.1),
        (0.54, 0.46),
        (0.9, 0.46),
        (0.9, 0.54),
        (0.54, 0.54),
        (0.54, 0.9),
        (0.46, 0.9),
        (0.46, 0.54),
        (0.1, 0.54),
        (0.1, 0.46),
        (0.46, 0.46),
    ];
    outlined(&poly, (0.5, 0.5))
}

/// A filled dot with a white rim.
pub fn dot(rgb: [f32; 3]) -> Bitmap {
    raster((0.5, 0.5), |(x, y)| {
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        if r < 0.24 {
            Some([rgb[0], rgb[1], rgb[2], 1.0])
        } else if r < 0.29 {
            Some([1.0, 1.0, 1.0, 1.0])
        } else if r < 0.36 {
            Some([0.0, 0.0, 0.0, 0.2 * (1.0 - (r - 0.29) / 0.07)])
        } else {
            None
        }
    })
}

/// A ring (the Ring style, and click effects).
pub fn ring(rgb: [f32; 3], radius: f32, width: f32) -> Bitmap {
    raster((0.5, 0.5), |(x, y)| {
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        ((r - radius).abs() < width / 2.0).then_some([rgb[0], rgb[1], rgb[2], 1.0])
    })
}

/// A filled disc (click circles).
pub fn disc(rgb: [f32; 3]) -> Bitmap {
    raster((0.5, 0.5), |(x, y)| {
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        (r < 0.48).then_some([rgb[0], rgb[1], rgb[2], 1.0])
    })
}

