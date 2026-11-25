//! Big-endian writing and ISO base media boxes.

pub(crate) trait Put {
    fn u8(&mut self, v: u8);
    fn u16(&mut self, v: u16);
    fn i16(&mut self, v: i16);
    fn u24(&mut self, v: u32);
    fn u32(&mut self, v: u32);
    fn i32(&mut self, v: i32);
    fn u64(&mut self, v: u64);
    fn bytes(&mut self, v: &[u8]);
    fn zeros(&mut self, n: usize);
}

impl Put for Vec<u8> {
    fn u8(&mut self, v: u8) {
        self.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u24(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes()[1..]);
    }
    fn u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.extend_from_slice(v);
    }
    fn zeros(&mut self, n: usize) {
        self.resize(self.len() + n, 0);
    }
}

/// A box of `kind` whose body `f` writes; its size is filled in after.
pub(crate) fn bx(out: &mut Vec<u8>, kind: &[u8; 4], f: impl FnOnce(&mut Vec<u8>)) {
    let at = out.len();
    out.u32(0);
    out.bytes(kind);
    f(out);
    let size = (out.len() - at) as u32;
    out[at..at + 4].copy_from_slice(&size.to_be_bytes());
}

/// A full box: version and flags before the body.
pub(crate) fn fbx(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, f: impl FnOnce(&mut Vec<u8>)) {
    bx(out, kind, |o| {
        o.u8(version);
        o.u24(flags);
        f(o);
    });
}

/// The identity transformation matrix of `mvhd` and `tkhd`.
pub(crate) fn unity_matrix(out: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        out.u32(v);
    }
}

/// An ISO 639-2 code packed as `mdhd` wants it (three 5-bit letters); `und` when malformed.
pub(crate) fn packed_language(code: &str) -> u16 {
    let b = code.as_bytes();
    if b.len() != 3 || !b.iter().all(u8::is_ascii_lowercase) {
        return packed_language("und");
    }
    b.iter().fold(0u16, |acc, &c| (acc << 5) | (c - 0x60) as u16)
}

/// `ns` in units of `1 / timescale` seconds, rounded to the nearest.
pub(crate) fn ticks(ns: i64, timescale: u32) -> i64 {
    let v = ns as i128 * timescale as i128;
    let half = 500_000_000i128 * v.signum();
    ((v + half) / 1_000_000_000) as i64
}
