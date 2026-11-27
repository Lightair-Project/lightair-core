//! The catalog's metadata: roots, timestamps, snapshots, shards, attestations and revocations
//! (catalog formats §2–§5).

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;
use serde::{Deserialize, Serialize};

use super::envelope::{Envelope, KeyEntry, Role};
use super::time::parse_rfc3339;
use super::{Target, TrustError};
use crate::{Package, Range, Sha256};

/// The catalog formats' version this crate reads and writes.
pub const FORMAT: u32 = 1;

fn expired(expires: &str, now: u64, what: &str) -> Result<(), TrustError> {
    let at = parse_rfc3339(expires)
        .ok_or_else(|| TrustError::Malformed(format!("the {what}'s expiry \"{expires}\" isn't a time")))?;
    if now > at { Err(TrustError::Expired(what.to_string())) } else { Ok(()) }
}

fn parse<T: for<'de> Deserialize<'de>>(bytes: &[u8], what: &str) -> Result<T, TrustError> {
    serde_json::from_slice(bytes).map_err(|e| TrustError::Malformed(format!("the {what} isn't valid: {e}")))
}

fn check_format(format: u32, what: &str) -> Result<(), TrustError> {
    if format == FORMAT {
        Ok(())
    } else {
        Err(TrustError::Malformed(format!(
            "the {what} is in format {format}, and this Lightair reads format {FORMAT}: update Lightair"
        )))
    }
}

/// The roles a root names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roles {
    pub root: Role,
    pub catalog: Role,
    pub timestamp: Role,
}

/// A root (`root/<n>.json`'s payload): the catalog's keys and which role each signs for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    pub format: u32,
    pub version: u64,
    pub expires: String,
    /// The catalog's name, for the user ("Lightair", a community's name).
    #[serde(default)]
    pub name: String,
    pub keys: BTreeMap<String, KeyEntry>,
    pub roles: Roles,
}

impl Root {
    /// Verifies `env` as a root signed by its own root role, and not expired. That's how a root
    /// compiled into the app or added by the user is first trusted.
    pub fn trust_self_signed(env: &Envelope, now: u64) -> Result<Root, TrustError> {
        let root: Root = parse(&env.payload_bytes()?, "root")?;
        check_format(root.format, "root")?;
        env.verify("root", &root.roles.root, &root.keys)?;
        expired(&root.expires, now, "catalog's keys")?;
        root.check_roles()?;
        Ok(root)
    }

    /// Verifies `env` as the next root after this one: version one higher, signed by this root's
    /// root role and by its own, not expired.
    pub fn next(&self, env: &Envelope, now: u64) -> Result<Root, TrustError> {
        let next: Root = parse(&env.payload_bytes()?, "root")?;
        check_format(next.format, "root")?;
        if next.version != self.version + 1 {
            return Err(TrustError::Malformed(format!(
                "the next root should be version {}, and this is version {}",
                self.version + 1,
                next.version
            )));
        }
        env.verify("root", &self.roles.root, &self.keys)?;
        env.verify("root", &next.roles.root, &next.keys)?;
        expired(&next.expires, now, "catalog's new keys")?;
        next.check_roles()?;
        Ok(next)
    }

    fn check_roles(&self) -> Result<(), TrustError> {
        for (name, role) in
            [("root", &self.roles.root), ("catalog", &self.roles.catalog), ("timestamp", &self.roles.timestamp)]
        {
            if role.keys.is_empty() || role.threshold == 0 || role.threshold as usize > role.keys.len() {
                return Err(TrustError::Malformed(format!("the root's {name} role has no usable keys")));
            }
            if let Some(k) = role.keys.iter().find(|k| !self.keys.contains_key(*k)) {
                return Err(TrustError::Malformed(format!("the root's {name} role names a key it doesn't list ({k})")));
            }
        }
        Ok(())
    }

    /// The public keys of a role, for showing (fingerprints).
    pub fn role_keys(&self, role: &Role) -> Vec<super::PublicKey> {
        role.keys
            .iter()
            .filter_map(|k| self.keys.get(k))
            .filter_map(|e| super::PublicKey::from_base64(&e.public).ok())
            .collect()
    }
}

/// A pointer to a file by version, checksum and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    pub sha256: Sha256,
    pub size: u64,
}

