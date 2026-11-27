//! What an app trusts about one catalog between fetches (catalog formats §2, §3).

use serde::{Deserialize, Serialize};

use super::envelope::Envelope;
use super::meta::{Attestation, FileRef, Revoked, Root, Shard, Snapshot, Timestamp};
use super::{PublicKey, TrustError};

/// One catalog as an app knows it: its address, the root it trusts, and the newest timestamp,
/// snapshot and revoked list it has accepted. Saved by the app (it's local data) and updated by
/// passing in fetched bytes, in this order: [`apply_root`](Self::apply_root) for each newer root,
/// [`apply_timestamp`](Self::apply_timestamp), then if it names a new snapshot
/// [`apply_snapshot`](Self::apply_snapshot), then [`check_shard`](Self::check_shard) for each
/// changed shard and [`apply_revoked`](Self::apply_revoked) when the revoked list changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogState {
    /// The base URL (`https://packages.lightair.app/catalog/v1/`).
    pub base_url: String,
    /// Whether this is Lightair's own catalog (its revoked list applies to everything).
    pub lightair: bool,
    pub root: Root,
    #[serde(default)]
    pub timestamp: Option<Timestamp>,
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
    #[serde(default)]
    pub revoked: Option<Revoked>,
}

/// What a new timestamp means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimestampOutcome {
    /// Nothing changed since the last one.
    Unchanged,
    /// A new snapshot to fetch (`snapshot.json`), with what it must match.
    NewSnapshot(FileRef),
}

/// What a new snapshot changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotChanges {
    /// Shards (hex digits) whose checksum changed, to fetch again.
    pub shards: Vec<String>,
    /// Whether the revoked list changed (fetch `revoked.json`).
    pub revoked: bool,
}

impl CatalogState {
    /// Starts trusting a catalog from a root: the one compiled into the app for Lightair's
    /// catalog, or the first one fetched when the user adds a community catalog (trust on first
    /// use: show [`fingerprint`](Self::fingerprint) and the root's name first).
    pub fn new(base_url: &str, root: &[u8], lightair: bool, now: u64) -> Result<CatalogState, TrustError> {
        let env = Envelope::from_json(root)?;
        let root = Root::trust_self_signed(&env, now)?;
        let mut base_url = base_url.trim().to_string();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        Ok(CatalogState { base_url, lightair, root, timestamp: None, snapshot: None, revoked: None })
    }

    /// The catalog's name, as its root gives it.
    pub fn name(&self) -> &str {
        &self.root.name
    }

    /// The root keys' fingerprints, for the user to compare with the catalog's own page.
    pub fn fingerprint(&self) -> String {
        self.root.role_keys(&self.root.roles.root).iter().map(PublicKey::fingerprint).collect::<Vec<_>>().join(", ")
    }

