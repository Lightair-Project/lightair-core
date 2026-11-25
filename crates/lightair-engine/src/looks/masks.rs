//! Temporal filtering of the multiclass mask: skin, face skin and hair by the ±40 rule; the
//! person by the picture's motion per 16-px tile. No spatial filtering: the renderer snaps the
//! edges to the frame.

use std::sync::Arc;

use super::Masks;
use super::face::Luma;
use crate::feeds::Segmentation;

/// Mask pixels per motion tile.
pub const PERSON_TILE: usize = 16;

// How much the picture moved in each mask tile since the last frame, as the change of each
// tile's mean luma in levels (the most changed neighbour's counts, so motion at a tile border is
// seen on both sides); 255 everywhere without a previous frame. `prev` holds the last frame's
// tile means. The luma is the frame's BT.601 luma.
pub fn tile_motion(luma: &Luma, mw: usize, mh: usize, prev: &mut Vec<f32>) -> Vec<f32> {
    const T: usize = PERSON_TILE;
    let (fw, fh) = (luma.width as usize, luma.height as usize);
    let (tw, th) = (mw.div_ceil(T), mh.div_ceil(T));
    let mut means = vec![0.0f32; tw * th];
    for ty in 0..th {
        for tx in 0..tw {
            let x0 = tx * T * fw / mw;
            let x1 = fw.min((tx + 1) * T * fw / mw);
            let y0 = ty * T * fh / mh;
            let y1 = fh.min((ty + 1) * T * fh / mh);
            let (mut sum, mut n) = (0u64, 0u64);
            for y in (y0..y1).step_by(2) {
                for x in (x0..x1).step_by(2) {
                    sum += luma.bytes[y * fw + x] as u64;
                    n += 1;
                }
            }
            means[ty * tw + tx] = if n > 0 { sum as f32 / n as f32 } else { 0.0 };
        }
    }
    let mut motion = vec![255.0f32; tw * th];
    if prev.len() == means.len() {
        for ty in 0..th as i64 {
            for tx in 0..tw as i64 {
                let mut m = 0.0f32;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (x, y) = (tx + dx, ty + dy);
                        if x >= 0 && y >= 0 && x < tw as i64 && y < th as i64 {
                            let i = y as usize * tw + x as usize;
                            m = m.max((means[i] - prev[i]).abs());
                        }
                    }
                }
                motion[ty as usize * tw + tx as usize] = m; // levels of mean luma change
            }
        }
    }
    *prev = means;
    motion
}

// The person mask blended towards `next` by the picture's motion: still areas slowly (~1/9 a
// frame), steady motion ~1/3, a big change all at once. One channel of `stride` bytes a pixel;
// motion of the wrong size counts as a big change.
pub fn smooth_person(
    mask: &mut [u8],
    next: &[u8],
    w: usize,
    h: usize,
    motion: &[f32],
    stride: usize,
    channel: usize,
) {
    const T: usize = PERSON_TILE;
    let tw = w.div_ceil(T);
    let have = motion.len() == tw * h.div_ceil(T);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * stride + channel;
            let m = if have {
                motion[(y / T) * tw + x / T]
            } else {
                255.0
            };
            let small = ((m - 1.5) / 4.5).clamp(0.0, 1.0); // noise .. steady motion
            let large = ((m - 8.0) / 16.0).clamp(0.0, 1.0); // a big change
            // of 256: still ~1/9 of the new mask per frame, steady motion ~1/3, a big change all
            let wgt = (28.0 + small * 52.0 + large * 176.0) as i32;
            let d = next[i] as i32 - mask[i] as i32;
            mask[i] = (mask[i] as i32 + ((d * wgt) >> 8)).clamp(0, 255) as u8;
        }
    }
}

/// The multiclass mask over time, for one feed.
#[derive(Debug, Default)]
pub struct MaskFilter {
    mask: Option<Arc<Vec<u8>>>,
    size: (u32, u32),
    serial: u64,
    tile_luma: Vec<f32>,
    last_serial: Option<u64>,
}

impl MaskFilter {
    pub fn new() -> MaskFilter {
        MaskFilter::default()
    }