impl FileRef {
    pub fn of(bytes: &[u8], version: Option<u64>) -> FileRef {
        FileRef { version, sha256: Sha256::of(bytes), size: bytes.len() as u64 }
    }

    /// Whether `bytes` are the file it points to.
    pub fn check(&self, bytes: &[u8], what: &str) -> Result<(), TrustError> {
        if bytes.len() as u64 != self.size || Sha256::of(bytes) != self.sha256 {
            Err(TrustError::Mismatch(format!("the {what} doesn't match the catalog's checksum")))
        } else {
            Ok(())
        }
    }
}

/// `timestamp.json`'s payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    pub format: u32,
    pub version: u64,
    pub expires: String,
    pub snapshot: FileRef,
}

impl Timestamp {
    pub fn verify(env: &Envelope, root: &Root, now: u64) -> Result<Timestamp, TrustError> {
        let t: Timestamp = parse(&env.verify("timestamp", &root.roles.timestamp, &root.keys)?, "timestamp")?;
        check_format(t.format, "timestamp")?;
        expired(&t.expires, now, "catalog's timestamp")?;
        Ok(t)
    }
}

/// `snapshot.json`'s payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    pub version: u64,
    pub expires: String,
    /// The 16 shards by hex digit.
    pub shards: BTreeMap<String, FileRef>,
    pub revoked: FileRef,
}

impl Snapshot {
    pub fn verify(env: &Envelope, root: &Root, now: u64) -> Result<Snapshot, TrustError> {
        let s: Snapshot = parse(&env.verify("snapshot", &root.roles.catalog, &root.keys)?, "snapshot")?;
        check_format(s.format, "snapshot")?;
        expired(&s.expires, now, "catalog's list")?;
        Ok(s)
    }
}

/// The shard an id belongs in: the first hex digit of SHA-256 over the id.
pub fn shard_of(id: &str) -> String {
    Sha256::of(id.as_bytes()).short(1)
}

/// A shard (plain JSON, checked against the snapshot's checksum).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    pub format: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, Listing>,
}

impl Shard {
    pub fn parse(bytes: &[u8]) -> Result<Shard, TrustError> {
        let s: Shard = parse(bytes, "shard")?;
        check_format(s.format, "shard")?;
        Ok(s)
    }
}

/// A package's listing in a shard. Only the attestations are trusted for what's installed; the
/// rest is for showing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub tier: u8,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub support: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub previews: Vec<String>,
    #[serde(default)]
    pub latest: String,
    #[serde(default)]
    pub versions: BTreeMap<String, ListedVersion>,
}

/// One version in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedVersion {
    pub attestation: Envelope,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub gone: bool,
}

/// One file of a version: a whole `.airpkg` for a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestedFile {
    pub platform: String,
    pub url: String,
    pub size: u64,
    pub archive_sha256: Sha256,
    pub content_sha256: Sha256,
}

/// Where a version's source is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub repository: String,
    pub commit: String,
}

/// A check a version passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRef {
    pub name: String,
    pub version: String,
}

/// Who approved a native helper, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewed {
    pub by: String,
    pub at: String,
}

/// An attestation's payload: what a catalog vouches for about one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub format: u32,
    pub id: String,
    pub version: String,
    pub publisher: String,
    pub tier: u8,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub full_access: bool,
    pub lightair: String,
    pub files: Vec<AttestedFile>,
    #[serde(default)]
    pub source: Option<SourceRef>,
    #[serde(default)]
    pub checks: Vec<CheckRef>,
    #[serde(default)]
    pub reviewed: Option<Reviewed>,
    pub published: String,
}

impl Attestation {
    /// Verifies `env` as an attestation signed by `root`'s catalog role.
    pub fn verify(env: &Envelope, root: &Root) -> Result<Attestation, TrustError> {
        let a: Attestation = parse(&env.verify("attestation", &root.roles.catalog, &root.keys)?, "attestation")?;
        check_format(a.format, "attestation")?;
        if a.files.is_empty() {
            return Err(TrustError::Malformed("the attestation names no files".into()));
        }
        Ok(a)
    }