    /// The address of `path` in this catalog (`timestamp.json`, `root/2.json`, `shards/a.json`).
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path.trim_start_matches('/'))
    }

    /// The next root's path (`root/<n+1>.json`), to fetch until there's none.
    pub fn next_root_path(&self) -> String {
        format!("root/{}.json", self.root.version + 1)
    }

    /// Accepts the next root (signed by the current root's keys and its own).
    pub fn apply_root(&mut self, bytes: &[u8], now: u64) -> Result<(), TrustError> {
        let env = Envelope::from_json(bytes)?;
        self.root = self.root.next(&env, now)?;
        Ok(())
    }

    /// Whether the trusted root has expired (the catalog's keys need replacing; no updates until
    /// a newer root arrives).
    pub fn root_expired(&self, now: u64) -> bool {
        super::parse_rfc3339(&self.root.expires).is_none_or(|at| now > at)
    }

    /// Checks `timestamp.json`.
    pub fn apply_timestamp(&mut self, bytes: &[u8], now: u64) -> Result<TimestampOutcome, TrustError> {
        let env = Envelope::from_json(bytes)?;
        let t = Timestamp::verify(&env, &self.root, now)?;
        if let Some(old) = &self.timestamp
            && t.version < old.version
        {
            return Err(TrustError::Rollback("timestamp".into()));
        }
        let snap_version = t.snapshot.version.unwrap_or(0);
        if let Some(s) = &self.snapshot
            && snap_version < s.version
        {
            return Err(TrustError::Rollback("list".into()));
        }
        let unchanged = self.snapshot.as_ref().is_some_and(|s| s.version == snap_version)
            && self.timestamp.as_ref().is_some_and(|o| o.snapshot == t.snapshot);
        let pointer = t.snapshot.clone();
        self.timestamp = Some(t);
        Ok(if unchanged { TimestampOutcome::Unchanged } else { TimestampOutcome::NewSnapshot(pointer) })
    }

    /// Checks `snapshot.json` against the accepted timestamp, and says what changed.
    pub fn apply_snapshot(&mut self, bytes: &[u8], now: u64) -> Result<SnapshotChanges, TrustError> {
        let pointer = self
            .timestamp
            .as_ref()
            .map(|t| t.snapshot.clone())
            .ok_or_else(|| TrustError::Malformed("a list arrived before its timestamp".into()))?;
        pointer.check(bytes, "catalog's list")?;
        let env = Envelope::from_json(bytes)?;
        let s = Snapshot::verify(&env, &self.root, now)?;
        if pointer.version.is_some_and(|v| v != s.version) {
            return Err(TrustError::Mismatch("the catalog's list isn't the version its timestamp names".into()));
        }
        let mut changes = SnapshotChanges::default();
        match &self.snapshot {
            Some(old) => {
                if s.version < old.version {
                    return Err(TrustError::Rollback("list".into()));
                }
                if s.revoked.version.unwrap_or(0) < old.revoked.version.unwrap_or(0) {
                    return Err(TrustError::Rollback("revoked list".into()));
                }
                changes.shards =
                    s.shards.iter().filter(|(k, v)| old.shards.get(*k) != Some(*v)).map(|(k, _)| k.clone()).collect();
                changes.revoked = old.revoked != s.revoked;
            }
            None => {
                changes.shards = s.shards.keys().cloned().collect();
                changes.revoked = true;
            }
        }
        self.snapshot = Some(s);
        Ok(changes)
    }

    /// Checks a fetched shard against the accepted snapshot.
    pub fn check_shard(&self, shard: &str, bytes: &[u8]) -> Result<Shard, TrustError> {
        let pointer = self
            .snapshot
            .as_ref()
            .and_then(|s| s.shards.get(shard))
            .ok_or_else(|| TrustError::Malformed(format!("the catalog's list has no shard {shard}")))?;
        pointer.check(bytes, "catalog's shard")?;
        Shard::parse(bytes)
    }

    /// Checks `revoked.json` against the accepted snapshot and keeps it.
    pub fn apply_revoked(&mut self, bytes: &[u8]) -> Result<&Revoked, TrustError> {
        let pointer = self
            .snapshot
            .as_ref()
            .map(|s| s.revoked.clone())
            .ok_or_else(|| TrustError::Malformed("a revoked list arrived before the catalog's list".into()))?;
        pointer.check(bytes, "catalog's revoked list")?;
        let env = Envelope::from_json(bytes)?;
        let r = Revoked::verify(&env, &self.root)?;
        if let Some(old) = &self.revoked
            && r.version < old.version
        {
            return Err(TrustError::Rollback("revoked list".into()));
        }
        Ok(self.revoked.insert(r))
    }

    /// Verifies an attestation with this catalog's keys.
    pub fn verify_attestation(&self, env: &Envelope) -> Result<Attestation, TrustError> {
        Attestation::verify(env, &self.root)
    }

    /// Whether the accepted list is still current (not expired): when it isn't, the app says the
    /// catalog is out of date and keeps working with what's installed.
    pub fn is_current(&self, now: u64) -> bool {
        self.snapshot.as_ref().and_then(|s| super::parse_rfc3339(&s.expires)).is_some_and(|at| now <= at)
            && self.timestamp.as_ref().and_then(|t| super::parse_rfc3339(&t.expires)).is_some_and(|at| now <= at)
    }
}
