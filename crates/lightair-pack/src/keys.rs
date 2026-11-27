//! Keys (catalog formats §9): the root key, kept offline in a passphrase-encrypted file; online
//! keys (catalog, timestamp) as plain key files for the catalog server's secrets; and signing a
//! root.

use std::fs;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use lightair_packages::catalog::{
    Envelope, FORMAT, KeyFile, PublicKey, Role, Roles, Root, SecretKey, format_rfc3339, now_unix,
};
use serde::{Deserialize, Serialize};

use crate::args::Args;

/// An encrypted root key file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedKey {
    format: u32,
    kind: String,
    /// The public half, readable without the passphrase.
    public: String,
    id: String,
    kdf: Kdf,
    cipher: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kdf {
    name: String,
    m_kib: u32,
    t: u32,
    p: u32,
    salt: String,
}

const ENCRYPTED_KIND: &str = "lightair-root-key";

/// A fresh key from the system's randomness.
pub fn generate() -> Result<SecretKey, String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("no randomness: {e}"))?;
    Ok(SecretKey::from_seed(&seed))
}

fn passphrase(prompt: &str, twice: bool) -> Result<String, String> {
    if let Ok(p) = std::env::var("LIGHTAIR_ROOT_PASSPHRASE")
        && !p.is_empty()
    {
        return Ok(p);
    }
    let p = rpassword::prompt_password(prompt).map_err(|e| e.to_string())?;
    if twice {
        if p.chars().count() < 12 {
            return Err("use a passphrase of 12 characters or more".into());
        }
        let again = rpassword::prompt_password("Again: ").map_err(|e| e.to_string())?;
        if again != p {
            return Err("the passphrases don't match".into());
        }
    }
    Ok(p)
}

fn derive(pass: &str, kdf: &Kdf) -> Result<[u8; 32], String> {
    let salt = B64.decode(&kdf.salt).map_err(|_| "the key file's salt isn't base64".to_string())?;
    let params = argon2::Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32)).map_err(|e| e.to_string())?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon.hash_password_into(pass.as_bytes(), &salt, &mut key).map_err(|e| e.to_string())?;
    Ok(key)
}

fn encrypt(key: &SecretKey, pass: &str) -> Result<EncryptedKey, String> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let kdf = Kdf { name: "argon2id".into(), m_kib: 256 * 1024, t: 3, p: 1, salt: B64.encode(salt) };
    let k = derive(pass, &kdf)?;
    let cipher = chacha20poly1305::ChaCha20Poly1305::new((&k).into());
    let plain = serde_json::to_vec(&key.to_file()).map_err(|e| e.to_string())?;
    let ct = cipher.encrypt((&nonce).into(), plain.as_slice()).map_err(|_| "couldn't encrypt the key".to_string())?;
    Ok(EncryptedKey {
        format: 1,
        kind: ENCRYPTED_KIND.into(),
        public: key.public().to_base64(),
        id: key.public().id(),
        kdf,
        cipher: "chacha20poly1305".into(),
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ct),
    })
}

fn decrypt(file: &EncryptedKey, pass: &str) -> Result<SecretKey, String> {
    if file.kdf.name != "argon2id" || file.cipher != "chacha20poly1305" {
        return Err("the key file uses an encryption this lightair-pack doesn't know".into());
    }
    let k = derive(pass, &file.kdf)?;
    let cipher = chacha20poly1305::ChaCha20Poly1305::new((&k).into());
    let nonce = B64.decode(&file.nonce).map_err(|_| "the key file is damaged".to_string())?;
    let ct = B64.decode(&file.ciphertext).map_err(|_| "the key file is damaged".to_string())?;
    let nonce: [u8; 12] = nonce.try_into().map_err(|_| "the key file is damaged".to_string())?;
    let plain = cipher
        .decrypt((&nonce).into(), ct.as_slice())
        .map_err(|_| "wrong passphrase (or the key file is damaged)".to_string())?;
    let kf: KeyFile = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
    SecretKey::from_file(&kf).map_err(|e| e.to_string())
}

/// Writes a file only its owner can read.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists; refusing to overwrite a key", path.display()));
    }
    fs::write(path, bytes).map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// A secret key from a file: a plain key file, or an encrypted root key file (asks for its
/// passphrase).
pub fn load_secret(path: &Path) -> Result<SecretKey, String> {
    let bytes = fs::read(path).map_err(|e| format!("couldn't read {}: {e}", path.display()))?;
    if let Ok(enc) = serde_json::from_slice::<EncryptedKey>(&bytes)
        && enc.kind == ENCRYPTED_KIND
    {
        let pass = passphrase(&format!("Passphrase for {} (key {}): ", path.display(), enc.id), false)?;
        return decrypt(&enc, &pass);
    }
    let kf: KeyFile = serde_json::from_slice(&bytes).map_err(|_| format!("{} isn't a key file", path.display()))?;
    SecretKey::from_file(&kf).map_err(|e| e.to_string())
}

