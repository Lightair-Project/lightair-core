//! Signed envelopes and Ed25519 keys (catalog formats §1, §9).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signer as _, Verifier as _};
use serde::{Deserialize, Serialize};

use super::TrustError;
use crate::Sha256;

/// A signed envelope: a type, the payload's bytes (base64) and signatures over
/// `lightair:<type>\n<payload bytes>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: String,
    #[serde(default)]
    pub signatures: Vec<Signature>,
}

/// One signature in an envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// The signing key's id ([`PublicKey::id`]).
    pub key: String,
    /// The Ed25519 signature, base64.
    pub sig: String,
}

/// A public key as roots list them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEntry {
    pub scheme: String,
    /// The 32-byte Ed25519 public key, base64.
    pub public: String,
}

/// Which keys may sign for a role, and how many must.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub keys: Vec<String>,
    pub threshold: u32,
}

/// An Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(ed25519_dalek::VerifyingKey);

impl PublicKey {
    /// From its 32 bytes.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<PublicKey, TrustError> {
        ed25519_dalek::VerifyingKey::from_bytes(bytes)
            .map(PublicKey)
            .map_err(|_| TrustError::BadKey("it isn't a valid Ed25519 public key".into()))
    }

    /// From its base64 text.
    pub fn from_base64(text: &str) -> Result<PublicKey, TrustError> {
        let bytes = B64.decode(text.trim()).map_err(|_| TrustError::BadKey("its public key isn't base64".into()))?;
        let bytes: [u8; 32] =
            bytes.try_into().map_err(|_| TrustError::BadKey("its public key isn't 32 bytes".into()))?;
        PublicKey::from_bytes(&bytes)
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn to_base64(&self) -> String {
        B64.encode(self.0.to_bytes())
    }

    /// The key id: the first 16 hex digits of SHA-256 over the 32-byte key.
    pub fn id(&self) -> String {
        Sha256::of(&self.0.to_bytes()).short(16)
    }

    /// The whole SHA-256 of the key, grouped in fours for showing to a user who checks it
    /// against the catalog's own page ("Its key: 3f2a 91c0 …").
    pub fn fingerprint(&self) -> String {
        let hex = Sha256::of(&self.0.to_bytes()).to_string();
        hex.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
    }

    /// As a root lists it.
    pub fn entry(&self) -> KeyEntry {
        KeyEntry { scheme: "ed25519".into(), public: self.to_base64() }
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.id())
    }
}

/// An Ed25519 private key (its 32-byte seed).
pub struct SecretKey(ed25519_dalek::SigningKey);

impl SecretKey {
    /// From a 32-byte seed (from a good source of randomness, or a key file).
    pub fn from_seed(seed: &[u8; 32]) -> SecretKey {
        SecretKey(ed25519_dalek::SigningKey::from_bytes(seed))
    }

    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Signs `message`.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.0.sign(message).to_bytes()
    }

    /// The key as a key file holds it (catalog formats §9): `{scheme, seed, public}`.
    pub fn to_file(&self) -> KeyFile {
        KeyFile { scheme: "ed25519".into(), seed: B64.encode(self.seed()), public: self.public().to_base64() }
    }

    /// From a key file's contents; the public half must match the seed.
    pub fn from_file(file: &KeyFile) -> Result<SecretKey, TrustError> {
        if file.scheme != "ed25519" {
            return Err(TrustError::BadKey(format!("\"{}\" keys aren't supported", file.scheme)));
        }
        let seed = B64.decode(file.seed.trim()).map_err(|_| TrustError::BadKey("its seed isn't base64".into()))?;
        let seed: [u8; 32] = seed.try_into().map_err(|_| TrustError::BadKey("its seed isn't 32 bytes".into()))?;
        let key = SecretKey::from_seed(&seed);
        if !file.public.is_empty() && key.public().to_base64() != file.public.trim() {
            return Err(TrustError::BadKey("its public half doesn't match its seed".into()));
        }
        Ok(key)
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey({})", self.public().id())
    }
}

/// A key file: what an online key's secret or a decrypted root key holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFile {
    pub scheme: String,
    pub seed: String,
    pub public: String,
}

impl Envelope {
    /// The bytes a signature covers.
    pub fn message(kind: &str, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(payload.len() + kind.len() + 10);
        m.extend_from_slice(b"lightair:");
        m.extend_from_slice(kind.as_bytes());
        m.push(b'\n');
        m.extend_from_slice(payload);
        m
    }

    /// An envelope of `kind` holding `payload`, signed by each of `keys`.
    pub fn sign(kind: &str, payload: &[u8], keys: &[&SecretKey]) -> Envelope {
        let mut env = Envelope { kind: kind.to_string(), payload: B64.encode(payload), signatures: Vec::new() };
        for k in keys {
            env.add_signature(k);
        }
        env
    }

    /// An envelope of `kind` holding `payload` serialised as JSON, signed by each of `keys`.
    pub fn sign_json<T: Serialize>(kind: &str, payload: &T, keys: &[&SecretKey]) -> Envelope {
        let bytes = serde_json::to_vec(payload).expect("catalog payloads serialise");
        Envelope::sign(kind, &bytes, keys)
    }

    /// Adds `key`'s signature (replacing an earlier one by the same key).
    pub fn add_signature(&mut self, key: &SecretKey) {
        let payload = B64.decode(&self.payload).unwrap_or_default();
        let sig = key.sign(&Envelope::message(&self.kind, &payload));
        let id = key.public().id();
        self.signatures.retain(|s| s.key != id);
        self.signatures.push(Signature { key: id, sig: B64.encode(sig) });
    }

    /// The payload's bytes, unverified.
    pub fn payload_bytes(&self) -> Result<Vec<u8>, TrustError> {
        B64.decode(self.payload.trim()).map_err(|_| TrustError::Malformed("its payload isn't base64".into()))
    }

    /// Parses an envelope from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Envelope, TrustError> {
        serde_json::from_slice(bytes).map_err(|e| TrustError::Malformed(format!("it isn't a signed envelope: {e}")))
    }

    pub fn to_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("envelopes serialise");
        out.push(b'\n');
        out
    }

    /// Checks the envelope is of `kind` and carries `role`'s threshold of valid signatures from
    /// distinct keys among `keys`; returns the payload's bytes.
    pub fn verify(&self, kind: &str, role: &Role, keys: &BTreeMap<String, KeyEntry>) -> Result<Vec<u8>, TrustError> {
        if self.kind != kind {
            return Err(TrustError::Malformed(format!("expected a {kind}, found a {}", self.kind)));
        }
        let payload = self.payload_bytes()?;
        let message = Envelope::message(&self.kind, &payload);
        let threshold = role.threshold.max(1) as usize;
        let mut good = BTreeSet::new();
        for s in &self.signatures {
            if !role.keys.contains(&s.key) || good.contains(&s.key) {
                continue;
            }
            let Some(entry) = keys.get(&s.key) else { continue };
            if entry.scheme != "ed25519" {
                continue;
            }
            let Ok(public) = PublicKey::from_base64(&entry.public) else { continue };
            if public.id() != s.key {
                continue;
            }
            let Ok(sig) = B64.decode(s.sig.trim()) else { continue };
            let Ok(sig) = ed25519_dalek::Signature::from_slice(&sig) else { continue };
            if public.0.verify(&message, &sig).is_ok() {
                good.insert(s.key.clone());
            }
        }
        if good.len() >= threshold {
            Ok(payload)
        } else {
            Err(TrustError::BadSignature { kind: kind.to_string(), found: good.len(), needed: threshold })
        }
    }
}
