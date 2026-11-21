//! A take's input log (`input.bin`, docs/plans/screen-studio.md 4.1): the pointer, clicks,
//! scrolls, keys and pointer shapes, the same file on macOS and Windows.
//!
//! The file is a 16-byte header, then 24-byte records in time order:
//!
//! ```text
//! header:  "LAINPUT1" · u32 version · u32 flags (bit 0: clicks and keys were listened to)
//! record:  u8 kind · u8 button/shape · u16 code/modifiers... · u64 time (µs) · f32 a · f32 b
//! ```
//!
//! little-endian. Records are appended and flushed often, so a take that stops abruptly keeps
//! its log up to the last flush; a torn last record is ignored on reading.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

const MAGIC: &[u8; 8] = b"LAINPUT1";
const VERSION: u32 = 1;
const RECORD: usize = 24;

/// What a record holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Entry {
    /// The pointer, in display points.
    Move {
        x: f32,
        y: f32,
    },
    Down {
        button: u8,
        x: f32,
        y: f32,
    },
    Up {
        button: u8,
        x: f32,
        y: f32,
    },
    Scroll {
        dx: f32,
        dy: f32,
    },
    KeyDown {
        code: u16,
        modifiers: u16,
    },
    KeyUp {
        code: u16,
        modifiers: u16,
    },
    /// The pointer's shape (`lightair_platform::input::Shape` as a number).
    Shape(u8),
    /// A display's frame in points (x, y, then width and height in the next record), so
    /// positions can be mapped to the screen track: written at the start.
    Display {
        x: f32,
        y: f32,
    },
    DisplaySize {
        w: f32,
        h: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Record {
    pub at: Duration,
    pub entry: Entry,
}

fn encode(r: &Record) -> [u8; RECORD] {
    let mut b = [0u8; RECORD];
    let (kind, small, code, a, c) = match r.entry {
        Entry::Move { x, y } => (1, 0, 0, x, y),
        Entry::Down { button, x, y } => (2, button, 0, x, y),
        Entry::Up { button, x, y } => (3, button, 0, x, y),
        Entry::Scroll { dx, dy } => (4, 0, 0, dx, dy),
        Entry::KeyDown { code, modifiers } => (5, 0, code, modifiers as f32, 0.0),
        Entry::KeyUp { code, modifiers } => (6, 0, code, modifiers as f32, 0.0),
        Entry::Shape(s) => (7, s, 0, 0.0, 0.0),
        Entry::Display { x, y } => (8, 0, 0, x, y),
        Entry::DisplaySize { w, h } => (9, 0, 0, w, h),
    };
    b[0] = kind;
    b[1] = small;
    b[2..4].copy_from_slice(&code.to_le_bytes());
    b[8..16].copy_from_slice(&(r.at.as_micros() as u64).to_le_bytes());
    b[16..20].copy_from_slice(&a.to_le_bytes());
    b[20..24].copy_from_slice(&c.to_le_bytes());
    b
}

fn decode(b: &[u8]) -> Option<Record> {
    let small = b[1];
    let code = u16::from_le_bytes([b[2], b[3]]);
    let at = Duration::from_micros(u64::from_le_bytes(b[8..16].try_into().ok()?));
    let a = f32::from_le_bytes(b[16..20].try_into().ok()?);
    let c = f32::from_le_bytes(b[20..24].try_into().ok()?);
    let entry = match b[0] {
        1 => Entry::Move { x: a, y: c },
        2 => Entry::Down { button: small, x: a, y: c },
        3 => Entry::Up { button: small, x: a, y: c },
        4 => Entry::Scroll { dx: a, dy: c },
        5 => Entry::KeyDown { code, modifiers: a as u16 },
        6 => Entry::KeyUp { code, modifiers: a as u16 },
        7 => Entry::Shape(small),
        8 => Entry::Display { x: a, y: c },
        9 => Entry::DisplaySize { w: a, h: c },
        _ => return None,
    };
    Some(Record { at, entry })
}

/// Appends records to a log.
pub struct Writer {
    out: BufWriter<File>,
}

impl Writer {
    /// A new log at `path`. `listening`: clicks and keys are in it (else the pointer only).
    pub fn create(path: &Path, listening: bool) -> io::Result<Writer> {
        let mut f = OpenOptions::new().create(true).truncate(true).write(true).open(path)?;
        f.write_all(MAGIC)?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&(listening as u32).to_le_bytes())?;
        Ok(Writer { out: BufWriter::new(f) })
    }

    pub fn write(&mut self, records: &[Record]) -> io::Result<()> {
        for r in records {
            self.out.write_all(&encode(r))?;
        }
        Ok(())
    }

    /// To disk (call about once a second).
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_data()
    }
}

/// A whole log: whether clicks and keys were listened to, and the records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Log {
    pub listening: bool,
    pub records: Vec<Record>,
}

impl Log {
    pub fn read(path: &Path) -> io::Result<Log> {
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        if bytes.len() < 16 || &bytes[..8] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an input log"));
        }
        let listening = u32::from_le_bytes(bytes[12..16].try_into().unwrap_or_default()) & 1 != 0;
        // whole records only: a torn last one (a crash mid-write) is dropped
        let records = bytes[16..].as_chunks::<RECORD>().0.iter().filter_map(|r| decode(r)).collect();
        Ok(Log { listening, records })
    }

    /// The pointer position at `at`, between the moves either side (`None` before the first).
    pub fn pointer_at(&self, at: Duration) -> Option<(f32, f32)> {
        let moves: Vec<(Duration, f32, f32)> = self
            .records
            .iter()
            .filter_map(|r| match r.entry {
                Entry::Move { x, y } => Some((r.at, x, y)),
                _ => None,
            })
            .collect();
        let i = moves.partition_point(|m| m.0 <= at);
        let before = *moves.get(i.checked_sub(1)?)?;
        let Some(after) = moves.get(i) else { return Some((before.1, before.2)) };
        let span = (after.0 - before.0).as_secs_f32();
        let t = if span > 0.0 { (at - before.0).as_secs_f32() / span } else { 0.0 };
        Some((before.1 + (after.1 - before.1) * t, before.2 + (after.2 - before.2) * t))
    }
}