/// A public key from a key file (plain or encrypted, no passphrase needed) or base64 text.
pub fn load_public(arg: &str) -> Result<PublicKey, String> {
    let path = Path::new(arg);
    if path.is_file() {
        let bytes = fs::read(path).map_err(|e| format!("couldn't read {arg}: {e}"))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| format!("{arg} isn't a key file"))?;
        let public = v["public"].as_str().ok_or_else(|| format!("{arg} has no public key"))?;
        return PublicKey::from_base64(public).map_err(|e| e.to_string());
    }
    PublicKey::from_base64(arg).map_err(|e| e.to_string())
}

/// `lightair-pack keys …`
pub fn run(args: &mut Args) -> Result<(), String> {
    match args.next_word().as_deref() {
        Some("root") => {
            let out = args.path("--out").unwrap_or_else(|| "root-key.json".into());
            let key = generate()?;
            let pass = passphrase("A passphrase for the root key: ", true)?;
            let enc = encrypt(&key, &pass)?;
            write_private(&out, &serde_json::to_vec_pretty(&enc).unwrap())?;
            println!("Root key {} written to {} (encrypted).", key.public().id(), out.display());
            println!("Public key: {}", key.public().to_base64());
            println!("Fingerprint: {}", key.public().fingerprint());
            println!("Copy the file to two USB sticks kept apart, and never to a networked disk.");
            Ok(())
        }
        Some("online") => {
            let out = args.path("--out").ok_or("keys online needs --out <file>")?;
            let key = generate()?;
            write_private(&out, &serde_json::to_vec_pretty(&key.to_file()).unwrap())?;
            println!("Key {} written to {}.", key.public().id(), out.display());
            println!("Public key: {}", key.public().to_base64());
            println!("Put the file's contents in the catalog server's secret, then delete the file.");
            Ok(())
        }
        Some("sign-root") => sign_root(args),
        _ => Err("usage: lightair-pack keys root|online|sign-root …".into()),
    }
}

/// `keys sign-root`: writes a root naming the given keys, signed by the given root keys.
fn sign_root(args: &mut Args) -> Result<(), String> {
    let version: u64 = args
        .value("--version")
        .ok_or("sign-root needs --version <n>")?
        .parse()
        .map_err(|_| "--version isn't a number")?;
    let name = args.value("--name").unwrap_or_else(|| "Lightair".into());
    let days: u64 = args.value("--expires-days").map(|d| d.parse().unwrap_or(1825)).unwrap_or(1825);
    let out = args.path("--out").ok_or("sign-root needs --out <root/N.json>")?;
    let roots = args.values("--root");
    let catalogs = args.values("--catalog");
    let timestamps = args.values("--timestamp");
    let signers = args.values("--sign-with");
    if roots.is_empty() || catalogs.is_empty() || timestamps.is_empty() || signers.is_empty() {
        return Err("sign-root needs at least one --root, --catalog, --timestamp and --sign-with".into());
    }
    let threshold = |flag: &str, n: usize| -> Result<u32, String> {
        let t: u32 = args.peek_value(flag).map(|v| v.parse().unwrap_or(1)).unwrap_or(1);
        if t == 0 || t as usize > n { Err(format!("{flag} must be between 1 and {n}")) } else { Ok(t) }
    };
    let root_threshold = threshold("--root-threshold", roots.len())?;
    let mut keys = std::collections::BTreeMap::new();
    let mut role = |list: &[String]| -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        for k in list {
            let public = load_public(k)?;
            keys.insert(public.id(), public.entry());
            ids.push(public.id());
        }
        Ok(ids)
    };
    let roles = Roles {
        root: Role { keys: role(&roots)?, threshold: root_threshold },
        catalog: Role { keys: role(&catalogs)?, threshold: 1 },
        timestamp: Role { keys: role(&timestamps)?, threshold: 1 },
    };
    let root = Root { format: FORMAT, version, expires: format_rfc3339(now_unix() + days * 86_400), name, keys, roles };
    let secret: Vec<SecretKey> = signers.iter().map(|s| load_secret(Path::new(s))).collect::<Result<_, _>>()?;
    let refs: Vec<&SecretKey> = secret.iter().collect();
    let env = Envelope::sign_json("root", &root, &refs);
    // It must verify: on its own for version 1, and as the next root after --previous.
    match args.path("--previous") {
        Some(prev) => {
            let prev_env =
                Envelope::from_json(&fs::read(&prev).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let prev_root = Root::trust_self_signed(&prev_env, now_unix()).map_err(|e| e.to_string())?;
            prev_root
                .next(&env, now_unix())
                .map_err(|e| format!("the new root doesn't follow {}: {e}", prev.display()))?;
        }
        None => {
            Root::trust_self_signed(&env, now_unix()).map_err(|e| format!("the new root doesn't verify: {e}"))?;
        }
    }
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(&out, env.to_json()).map_err(|e| e.to_string())?;
    println!("Root version {version} written to {}.", out.display());
    Ok(())
}