    /// One segmentation, in push order. `luma` is the grey copy of the frame it was computed
    /// from (the person channel's tile motion; without it the person is taken as it is, as it
    /// is without a previous frame). Returns false for one already taken or malformed.
    pub fn push(&mut self, seg: &Segmentation, luma: Option<&Luma>) -> bool {
        if self.last_serial == Some(seg.serial) {
            return false;
        }
        let (mw, mh) = (seg.size.0 as usize, seg.size.1 as usize);
        if mw == 0 || mh == 0 || seg.rgba.len() != mw * mh * 4 {
            return false;
        }
        self.last_serial = Some(seg.serial);
        let usable = luma.filter(|l| {
            l.width > 0 && l.height > 0 && l.bytes.len() >= l.width as usize * l.height as usize
        });
        let motion = match usable {
            Some(l) => tile_motion(l, mw, mh, &mut self.tile_luma),
            None => {
                self.tile_luma.clear();
                Vec::new()
            }
        };
        let m = &seg.rgba;
        match &mut self.mask {
            Some(mask) if self.size == seg.size && mask.len() == m.len() => {
                // against flicker, without trailing: small changes (noise) are averaged, real
                // movement (a big change) is taken at once
                let mask = Arc::make_mut(mask);
                for i in 0..mask.len() {
                    if i % 4 == 3 {
                        continue; // the person: graded by the picture's motion, below
                    }
                    let d = m[i] as i32 - mask[i] as i32;
                    mask[i] = if !(-40..=40).contains(&d) {
                        m[i]
                    } else {
                        (mask[i] as u32 + m[i] as u32).div_ceil(2) as u8
                    };
                }
                smooth_person(mask, m, mw, mh, &motion, 4, 3);
            }
            _ => {
                self.mask = Some(Arc::new(m.clone()));
                self.size = seg.size;
            }
        }
        self.serial += 1;
        true
    }

    /// The filtered mask for drawing; its serial changes with each new one.
    pub fn masks(&self) -> Masks {
        Masks {
            seg: self
                .mask
                .as_ref()
                .map(|m| (m.clone(), self.size, self.serial)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(serial: u64, px: [u8; 4]) -> Segmentation {
        Segmentation {
            pts_ns: serial as i64,
            rgba: px.repeat(32 * 32),
            size: (32, 32),
            serial,
        }
    }

    fn grey(v: u8) -> Luma {
        Luma {
            width: 64,
            height: 64,
            bytes: vec![v; 64 * 64],
            cbcr: Vec::new(),
        }
    }

    fn first(f: &MaskFilter) -> [u8; 4] {
        let (m, _, _) = f.masks().seg.unwrap();
        [m[0], m[1], m[2], m[3]]
    }

    #[test]
    fn plus_minus_40_rule() {
        let mut f = MaskFilter::new();
        assert!(f.masks().seg.is_none());
        assert!(f.push(&seg(1, [100, 100, 100, 100]), None));
        assert_eq!(first(&f), [100, 100, 100, 100]); // the first is taken as it is
        // R +41: taken; G +40: averaged (rounded up); B -41: taken; A without luma: taken
        assert!(f.push(&seg(2, [141, 140, 59, 0]), None));
        assert_eq!(first(&f), [141, 120, 59, 0]);
        // G -40 → (120 + 80 + 1) / 2
        assert!(f.push(&seg(3, [141, 80, 59, 0]), None));
        assert_eq!(first(&f), [141, 100, 59, 0]);
        // the same serial again changes nothing
        assert!(!f.push(&seg(3, [0, 0, 0, 0]), None));
        assert_eq!(f.masks().seg.unwrap().2, 3);
    }

    #[test]
    fn person_follows_the_pictures_motion() {
        let mut f = MaskFilter::new();
        f.push(&seg(1, [0, 0, 0, 0]), Some(&grey(100)));
        // a still picture: 28/256 of the change
        f.push(&seg(2, [0, 0, 0, 255]), Some(&grey(100)));
        assert_eq!(first(&f)[3], ((255 * 28) >> 8) as u8);
        // a big change in the picture: all of it
        f.push(&seg(3, [0, 0, 0, 0]), Some(&grey(160)));
        assert_eq!(first(&f)[3], 0);
    }

    #[test]
    fn tile_motion_marks_neighbours() {
        let mut prev = Vec::new();
        let a = grey(50);
        let first = tile_motion(&a, 64, 64, &mut prev);
        assert!(first.iter().all(|&m| m == 255.0));
        let mut b = grey(50);
        // brighten tile (0, 0) of the 4×4 tiles (frame pixels 0..16 at this 1:1 scale)
        for y in 0..16 {
            for x in 0..16 {
                b.bytes[y * 64 + x] = 70;
            }
        }
        let m = tile_motion(&b, 64, 64, &mut prev);
        assert_eq!(m[0], 20.0);
        assert_eq!(m[1], 20.0);
        assert_eq!(m[4 + 1], 20.0);
        assert_eq!(m[2], 0.0);
    }
}
