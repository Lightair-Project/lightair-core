//! Pairing phones (docs/05 §2 in the mobile spec): one-use offers shown as a QR code, a claim the
//! person allows on the desktop, an Ed25519 key per device, and every later connection signed.
//!
//! - **Offer.** [`Pairing::offer`] makes a 128-bit secret valid for two minutes; [`Pairing::link`]
//!   turns it into the `https://lightair.app/pair#…` link the QR code shows.
//! - **Claim.** A phone connects to the TLS port asking to pair and calls `pair.claim` with the
//!   secret, its device details and its public key. The secret is checked (one use, not expired),
//!   then the person is asked ([`Pairing::new`]'s `confirm`), then the device is stored.
//! - **Every later connection.** The phone asks for a challenge (a 401 carrying
//!   `Lightair-Challenge: <nonce>`) and upgrades with
//!   `Authorization: Lightair-Key <device>:<nonce>:<unix seconds>:<signature>`, the signature over
//!   `<nonce>:<device>:<unix seconds>`. Nonces are one use and live 60 seconds; the time must be
//!   within 60 seconds of the desktop's.
//!
//! Devices are kept by the app (`on_change` hands it the list to save, [`devices_from_json`] reads
//! it back); this crate only checks them.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// The pairing link's format (`v=`).
pub const LINK_FORMAT: u32 = 1;
/// How long an offer lasts.
pub const OFFER_FOR: Duration = Duration::from_secs(120);
/// How long a challenge nonce lasts, and how far a device's clock may be off.
pub const CHALLENGE_FOR: Duration = Duration::from_secs(60);
/// Claims allowed per address per minute before a 15-minute lockout.
const CLAIMS_PER_MINUTE: usize = 5;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What a paired device may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every method.
    Control,
    /// Reading only: state, stats, previews, chat and every event; nothing that changes anything.
    Watch,
    /// Only the phone camera endpoint for one source.
    Camera,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Control => "control",
            Scope::Watch => "watch",
            Scope::Camera => "camera",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "control" => Some(Scope::Control),
            "watch" => Some(Scope::Watch),
            "camera" => Some(Scope::Camera),
            _ => None,
        }
    }

    /// Whether this scope may call `method`.
    pub fn allows(self, method: &str) -> bool {
        match self {
            Scope::Control => true,
            Scope::Watch => READ_ONLY.contains(&method),
            Scope::Camera => matches!(method, "session.hello" | "pair.forget"),
        }
    }
}

/// What a `watch` device may call (docs/05 §2, Scopes).
const READ_ONLY: &[&str] = &[
    "session.hello",
    "pair.forget",
    "events.subscribe",
    "app.version",
    "state.get",
    "workspace.get",
    "stats.get",
    "screenshot",
    "scene.thumbnails",
    "staging.get",
    "outputs.list",
    "output.list",
    "canvas.list",
    "canvas.get",
    "issues.list",
    "deck.get",
    "chat.history",
    "activity.history",
    "record.status",
    "stream.status",
    "stream.info.get",
    "stream.categories",
    "clip.status",
    "panic.get",
];

/// A paired device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub model: String,
    pub platform: String,
    pub app_version: String,
    pub public_key: [u8; 32],
    pub scope: Scope,
    /// Unix seconds.
    pub paired_at: u64,
    pub last_seen: Option<u64>,
    /// The source a camera-scope device feeds (the desktop's Phone camera layer).
    pub source: Option<String>,
}

impl Device {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "model": self.model, "platform": self.platform,
            "app_version": self.app_version, "public_key": b64().encode(self.public_key),
            "scope": self.scope.as_str(), "paired_at": self.paired_at, "last_seen": self.last_seen,
            "source": self.source,
        })
    }

    fn from_json(v: &Value) -> Option<Device> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let key = b64().decode(s("public_key")?).ok()?;
        Some(Device {
            id: s("id")?,
            name: s("name").unwrap_or_default(),
            model: s("model").unwrap_or_default(),
            platform: s("platform").unwrap_or_default(),
            app_version: s("app_version").unwrap_or_default(),
            public_key: key.try_into().ok()?,
            scope: Scope::parse(&s("scope")?)?,
            paired_at: v.get("paired_at").and_then(Value::as_u64).unwrap_or(0),
            last_seen: v.get("last_seen").and_then(Value::as_u64),
            source: s("source"),
        })
    }
}

