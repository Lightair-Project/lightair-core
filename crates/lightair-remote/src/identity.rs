//! The desktop's TLS identity for paired phones (docs/05 §1 in the mobile spec): a self-signed
//! ECDSA P-256 certificate, valid ten years, its common name the desktop's id. Phones pin its
//! SHA-256 fingerprint (from the pairing QR) and never ask a trust store. The app keeps the key in
//! the system's credential store and hands the identity back on each start.

use base64::Engine as _;
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    /// The certificate, DER.
    pub cert_der: Vec<u8>,
    /// The private key, PKCS #8 DER.
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", self.fingerprint_b64())
    }
}

impl Identity {
    /// A new identity for the desktop `id`.
    pub fn generate(id: &str) -> Result<Identity, String> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
        params.distinguished_name.push(rcgen::DnType::CommonName, id);
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(3650);
        let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
        Ok(Identity { cert_der: cert.der().to_vec(), key_der: key.serialize_der() })
    }

    /// The certificate's SHA-256.
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(&self.cert_der).into()
    }

    /// The fingerprint as the pairing link carries it (base64url, unpadded).
    pub fn fingerprint_b64(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.fingerprint())
    }

    /// The first four bytes, hex: what mDNS shows to tell two desktops apart (never trusted).
    pub fn short_fingerprint(&self) -> String {
        self.fingerprint()[..4].iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generates_a_pinnable_identity() {
        let a = super::Identity::generate("7f3c").unwrap();
        let b = super::Identity::generate("7f3c").unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint_b64().len(), 43);
        assert_eq!(a.short_fingerprint().len(), 8);
    }
}
