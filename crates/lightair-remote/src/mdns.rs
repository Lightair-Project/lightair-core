//! Telling phones on the network where the desktop is (docs/05 §3 in the mobile spec): the TLS
//! port advertised over mDNS/DNS-SD as `_lightair._tcp`, with a TXT record:
//!
//! | Key | Meaning |
//! |---|---|
//! | `id` | The desktop's id, to find a paired desktop whose address changed |
//! | `n` | Its display name |
//! | `v` | The remote protocol version |
//! | `fp` | The certificate fingerprint's first 4 bytes, hex: tells two desktops with one name apart; never trusted |
//! | `pair` | `1` while a pairing offer is open |
//! | `live` | `1` while streaming or recording |

use mdns_sd::{ServiceDaemon, ServiceInfo};
use parking_lot::Mutex;

use crate::pairing::DesktopInfo;

pub const SERVICE_TYPE: &str = "_lightair._tcp.local.";

pub struct Advertiser {
    daemon: ServiceDaemon,
    desktop: DesktopInfo,
    port: u16,
    fingerprint: String,
    state: Mutex<(bool, bool)>,
    fullname: Mutex<Option<String>>,
}

impl Advertiser {
    /// Starts advertising `port` (the TLS port) on every interface.
    pub fn start(desktop: DesktopInfo, port: u16, short_fingerprint: &str) -> Result<Advertiser, String> {
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        let a = Advertiser {
            daemon,
            desktop,
            port,
            fingerprint: short_fingerprint.to_owned(),
            state: Mutex::new((false, false)),
            fullname: Mutex::new(None),
        };
        a.register()?;
        Ok(a)
    }

    /// Updates `pair` and `live` (re-announced only when they change).
    pub fn update(&self, pairing: bool, live: bool) {
        let changed = {
            let mut s = self.state.lock();
            let changed = *s != (pairing, live);
            *s = (pairing, live);
            changed
        };
        if changed && let Err(e) = self.register() {
            tracing::warn!("mDNS: {e}");
        }
    }

    fn register(&self) -> Result<(), String> {
        let (pairing, live) = *self.state.lock();
        let flag = |b: bool| if b { "1" } else { "0" };
        let version = crate::secure::PROTOCOL.to_string();
        let props = [
            ("id", self.desktop.id.as_str()),
            ("n", self.desktop.name.as_str()),
            ("v", version.as_str()),
            ("fp", self.fingerprint.as_str()),
            ("pair", flag(pairing)),
            ("live", flag(live)),
        ];
        // instance names must be unique on the network: the name, then the id's start
        let short: String = self.desktop.id.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
        let instance = format!("{} ({short})", self.desktop.name);
        let host = format!("lightair-{short}.local.");
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", self.port, &props[..])
            .map_err(|e| e.to_string())?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info).map_err(|e| e.to_string())?;
        *self.fullname.lock() = Some(fullname);
        Ok(())
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Some(name) = self.fullname.lock().take() {
            let _ = self.daemon.unregister(&name);
        }
        let _ = self.daemon.shutdown();
    }
}