    /// The file for `target`: its own, else the one for any platform.
    pub fn file_for(&self, target: Target) -> Option<&AttestedFile> {
        self.files
            .iter()
            .find(|f| Target::parse(&f.platform) == Some(target))
            .or_else(|| self.files.iter().find(|f| Target::parse(&f.platform) == Some(Target::Any)))
    }

    /// Checks a downloaded package against `file` and this attestation: both checksums, and the
    /// manifest's id, version, permissions and package API range. Every difference is a sentence.
    pub fn check_package(&self, file: &AttestedFile, package: &Package) -> Result<(), Vec<String>> {
        let mut out = Vec::new();
        if package.archive_sha256.as_ref() != Some(&file.archive_sha256) {
            out.push("the downloaded file isn't the one the catalog checked (its checksum differs)".to_string());
        }
        if package.content_sha256 != file.content_sha256 {
            out.push("the package's contents aren't the ones the catalog checked".to_string());
        }
        let m = &package.manifest;
        if m.id.as_str() != self.id {
            out.push(format!("the package is {}, and the catalog listed {}", m.id, self.id));
        }
        if Version::parse(&self.version).ok().as_ref() != Some(&m.version) {
            out.push(format!("the package is version {}, and the catalog listed {}", m.version, self.version));
        }
        let mine: BTreeSet<String> =
            m.parsed_permissions().iter().map(|p| p.to_string()).chain(unparsed(&m.permissions)).collect();
        let theirs: BTreeSet<String> = self
            .permissions
            .iter()
            .map(|p| crate::Permission::parse(p).map_or_else(|_| p.trim().to_string(), |p| p.to_string()))
            .collect();
        if mine != theirs {
            out.push("the package asks for other permissions than the catalog listed".to_string());
        }
        if m.lightair.as_str() != self.lightair.trim() {
            out.push("the package's Lightair range isn't the one the catalog listed".to_string());
        }
        if out.is_empty() { Ok(()) } else { Err(out) }
    }
}

fn unparsed(perms: &[String]) -> Vec<String> {
    perms.iter().filter(|p| crate::Permission::parse(p).is_err()).map(|p| p.trim().to_string()).collect()
}

/// One revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Sha256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// A version range; none means every version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    pub reason: String,
    pub at: String,
}

/// `revoked.json`'s payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    pub format: u32,
    pub version: u64,
    #[serde(default)]
    pub entries: Vec<RevokedEntry>,
}

/// What's known about an installed package, to look it up in a revoked list.
#[derive(Debug, Clone, Copy)]
pub struct RevocationQuery<'a> {
    pub id: &'a str,
    pub version: &'a Version,
    /// The publisher its attestation names, when it came from a catalog.
    pub publisher: Option<&'a str>,
    pub archive_sha256: Option<&'a Sha256>,
    pub content_sha256: &'a Sha256,
    /// Whether it was installed from the catalog this list belongs to.
    pub from_this_catalog: bool,
}

impl Revoked {
    pub fn verify(env: &Envelope, root: &Root) -> Result<Revoked, TrustError> {
        let r: Revoked = parse(&env.verify("revoked", &root.roles.catalog, &root.keys)?, "revoked list")?;
        check_format(r.format, "revoked list")?;
        Ok(r)
    }

    /// The entry that revokes the package `q` describes, if any.
    ///
    /// Lightair's list (`lightairs_list`) revokes by checksum whatever the source; its id and
    /// publisher entries, and a community catalog's whole list, apply only to packages installed
    /// from that catalog.
    pub fn find(&self, q: &RevocationQuery<'_>, lightairs_list: bool) -> Option<&RevokedEntry> {
        self.entries.iter().find(|e| {
            if let Some(sha) = &e.sha256 {
                let hit = Some(sha) == q.archive_sha256 || sha == q.content_sha256;
                return hit && (q.from_this_catalog || lightairs_list);
            }
            if !q.from_this_catalog {
                return false;
            }
            if let Some(id) = &e.id {
                if id != q.id {
                    return false;
                }
                return match &e.versions {
                    None => true,
                    Some(r) => Range::parse(r).is_ok_and(|r| r.matches(q.version)),
                };
            }
            if let Some(p) = &e.publisher {
                return q.publisher == Some(p.as_str());
            }
            false
        })
    }
}