/// The paired devices as JSON, for the app to save.
pub fn devices_to_json(devices: &[Device]) -> String {
    Value::Array(devices.iter().map(Device::to_json).collect()).to_string()
}

/// Devices saved by [`devices_to_json`]; entries that don't read are skipped.
pub fn devices_from_json(text: &str) -> Vec<Device> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(Device::from_json).collect()))
        .unwrap_or_default()
}

/// Who asks to pair, as the desktop shows it: "Allow Sam's iPhone to control Lightair?"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequest {
    pub name: String,
    pub model: String,
    pub platform: String,
    pub app_version: String,
    pub scope: Scope,
    pub peer: IpAddr,
    /// For a camera: the source it would feed.
    pub source: Option<String>,
}

/// An open offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub secret: [u8; 16],
    pub scope: Scope,
    /// Unix seconds.
    pub expires_at: u64,
}

/// The desktop as phones see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopInfo {
    /// A UUID made once and kept in the desktop's settings.
    pub id: String,
    pub name: String,
    pub version: String,
}

/// Recent claims from one address, and the end of its lockout.
type Attempts = (Vec<Instant>, Option<Instant>);
type Confirm = Box<dyn Fn(&DeviceRequest) -> bool + Send + Sync>;
type OnChange = Box<dyn Fn(&[Device]) + Send + Sync>;

/// The paired devices, open offers and challenges.
pub struct Pairing {
    devices: Mutex<Vec<Device>>,
    offers: Mutex<Vec<Offer>>,
    /// Camera offers' sources, by secret.
    sources: Mutex<HashMap<[u8; 16], String>>,
    nonces: Mutex<Vec<(String, Instant)>>,
    claims: Mutex<HashMap<IpAddr, Attempts>>,
    confirm: Confirm,
    on_change: OnChange,
}

