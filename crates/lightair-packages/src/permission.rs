//! The permissions a package can ask for (the manifest's `permissions`): what its code may reach
//! beyond its own folder. Lightair grants them one by one and shows every use.

use std::fmt;

use crate::manifest::Platform;

/// One permission, parsed from its manifest form (`network:api.example.com`, `device.midi`, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// `network:<host>`: that host (and its subdomains when written `*.example.com`).
    Network(String),
    /// `network:*`: any host.
    NetworkAny,
    /// `network:localhost:<port>`: an app's server on this computer.
    Localhost(u16),
    /// `network.lan`: devices on the home network, not the internet.
    Lan,
    /// `network.lan.discovery`: Bonjour/mDNS discovery on the home network.
    LanDiscovery,
    /// `network.listen:<port>`: listening on that port.
    Listen(u16),
    /// `ipc:<name>`: one local socket or named pipe.
    Ipc(String),
    /// `automation:<app id>`: scripting one other app.
    Automation(String),
    /// `fs.read:picked`: files or folders the user picks.
    FsReadPicked,
    /// `fs.write:picked`: writing to files or folders the user picks.
    FsWritePicked,
    /// `device.hid:<vendor id>`.
    DeviceHid(String),
    /// `device.usb:<vendor id>`.
    DeviceUsb(String),
    DeviceMidi,
    DeviceSerial,
    DeviceBluetooth,
    /// `screen.capture`, through Lightair.
    ScreenCapture,
    Camera,
    Mic,
    ChatRead,
    ChatWrite,
    /// `api.call`: Lightair's own methods (the remote API's).
    ApiCall,
    /// `computer.control`: the mouse and keyboard, through Lightair's AI drives and its gates
    /// (helpers only).
    ComputerControl,
    /// `apps.run:<app>`: one of the user's own apps, started by Lightair outside the sandbox
    /// (helpers only; Lightair decides which apps it will start).
    AppsRun(String),
    /// `speech.read`: what's said on the tracks Lightair hears, as text (helpers only).
    SpeechRead,
    /// `secrets`: its own place in the Keychain.
    Secrets,
    /// `full-access`, everywhere (`None`) or on one platform (`full-access:windows`).
    FullAccess(Option<Platform>),
    /// A script permission from the extension runtime (`project.read`, `hotkeys`, …).
    Script(String),
}

/// The script permissions of the extension runtime (docs/13, tier 3).
const SCRIPT: &[&str] =
    &["project.read", "project.write", "outputs.control", "notifications", "audio.levels", "hotkeys"];

impl Permission {
    /// `text` as a permission, or why it isn't one.
    pub fn parse(text: &str) -> Result<Permission, String> {
        let t = text.trim();
        let simple = match t {
            "network:*" => Some(Permission::NetworkAny),
            "network.lan" => Some(Permission::Lan),
            "network.lan.discovery" => Some(Permission::LanDiscovery),
            "fs.read:picked" => Some(Permission::FsReadPicked),
            "fs.write:picked" => Some(Permission::FsWritePicked),
            "device.midi" => Some(Permission::DeviceMidi),
            "device.serial" => Some(Permission::DeviceSerial),
            "device.bluetooth" => Some(Permission::DeviceBluetooth),
            "screen.capture" => Some(Permission::ScreenCapture),
            "camera" => Some(Permission::Camera),
            "mic" => Some(Permission::Mic),
            "chat.read" => Some(Permission::ChatRead),
            "chat.write" => Some(Permission::ChatWrite),
            "api.call" => Some(Permission::ApiCall),
            "computer.control" => Some(Permission::ComputerControl),
            "speech.read" => Some(Permission::SpeechRead),
            "secrets" => Some(Permission::Secrets),
            "full-access" => Some(Permission::FullAccess(None)),
            _ => None,
        };
        if let Some(p) = simple {
            return Ok(p);
        }
        if SCRIPT.contains(&t) {
            return Ok(Permission::Script(t.to_string()));
        }
        let (head, rest) = t.split_once(':').ok_or("it isn't one of the permissions Lightair knows")?;
        match head {
            "network" => {
                if let Some(port) = rest.strip_prefix("localhost:") {
                    return port_of(port).map(Permission::Localhost);
                }
                host_ok(rest)?;
                Ok(Permission::Network(rest.to_ascii_lowercase()))
            }
            "network.listen" => port_of(rest).map(Permission::Listen),
            "ipc" => name_ok(rest, "a socket or pipe name").map(|n| Permission::Ipc(n.to_string())),
            "automation" => name_ok(rest, "an app id").map(|n| Permission::Automation(n.to_string())),
            "apps.run" => name_ok(rest, "an app's name").map(|n| Permission::AppsRun(n.to_string())),
            "device.hid" => vendor_ok(rest).map(Permission::DeviceHid),
            "device.usb" => vendor_ok(rest).map(Permission::DeviceUsb),
            "full-access" => Platform::parse(rest)
                .map(|p| Permission::FullAccess(Some(p)))
                .ok_or_else(|| format!("\"{rest}\" isn't a platform (macos, windows, linux)")),
            "fs.read" | "fs.write" => Err("files are only the ones the user picks: write fs.read:picked".into()),
            _ => Err("it isn't one of the permissions Lightair knows".into()),
        }
    }

