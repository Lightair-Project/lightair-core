//! Catalogs: the signed index Lightair and community catalogs publish, and checking it
//! (catalog formats, `lightair-docs/lightair/docs/plans/catalog-formats.md`).
//!
//! Nothing here touches the network or the clock on its own: the app fetches the bytes and
//! passes them in, with the time ([`now_unix`]). [`CatalogState`] keeps what's trusted between
//! fetches and is saved by the app.

mod envelope;
mod meta;
mod state;
mod time;

use std::fmt;

pub use envelope::{Envelope, KeyEntry, KeyFile, PublicKey, Role, SecretKey, Signature};
pub use meta::{
    Attestation, AttestedFile, CheckRef, FORMAT, FileRef, ListedVersion, Listing, Reviewed, RevocationQuery, Revoked,
    RevokedEntry, Roles, Root, Shard, Snapshot, SourceRef, Timestamp, shard_of,
};
pub use state::{CatalogState, SnapshotChanges, TimestampOutcome};
pub use time::{format_rfc3339, now_unix, parse_rfc3339};

use crate::Platform;

/// Why catalog data can't be trusted, as a sentence for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    /// The data isn't in the expected shape.
    #[error("the catalog's data isn't valid: {0}")]
    Malformed(String),
    /// Not enough valid signatures.
    #[error("the catalog's {kind} isn't signed by its keys ({found} of {needed} signatures)")]
    BadSignature { kind: String, found: usize, needed: usize },
    /// A key that can't be used.
    #[error("a catalog key isn't usable: {0}")]
    BadKey(String),
    /// Past its expiry: a frozen or out-of-date catalog.
    #[error("the {0} expired, so the catalog is out of date")]
    Expired(String),
    /// Older than what was already seen: a replayed catalog.
    #[error("the catalog sent an older {0} than the one this Lightair already has")]
    Rollback(String),
    /// A file doesn't match the checksum that vouches for it.
    #[error("{0}")]
    Mismatch(String),
}

/// What a catalog file is for: a platform and processor, or any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    Any,
    MacosArm64,
    MacosX64,
    WindowsX64,
    WindowsArm64,
    LinuxX64,
    Ios,
    Android,
}

impl Target {
    pub const ALL: [Target; 8] = [
        Target::Any,
        Target::MacosArm64,
        Target::MacosX64,
        Target::WindowsX64,
        Target::WindowsArm64,
        Target::LinuxX64,
        Target::Ios,
        Target::Android,
    ];

    pub fn parse(text: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.as_str() == text.trim())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Target::Any => "any",
            Target::MacosArm64 => "macos-arm64",
            Target::MacosX64 => "macos-x64",
            Target::WindowsX64 => "windows-x64",
            Target::WindowsArm64 => "windows-arm64",
            Target::LinuxX64 => "linux-x64",
            Target::Ios => "ios",
            Target::Android => "android",
        }
    }

    /// Its platform (none for `any`).
    pub fn platform(self) -> Option<Platform> {
        Some(match self {
            Target::Any => return None,
            Target::MacosArm64 | Target::MacosX64 => Platform::Macos,
            Target::WindowsX64 | Target::WindowsArm64 => Platform::Windows,
            Target::LinuxX64 => Platform::Linux,
            Target::Ios => Platform::Ios,
            Target::Android => Platform::Android,
        })
    }

    /// The target this build runs on.
    pub fn current() -> Target {
        let arm = cfg!(target_arch = "aarch64");
        if cfg!(target_os = "ios") {
            Target::Ios
        } else if cfg!(target_os = "android") {
            Target::Android
        } else if cfg!(target_os = "macos") {
            if arm { Target::MacosArm64 } else { Target::MacosX64 }
        } else if cfg!(target_os = "windows") {
            if arm { Target::WindowsArm64 } else { Target::WindowsX64 }
        } else if cfg!(target_os = "linux") {
            Target::LinuxX64
        } else {
            Target::Any
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