/// Why a claim or a signature was refused, in words for the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl Pairing {
    /// `confirm` asks the person to allow a device (it may block until they answer; refuse after a
    /// minute without one); `on_change` saves the devices whenever they change.
    pub fn new(
        devices: Vec<Device>,
        confirm: impl Fn(&DeviceRequest) -> bool + Send + Sync + 'static,
        on_change: impl Fn(&[Device]) + Send + Sync + 'static,
    ) -> Arc<Pairing> {
        Arc::new(Pairing {
            devices: Mutex::new(devices),
            offers: Mutex::new(vec![]),
            sources: Mutex::new(HashMap::new()),
            nonces: Mutex::new(vec![]),
            claims: Mutex::new(HashMap::new()),
            confirm: Box::new(confirm),
            on_change: Box::new(on_change),
        })
    }

    pub fn devices(&self) -> Vec<Device> {
        self.devices.lock().clone()
    }

    /// Removes a device: its key stops working for new connections at once.
    pub fn remove(&self, id: &str) -> bool {
        let mut d = self.devices.lock();
        let before = d.len();
        d.retain(|x| x.id != id);
        let changed = d.len() != before;
        if changed {
            (self.on_change)(&d);
        }
        changed
    }

    /// A new one-use offer, valid for two minutes.
    pub fn offer(&self, scope: Scope) -> Offer {
        let mut secret = [0u8; 16];
        let _ = getrandom::fill(&mut secret);
        let offer = Offer { secret, scope, expires_at: unix_now() + OFFER_FOR.as_secs() };
        let mut offers = self.offers.lock();
        let now = unix_now();
        offers.retain(|o| o.expires_at > now);
        offers.push(offer.clone());
        offer
    }

    /// A one-use offer that pairs a phone as the camera for `source` (docs/05 §6): scope
    /// `camera`, and the link carries `c=<source>`.
    pub fn offer_camera(&self, source: &str) -> Offer {
        let offer = self.offer(Scope::Camera);
        let mut sources = self.sources.lock();
        let open: Vec<[u8; 16]> = self.offers.lock().iter().map(|o| o.secret).collect();
        sources.retain(|k, _| open.contains(k));
        sources.insert(offer.secret, source.to_owned());
        offer
    }

    /// A one-use offer to type rather than scan (docs/05 §2, "Without a camera"): six words for
    /// the person to type on the phone, which stand for the offer's secret. Valid two minutes.
    pub fn offer_words(&self, scope: Scope) -> (Offer, [String; 6]) {
        let mut bits = [0u8; 9];
        let _ = getrandom::fill(&mut bits);
        let words = words_of(&bits, 6);
        let secret = words_secret(&words.join(" ")).expect("words just made read back");
        let offer = Offer { secret, scope, expires_at: unix_now() + OFFER_FOR.as_secs() };
        let mut offers = self.offers.lock();
        let now = unix_now();
        offers.retain(|o| o.expires_at > now);
        offers.push(offer.clone());
        (offer, words.try_into().expect("six words"))
    }

    /// The source a camera offer is for.
    pub fn offer_source(&self, offer: &Offer) -> Option<String> {
        self.sources.lock().get(&offer.secret).cloned()
    }

    /// Withdraws an offer (the pairing sheet closed).
    pub fn withdraw(&self, offer: &Offer) {
        self.offers.lock().retain(|o| o.secret != offer.secret);
        self.sources.lock().remove(&offer.secret);
    }

    /// Whether an offer is open (mDNS says `pair=1`).
    pub fn open(&self) -> bool {
        let now = unix_now();
        self.offers.lock().iter().any(|o| o.expires_at > now)
    }

    /// The link the QR code shows (docs/05 §2).
    pub fn link(
        &self,
        offer: &Offer,
        desktop: &DesktopInfo,
        addresses: &[String],
        port: u16,
        fingerprint_b64: &str,
    ) -> String {
        let enc = |s: &str| {
            s.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        };
        let camera = self.offer_source(offer).map(|c| format!("&c={}", enc(&c))).unwrap_or_default();
        format!(
            "https://lightair.app/pair#v={LINK_FORMAT}&id={}&n={}&a={}&p={port}&k={}&f={fingerprint_b64}&s={}&x={}{camera}",
            enc(&desktop.id),
            enc(&desktop.name),
            addresses.iter().map(|a| enc(a)).collect::<Vec<_>>().join(","),
            b64().encode(offer.secret),
            offer.scope.as_str(),
            offer.expires_at
        )
    }

    /// `pair.claim`: checks the secret, asks the person, stores the device.
    pub(crate) fn claim(&self, params: &Value, peer: IpAddr, desktop: &DesktopInfo) -> Result<Value, Refused> {
        self.count_claim(peer)?;
        // the QR's secret, or the six words typed from the screen (the secret they stand for)
        let secret = match params.get("words").and_then(Value::as_str) {
            Some(typed) => words_secret(typed)
                .ok_or_else(|| Refused("Those aren't the six words. Check them against the computer.".into()))?
                .to_vec(),
            None => params
                .get("secret")
                .and_then(Value::as_str)
                .and_then(|s| b64().decode(s).ok())
                .ok_or_else(|| Refused("The pairing code is missing.".into()))?,
        };
        let offer = {
            let mut offers = self.offers.lock();
            let now = unix_now();
            offers.retain(|o| o.expires_at > now);
            let at = offers.iter().position(|o| o.secret[..] == secret[..]).ok_or_else(|| {
                Refused("This pairing code has expired or was already used. Show a new one on the computer.".into())
            })?;
            offers.remove(at)
        };
        let source = self.sources.lock().remove(&offer.secret);
        let device = params.get("device").cloned().unwrap_or(Value::Null);
        let field = |k: &str| device.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let key: [u8; 32] = params
            .get("public_key")
            .and_then(Value::as_str)
            .and_then(|s| b64().decode(s).ok())
            .and_then(|k| k.try_into().ok())
            .ok_or_else(|| Refused("The phone's key is missing.".into()))?;
        VerifyingKey::from_bytes(&key).map_err(|_| Refused("The phone's key isn't valid.".into()))?;
        let name = Some(field("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| "A phone".into());
        let request = DeviceRequest {
            name: name.clone(),
            model: field("model"),
            platform: field("platform"),
            app_version: field("app_version"),
            scope: offer.scope,
            peer,
            source: source.clone(),
        };
        if !(self.confirm)(&request) {
            return Err(Refused(format!("{} wasn't allowed on the computer.", request.name)));
        }
        let id = Some(field("id")).filter(|i| !i.is_empty()).unwrap_or_else(|| crate::random_b64(12));
        let entry = Device {
            id: id.clone(),
            name,
            model: request.model,
            platform: request.platform,
            app_version: request.app_version,
            public_key: key,
            scope: offer.scope,
            paired_at: unix_now(),
            last_seen: Some(unix_now()),
            source: source.clone(),
        };
        {
            let mut d = self.devices.lock();
            // pairing again replaces the device's old key
            d.retain(|x| x.id != id);
            d.push(entry);
            (self.on_change)(&d);
        }
        Ok(json!({
            "device_id": id,
            "scope": offer.scope.as_str(),
            "source": source,
            "desktop": {"id": desktop.id, "name": desktop.name, "version": desktop.version},
        }))
    }

    fn count_claim(&self, peer: IpAddr) -> Result<(), Refused> {
        let mut claims = self.claims.lock();
        let (times, locked) = claims.entry(peer).or_default();
        let now = Instant::now();
        if let Some(until) = *locked {
            if now < until {
                return Err(Refused("Too many pairing attempts. Try again in 15 minutes.".into()));
            }
            *locked = None;
        }
        times.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        times.push(now);
        if times.len() > CLAIMS_PER_MINUTE {
            *locked = Some(now + Duration::from_secs(15 * 60));
            return Err(Refused("Too many pairing attempts. Try again in 15 minutes.".into()));
        }
        Ok(())
    }

    /// A one-use nonce for the next connection's signature.
    pub(crate) fn challenge(&self) -> String {
        let nonce = crate::random_b64(18).replace(['+', '/', '='], "");
        let mut n = self.nonces.lock();
        n.retain(|(_, at)| at.elapsed() < CHALLENGE_FOR);
        n.push((nonce.clone(), Instant::now()));
        nonce
    }

    /// Checks a `Lightair-Key <device>:<nonce>:<unix seconds>:<signature>` header.
    pub(crate) fn authenticate(&self, header: &str) -> Result<Device, Refused> {
        let bad = || Refused("This phone isn't paired with this computer. Pair again.".into());
        let value = header.strip_prefix("Lightair-Key ").ok_or_else(bad)?;
        let mut parts = value.splitn(4, ':');
        let (Some(id), Some(nonce), Some(time), Some(sig)) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let time: u64 = time.parse().map_err(|_| bad())?;
        if unix_now().abs_diff(time) > CHALLENGE_FOR.as_secs() {
            return Err(Refused(
                "This phone's clock is off by more than a minute. Set it automatically and try again.".into(),
            ));
        }
        {
            let mut n = self.nonces.lock();
            n.retain(|(_, at)| at.elapsed() < CHALLENGE_FOR);
            let at = n.iter().position(|(x, _)| x == nonce).ok_or_else(bad)?;
            n.remove(at);
        }
        let sig: [u8; 64] = b64().decode(sig).ok().and_then(|s| s.try_into().ok()).ok_or_else(bad)?;
        let mut devices = self.devices.lock();
        let device = devices.iter_mut().find(|d| d.id == id).ok_or_else(bad)?;
        let key = VerifyingKey::from_bytes(&device.public_key).map_err(|_| bad())?;
        key.verify(format!("{nonce}:{id}:{time}").as_bytes(), &Signature::from_bytes(&sig)).map_err(|_| bad())?;
        device.last_seen = Some(unix_now());
        Ok(device.clone())
    }
}

/// What a phone sends to authenticate: `Lightair-Key <device>:<nonce>:<time>:<signature>`. For
/// clients and tests; the phone apps build the same string.
pub fn sign_header(device_id: &str, nonce: &str, time: u64, sign: impl Fn(&[u8]) -> [u8; 64]) -> String {
    let sig = sign(format!("{nonce}:{device_id}:{time}").as_bytes());
    format!("Lightair-Key {device_id}:{nonce}:{time}:{}", b64().encode(sig))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn desktop() -> DesktopInfo {
        DesktopInfo { id: "7f3c".into(), name: "Studio Mac".into(), version: "0.4.0".into() }
    }

    fn claim_params(secret: &[u8], key: &SigningKey) -> Value {
        json!({
            "secret": b64().encode(secret),
            "device": {"id": "phone-1", "name": "Sam's iPhone", "model": "iPhone17,1", "platform": "iOS 27", "app_version": "0.1.0"},
            "public_key": b64().encode(key.verifying_key().to_bytes()),
        })
    }

    #[test]
    fn pairs_once_with_consent_then_authenticates_signatures() {
        let allow = Arc::new(AtomicBool::new(false));
        let a = allow.clone();
        let saved = Arc::new(Mutex::new(String::new()));
        let s = saved.clone();
        let p = Pairing::new(vec![], move |_| a.load(Ordering::Relaxed), move |d| *s.lock() = devices_to_json(d));
        let key = SigningKey::from_bytes(&[7; 32]);
        let peer: IpAddr = "192.168.1.30".parse().unwrap();

        let offer = p.offer(Scope::Control);
        assert!(p.open());
        let link = p.link(&offer, &desktop(), &["192.168.1.20".into()], 4456, "AAAA");
        assert!(link.starts_with("https://lightair.app/pair#v=1&id=7f3c&n=Studio%20Mac&a=192.168.1.20&p=4456&k="));

        // the person says no: refused, and the code is spent
        assert!(p.claim(&claim_params(&offer.secret, &key), peer, &desktop()).is_err());
        assert!(p.claim(&claim_params(&offer.secret, &key), peer, &desktop()).unwrap_err().0.contains("expired"));

        allow.store(true, Ordering::Relaxed);
        let offer = p.offer(Scope::Control);
        let r = p.claim(&claim_params(&offer.secret, &key), peer, &desktop()).unwrap();
        assert_eq!(r["device_id"], "phone-1");
        assert_eq!(r["desktop"]["name"], "Studio Mac");
        assert_eq!(devices_from_json(&saved.lock()).len(), 1);

        let nonce = p.challenge();
        let header = sign_header("phone-1", &nonce, unix_now(), |m| key.sign(m).to_bytes());
        assert_eq!(p.authenticate(&header).unwrap().name, "Sam's iPhone");
        // a nonce works once
        assert!(p.authenticate(&header).is_err());
        // another key doesn't
        let other = SigningKey::from_bytes(&[9; 32]);
        let nonce = p.challenge();
        assert!(p.authenticate(&sign_header("phone-1", &nonce, unix_now(), |m| other.sign(m).to_bytes())).is_err());
        // a removed device doesn't
        assert!(p.remove("phone-1"));
        let nonce = p.challenge();
        assert!(p.authenticate(&sign_header("phone-1", &nonce, unix_now(), |m| key.sign(m).to_bytes())).is_err());
    }

    #[test]
    fn locks_out_after_too_many_claims() {
        let p = Pairing::new(vec![], |_| true, |_| {});
        let peer: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..CLAIMS_PER_MINUTE {
            let _ = p.claim(&json!({"secret": "AAAA"}), peer, &desktop());
        }
        assert!(p.claim(&json!({"secret": "AAAA"}), peer, &desktop()).unwrap_err().0.contains("15 minutes"));
    }

    #[test]
    fn watch_is_read_only() {
        assert!(Scope::Watch.allows("state.get"));
        assert!(!Scope::Watch.allows("scene.switch"));
        assert!(!Scope::Watch.allows("chat.send"));
        assert!(Scope::Control.allows("scene.switch"));
    }
}

// ---------------------------------------------------------------- words to type

/// Consonants and vowels of the words: each word is consonant, vowel, consonant, vowel (16 × 4
/// × 16 × 2 = 2,048 words, 11 bits each), easy to say and type, made here rather than taken
/// from a list (so the phones make the same words with the same few lines).
const CONSONANTS: &[u8; 16] = b"bdfghjklmnprstvz";
const VOWELS: &[u8; 4] = b"aiou";
const ENDS: &[u8; 2] = b"ao";

/// Word number `n` (0–2047).
pub fn word(n: u16) -> String {
    let n = n as usize & 0x7FF;
    let c1 = CONSONANTS[n >> 7];
    let v1 = VOWELS[(n >> 5) & 0x03];
    let c2 = CONSONANTS[(n >> 1) & 0x0F];
    let v2 = ENDS[n & 0x01];
    String::from_utf8(vec![c1, v1, c2, v2]).expect("ASCII")
}

/// A word's number, any case.
pub fn word_number(w: &str) -> Option<u16> {
    let b = w.trim().to_ascii_lowercase().into_bytes();
    if b.len() != 4 {
        return None;
    }
    let c1 = CONSONANTS.iter().position(|c| *c == b[0])?;
    let v1 = VOWELS.iter().position(|c| *c == b[1])?;
    let c2 = CONSONANTS.iter().position(|c| *c == b[2])?;
    let v2 = ENDS.iter().position(|c| *c == b[3])?;
    Some(((c1 << 7) | (v1 << 5) | (c2 << 1) | v2) as u16)
}

/// `count` words from the leading bits of `bytes`, 11 bits each.
pub fn words_of(bytes: &[u8], count: usize) -> Vec<String> {
    let bit = |i: usize| (bytes.get(i / 8).copied().unwrap_or(0) >> (7 - i % 8)) & 1;
    (0..count).map(|k| word((0..11).fold(0u16, |n, j| (n << 1) | bit(k * 11 + j) as u16))).collect()
}

/// The secret six typed words stand for (`None` unless they're six of the words).
pub fn words_secret(typed: &str) -> Option<[u8; 16]> {
    use sha2::{Digest, Sha256};
    let numbers: Vec<u16> = typed
        .split(|c: char| c.is_whitespace() || c == '-' || c == ',')
        .filter(|w| !w.is_empty())
        .map(word_number)
        .collect::<Option<_>>()?;
    if numbers.len() != 6 {
        return None;
    }
    let mut h = Sha256::new();
    h.update(b"lightair pairing words\0");
    for n in numbers {
        h.update(n.to_be_bytes());
    }
    Some(h.finalize()[..16].try_into().expect("16 bytes"))
}

/// A certificate fingerprint as four words, to compare on both screens when pairing by typed
/// words (the phone takes the fingerprint on first use then).
pub fn fingerprint_words(fingerprint: &[u8; 32]) -> [String; 4] {
    words_of(fingerprint, 4).try_into().expect("four words")
}

#[cfg(test)]
mod word_tests {
    use super::*;

    #[test]
    fn words_go_both_ways_and_stand_for_a_secret() {
        let all: std::collections::HashSet<String> = (0..2048).map(word).collect();
        assert_eq!(all.len(), 2048, "every number its own word");
        for n in [0u16, 1, 777, 2047] {
            assert_eq!(word_number(&word(n)), Some(n));
        }
        assert_eq!(word(0), "baba");
        assert_eq!(word_number("TASA"), word_number("tasa"));
        let six = words_of(&[0xAB; 9], 6).join(" ");
        assert_eq!(words_secret(&six), words_secret(&six.to_uppercase().replace(' ', "-")));
        assert!(words_secret("baba baba").is_none());
        assert!(words_secret("baba baba baba baba baba xxxx").is_none());
    }

    #[test]
    fn a_phone_pairs_with_typed_words() {
        let pairing = Pairing::new(vec![], |_| true, |_| {});
        let (_, words) = pairing.offer_words(Scope::Control);
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let desktop = DesktopInfo { id: "d".into(), name: "Studio".into(), version: "1".into() };
        let claim = |w: &str| {
            serde_json::json!({"words": w, "device": {"id": "p", "name": "Sam's iPhone"},
                "public_key": b64().encode(key.verifying_key().to_bytes())})
        };
        let peer: IpAddr = "192.168.1.9".parse().unwrap();
        assert!(pairing.claim(&claim("baba baba baba baba baba baba"), peer, &desktop).is_err());
        let ok = pairing.claim(&claim(&words.join(" ")), peer, &desktop).expect("paired by words");
        assert_eq!(ok["device_id"], "p");
        // once only
        assert!(pairing.claim(&claim(&words.join(" ")), peer, &desktop).is_err());
    }
}
