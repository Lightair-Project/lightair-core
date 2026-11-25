//! SHA-256 checksums: the type, hex text, and hashing while reading or copying.

use std::fmt;
use std::io::{self, Read, Write};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest;

/// A SHA-256 checksum, written as 64 lower-case hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256(pub [u8; 32]);

impl Sha256 {
    /// The checksum of `bytes`.
    pub fn of(bytes: &[u8]) -> Sha256 {
        Sha256(sha2::Sha256::digest(bytes).into())
    }

    /// The checksum of everything `reader` gives.
    pub fn of_reader(reader: impl Read) -> io::Result<Sha256> {
        match copy_hashing(reader, None, u64::MAX) {
            Ok((sha, _)) => Ok(sha),
            Err(CopyError::Read(e) | CopyError::Write(e)) => Err(e),
            Err(CopyError::TooBig) => Err(io::Error::other("too large")),
        }
    }

    /// The first `n` hex digits (for short labels).
    pub fn short(&self, n: usize) -> String {
        let mut s = self.to_string();
        s.truncate(n);
        s
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

impl FromStr for Sha256 {
    type Err = String;

    fn from_str(s: &str) -> Result<Sha256, String> {
        let s = s.trim();
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("\"{s}\" isn't a SHA-256 checksum (64 hex digits)"));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
        }
        Ok(Sha256(out))
    }
}

impl Serialize for Sha256 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Sha256, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Why [`copy_hashing`] stopped.
#[derive(Debug)]
pub(crate) enum CopyError {
    Read(io::Error),
    Write(io::Error),
    /// More than the allowed number of bytes came out.
    TooBig,
}

/// Reads `reader` to its end (copying into `writer` if there is one), hashing as it goes. Fails
/// with [`CopyError::TooBig`] as soon as more than `max` bytes have come out, so a file that
/// claims one size and holds another (or a zip bomb) is caught without reading it all.
pub(crate) fn copy_hashing(
    mut reader: impl Read,
    mut writer: Option<&mut dyn Write>,
    max: u64,
) -> Result<(Sha256, u64), CopyError> {
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CopyError::Read(e)),
        };
        total += n as u64;
        if total > max {
            return Err(CopyError::TooBig);
        }
        hasher.update(&buf[..n]);
        if let Some(w) = writer.as_deref_mut() {
            w.write_all(&buf[..n]).map_err(CopyError::Write)?;
        }
    }
    if let Some(w) = writer {
        w.flush().map_err(CopyError::Write)?;
    }
    Ok((Sha256(hasher.finalize().into()), total))
}
