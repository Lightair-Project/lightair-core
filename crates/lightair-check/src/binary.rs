//! What a helper's programs and libraries use: their imports and linked libraries (Mach-O, PE,
//! ELF) and, on macOS, the entitlements in their code signature.

use goblin::Object;
use goblin::mach::{Mach, MachO};

/// What a binary was found to use.
#[derive(Debug, Default, Clone)]
pub struct Binary {
    /// Libraries and frameworks it links.
    pub libraries: Vec<String>,
    /// Functions it imports.
    pub imports: Vec<String>,
    /// Entitlements in its code signature (macOS), as `key` names.
    pub entitlements: Vec<String>,
    /// Whether it carries a code signature (macOS), and whether that signature has an identity
    /// (a CMS blob) rather than being ad hoc.
    pub signed: Option<bool>,
}

/// Reads a binary, or says why it can't.
pub fn read(bytes: &[u8]) -> Result<Binary, String> {
    match Object::parse(bytes).map_err(|e| e.to_string())? {
        Object::Mach(Mach::Binary(m)) => Ok(macho(&m, bytes)),
        Object::Mach(Mach::Fat(fat)) => {
            let mut out = Binary::default();
            for arch in fat.iter_arches().flatten() {
                let start = arch.offset as usize;
                let end = start.saturating_add(arch.size as usize).min(bytes.len());
                let Some(slice) = bytes.get(start..end) else { continue };
                if let Ok(m) = MachO::parse(slice, 0) {
                    let b = macho(&m, slice);
                    out.libraries.extend(b.libraries);
                    out.imports.extend(b.imports);
                    out.entitlements.extend(b.entitlements);
                    out.signed = match (out.signed, b.signed) {
                        (None, s) => s,
                        (Some(a), Some(b)) => Some(a && b),
                        (s, None) => s,
                    };
                }
            }
            out.libraries.sort();
            out.libraries.dedup();
            out.imports.sort();
            out.imports.dedup();
            out.entitlements.sort();
            out.entitlements.dedup();
            Ok(out)
        }
        Object::PE(pe) => Ok(Binary {
            libraries: pe.libraries.iter().map(|l| l.to_ascii_lowercase()).collect(),
            imports: pe.imports.iter().map(|i| i.name.to_string()).collect(),
            ..Binary::default()
        }),
        Object::Elf(elf) => Ok(Binary {
            libraries: elf.libraries.iter().map(|l| l.to_string()).collect(),
            imports: elf
                .dynsyms
                .iter()
                .filter(|s| s.is_import())
                .filter_map(|s| elf.dynstrtab.get_at(s.st_name).map(str::to_string))
                .collect(),
            ..Binary::default()
        }),
        _ => Err("isn't a program or library Lightair can read".into()),
    }
}

fn macho(m: &MachO<'_>, bytes: &[u8]) -> Binary {
    let mut out = Binary {
        libraries: m.libs.iter().filter(|l| **l != "self").map(|l| l.to_string()).collect(),
        imports: m
            .imports()
            .map(|v| v.into_iter().map(|i| i.name.trim_start_matches('_').to_string()).collect())
            .unwrap_or_default(),
        ..Binary::default()
    };
    for lc in &m.load_commands {
        if let goblin::mach::load_command::CommandVariant::CodeSignature(cs) = &lc.command {
            let start = cs.dataoff as usize;
            let end = start.saturating_add(cs.datasize as usize).min(bytes.len());
            if let Some(blob) = bytes.get(start..end) {
                let (ents, identity) = signature(blob);
                out.entitlements = ents;
                out.signed = Some(identity);
            }
        }
    }
    out
}

/// The entitlement keys in a code signature's superblob, and whether it holds a CMS signature.
fn signature(blob: &[u8]) -> (Vec<String>, bool) {
    let be32 = |o: usize| blob.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    if be32(0) != Some(0xfade_0cc0) {
        return (Vec::new(), false);
    }
    let count = be32(8).unwrap_or(0) as usize;
    let mut ents = Vec::new();
    let mut identity = false;
    for i in 0..count.min(64) {
        let Some(offset) = be32(12 + i * 8 + 4) else { break };
        let offset = offset as usize;
        match be32(offset) {
            Some(0xfade_7171) => {
                let len = be32(offset + 4).unwrap_or(0) as usize;
                if let Some(xml) = blob.get(offset + 8..offset + len.max(8)) {
                    ents.extend(plist_keys(&String::from_utf8_lossy(xml)));
                }
            }
            Some(0xfade_0b01) => {
                let len = be32(offset + 4).unwrap_or(0);
                identity = len > 8;
            }
            _ => {}
        }
    }
    (ents, identity)
}

/// The `<key>` names at the top of an entitlements plist whose value isn't `<false/>`.
fn plist_keys(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<key>") {
        let after = &rest[start + 5..];
        let Some(end) = after.find("</key>") else { break };
        let key = after[..end].trim().to_string();
        let value = after[end + 6..].trim_start();
        if !value.starts_with("<false/>") {
            out.push(key);
        }
        rest = &after[end + 6..];
    }
    out
}

/// Entitlements a helper may have: what runtimes like Bun need, and the sandbox itself.
pub const ALLOWED_ENTITLEMENTS: &[&str] = &[
    "com.apple.security.cs.allow-jit",
    "com.apple.security.cs.allow-unsigned-executable-memory",
    "com.apple.security.cs.disable-library-validation",
    "com.apple.security.app-sandbox",
    "com.apple.security.inherit",
    "com.apple.security.network.client",
];

/// Powers a sandboxed helper must ask Lightair for instead of using directly: (what it is, the
/// function or library names that use it).
pub const POWERS: &[(&str, &[&str])] = &[
    (
        "captures the screen",
        &[
            "CGWindowListCreateImage",
            "CGDisplayCreateImage",
            "CGDisplayStream",
            "SCStream",
            "SCShareableContent",
            "ScreenCaptureKit",
            "DuplicateOutput",
            "Windows.Graphics.Capture",
        ],
    ),
    (
        "posts keyboard or mouse input",
        &[
            "CGEventPost",
            "CGEventPostToPid",
            "CGEventCreateKeyboardEvent",
            "CGEventCreateMouseEvent",
            "SendInput",
            "keybd_event",
            "mouse_event",
        ],
    ),
    ("watches keyboard or mouse input system-wide", &["CGEventTapCreate", "SetWindowsHookExA", "SetWindowsHookExW"]),
    (
        "uses Accessibility to control other apps",
        &["AXUIElementCreateApplication", "AXUIElementPerformAction", "AXUIElementSetAttributeValue"],
    ),
    ("opens the camera or microphone", &["AVCaptureDevice", "MFEnumDeviceSources", "waveInOpen"]),
    (
        "writes into or reads other processes",
        &[
            "task_for_pid",
            "mach_vm_write",
            "ptrace",
            "WriteProcessMemory",
            "ReadProcessMemory",
            "CreateRemoteThread",
            "VirtualAllocEx",
        ],
    ),
    (
        "installs privileged helpers, login items or launch agents",
        &["SMJobBless", "SMAppService", "SMLoginItemSetEnabled", "AuthorizationExecuteWithPrivileges"],
    ),
];