    /// Whether it's a `network:` host permission (the only kind a widget may ask for).
    pub fn is_network_host(&self) -> bool {
        matches!(self, Permission::Network(_) | Permission::NetworkAny)
    }

    /// Whether it lets a host through: `api.example.com` for `network:api.example.com`, any
    /// subdomain for `network:*.example.com`, anything for `network:*`.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match self {
            Permission::NetworkAny => true,
            Permission::Network(h) => match h.strip_prefix("*.") {
                Some(base) => host == base || host.ends_with(&format!(".{base}")),
                None => host == *h,
            },
            _ => false,
        }
    }

    /// Whether the user is told every time it starts being used, not only the first time
    /// (design §4.13).
    pub fn is_sensitive(&self) -> bool {
        matches!(
            self,
            Permission::ScreenCapture
                | Permission::Camera
                | Permission::Mic
                | Permission::Automation(_)
                | Permission::ComputerControl
                | Permission::AppsRun(_)
                | Permission::SpeechRead
                | Permission::FsReadPicked
                | Permission::FsWritePicked
                | Permission::FullAccess(_)
        )
    }

    /// What it lets the package do, in plain words, for the install screen.
    pub fn describe(&self) -> String {
        match self {
            Permission::Network(h) => format!("Connect to {h}"),
            Permission::NetworkAny => "Connect to any website".into(),
            Permission::Localhost(p) => format!("Talk to an app on this computer (port {p})"),
            Permission::Lan => "Talk to devices on your home network".into(),
            Permission::LanDiscovery => "Find devices on your home network".into(),
            Permission::Listen(p) => format!("Accept connections on port {p}"),
            Permission::Ipc(n) => format!("Talk to another app through \"{n}\""),
            Permission::Automation(a) => format!("Control {a}"),
            Permission::FsReadPicked => "Read files and folders you choose".into(),
            Permission::FsWritePicked => "Change files and folders you choose".into(),
            Permission::DeviceHid(v) => format!("Use control devices from vendor {v}"),
            Permission::DeviceUsb(v) => format!("Use USB devices from vendor {v}"),
            Permission::DeviceMidi => "Use MIDI devices".into(),
            Permission::DeviceSerial => "Use serial devices".into(),
            Permission::DeviceBluetooth => "Use Bluetooth devices".into(),
            Permission::ScreenCapture => "See your screen, through Lightair".into(),
            Permission::Camera => "Use your camera, through Lightair".into(),
            Permission::Mic => "Hear your microphone, through Lightair".into(),
            Permission::ChatRead => "Read your stream's chat".into(),
            Permission::ChatWrite => "Send messages to your stream's chat".into(),
            Permission::ApiCall => "Control Lightair (scenes, sources, outputs)".into(),
            Permission::ComputerControl => "Use your mouse and keyboard, when you turn on AI drives".into(),
            Permission::AppsRun(a) => format!("Run your {a} app"),
            Permission::SpeechRead => "Hear what's said on your tracks, as text".into(),
            Permission::Secrets => "Keep its own keys in your Keychain".into(),
            Permission::FullAccess(None) => {
                "Full access: runs like any app you install, outside Lightair's sandbox".into()
            }
            Permission::FullAccess(Some(p)) => {
                format!("Full access on {p}: runs like any app you install, outside Lightair's sandbox")
            }
            Permission::Script(s) => match s.as_str() {
                "project.read" => "Read your show".into(),
                "project.write" => "Change your show".into(),
                "outputs.control" => "Start and stop streaming and recording".into(),
                "notifications" => "Show notifications".into(),
                "audio.levels" => "See sound levels".into(),
                "hotkeys" => "Use keyboard shortcuts".into(),
                other => other.to_string(),
            },
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Permission::Network(h) => write!(f, "network:{h}"),
            Permission::NetworkAny => f.write_str("network:*"),
            Permission::Localhost(p) => write!(f, "network:localhost:{p}"),
            Permission::Lan => f.write_str("network.lan"),
            Permission::LanDiscovery => f.write_str("network.lan.discovery"),
            Permission::Listen(p) => write!(f, "network.listen:{p}"),
            Permission::Ipc(n) => write!(f, "ipc:{n}"),
            Permission::Automation(a) => write!(f, "automation:{a}"),
            Permission::FsReadPicked => f.write_str("fs.read:picked"),
            Permission::FsWritePicked => f.write_str("fs.write:picked"),
            Permission::DeviceHid(v) => write!(f, "device.hid:{v}"),
            Permission::DeviceUsb(v) => write!(f, "device.usb:{v}"),
            Permission::DeviceMidi => f.write_str("device.midi"),
            Permission::DeviceSerial => f.write_str("device.serial"),
            Permission::DeviceBluetooth => f.write_str("device.bluetooth"),
            Permission::ScreenCapture => f.write_str("screen.capture"),
            Permission::Camera => f.write_str("camera"),
            Permission::Mic => f.write_str("mic"),
            Permission::ChatRead => f.write_str("chat.read"),
            Permission::ChatWrite => f.write_str("chat.write"),
            Permission::ApiCall => f.write_str("api.call"),
            Permission::ComputerControl => f.write_str("computer.control"),
            Permission::AppsRun(a) => write!(f, "apps.run:{a}"),
            Permission::SpeechRead => f.write_str("speech.read"),
            Permission::Secrets => f.write_str("secrets"),
            Permission::FullAccess(None) => f.write_str("full-access"),
            Permission::FullAccess(Some(p)) => write!(f, "full-access:{p}"),
            Permission::Script(s) => f.write_str(s),
        }
    }
}

fn port_of(text: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!("\"{text}\" isn't a port number")),
    }
}

/// A host name: letters, digits, hyphens and dots, optionally starting with `*.`.
fn host_ok(host: &str) -> Result<(), String> {
    let bare = host.strip_prefix("*.").unwrap_or(host);
    let ok = !bare.is_empty()
        && bare.len() <= 253
        && bare.contains('.')
        && bare.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if ok {
        Ok(())
    } else if bare == "localhost" {
        Err("write network:localhost:<port> for an app on this computer".into())
    } else {
        Err(format!("\"{host}\" isn't a host name such as api.example.com"))
    }
}

fn name_ok<'a>(name: &'a str, what: &str) -> Result<&'a str, String> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with('.');
    if ok { Ok(name) } else { Err(format!("\"{name}\" isn't {what} (letters, digits, '.', '-', '_')")) }
}

/// A USB vendor id: four hex digits (`0fd9`).
fn vendor_ok(text: &str) -> Result<String, String> {
    let t = text.trim_start_matches("0x").to_ascii_lowercase();
    if t.len() == 4 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(t)
    } else {
        Err(format!("\"{text}\" isn't a USB vendor id (four hex digits, such as 0fd9)"))
    }
}
