//! The static checks (design §4.3), over a package opened with every tier allowed.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use lightair_packages::catalog::Target;
use lightair_packages::{Kind, Limits, Manifest, Package, Permission, Tier};
use regex::Regex;

use crate::binary;
use crate::code::{self, Reach};
use crate::filetype::{self, Family};
use crate::normalise;
use crate::report::{Check, Code, Measured, Output};

/// The most code text sent for the auditor, in all (catalog formats §8).
const MAX_CODE: usize = 512 * 1024;

/// Runs every static check on the package at `path` (a `.airpkg` or a folder). `url` is where it
/// was downloaded from, for the measured file ("" when local).
pub fn run(path: &Path, url: &str) -> Output {
    let mut out = Output::default();
    let mut archive = Check::new("archive");
    let package = match Package::open_with(path, Limits::every_tier()) {
        Ok(p) => p,
        Err(e) => {
            for p in e.problems() {
                archive.fail(p.to_string(), None);
            }
            if e.problems().is_empty() {
                archive.fail(e.to_string(), None);
            }
            out.checks.push(archive);
            return out;
        }
    };
    out.checks.push(archive.finish("The package opens safely and its manifest is valid."));
    out.files.push(measure(&package, url));

    let files = Files::read(&package);
    out.checks.push(file_check(&package, &files));
    let (perm_check, code) = permission_check(&package, &files);
    out.checks.push(perm_check);
    out.code = code;
    out.checks.push(secret_check(&files));
    out.checks.push(licence_check(&package, &files));
    out
}

/// The package file as measured: what the catalog compares with the release.
fn measure(package: &Package, url: &str) -> Measured {
    let target = package
        .manifest
        .contents
        .iter()
        .filter(|c| c.kind == Kind::Helper)
        .flat_map(|c| c.entry.iter())
        .find(|(_, program)| package.files.contains_key(*program))
        .map(|(t, _)| t.clone())
        .unwrap_or_else(|| Target::Any.as_str().to_string());
    Measured {
        platform: target,
        url: url.to_string(),
        size: std::fs::metadata(package.origin_path()).map(|m| m.len()).unwrap_or(package.size),
        archive_sha256: package.archive_sha256.map(|s| s.to_string()).unwrap_or_default(),
        content_sha256: package.content_sha256.to_string(),
        manifest: serde_json::from_slice(&package.manifest.to_json()).unwrap_or_default(),
    }
}

/// Every file's bytes and what it is.
struct Files {
    items: Vec<FileItem>,
}

struct FileItem {
    path: String,
    bytes: Vec<u8>,
    family: Family,
    /// The content entry it belongs to, if any.
    kind: Option<Kind>,
}

impl Files {
    fn read(package: &Package) -> Files {
        let mut items = Vec::new();
        for path in package.files.keys() {
            let Ok(bytes) = package.read(path) else { continue };
            let family = filetype::sniff(&bytes);
            let kind = package
                .manifest
                .contents
                .iter()
                .find(|c| *path == c.path || path.starts_with(&format!("{}/", c.path)))
                .map(|c| c.kind.clone());
            items.push(FileItem { path: path.clone(), bytes, family, kind });
        }
        Files { items }
    }
}

fn file_check(package: &Package, files: &Files) -> Check {
    let mut c = Check::new("files");
    for f in &files.items {
        let p = Some(f.path.as_str());
        if f.path == lightair_packages::MANIFEST_FILE {
            continue;
        }
        if let Some(expected) = filetype::expected(&f.path) {
            let text_ok = f.family == Family::Json && expected.contains(&Family::Text);
            if !expected.contains(&f.family) && !text_ok {
                c.fail(format!("its name says one kind of file, and its contents are {}", f.family.name()), p);
                continue;
            }
        } else if f.family == Family::Unknown {
            c.fail("is a kind of file Lightair doesn't know", p);
            continue;
        }
        match &f.kind {
            Some(kind) => {
                if !filetype::allowed(kind, f.family, &f.path) {
                    c.fail(format!("{} content can't hold {}", kind, f.family.name()), p);
                    continue;
                }
                if f.family.is_executable() && *kind != Kind::Helper {
                    c.fail(format!("is {}, and only helpers may hold programs", f.family.name()), p);
                    continue;
                }
                if f.family == Family::Wasm && !matches!(kind, Kind::WasmEffect | Kind::WasmSource | Kind::Helper) {
                    c.fail("is WebAssembly, which only WebAssembly effects and sources may hold", p);
                    continue;
                }
                if f.family == Family::Zip && filetype::extension(&f.path) != "lairava" && *kind != Kind::Helper {
                    c.fail("is an archive inside the package: add its files to the package instead", p);
                    continue;
                }
            }
            None => {
                if !filetype::allowed_at_root(f.family) {
                    c.fail(
                        format!(
                            "is {} outside every content folder (only a README, licence, icon and previews may be)",
                            f.family.name()
                        ),
                        p,
                    );
                    continue;
                }
            }
        }
        if f.family.is_image() {
            if let Err(e) = normalise::decode_check(f.family, &f.bytes) {
                c.fail(format!("can't be read as an image: {e}"), p);
                continue;
            }
            match normalise::is_normalised(f.family, &f.bytes) {
                Ok(true) => {}
                Ok(false) => c.fail(
                    "isn't normalised (it may carry metadata or extra data): build the package with lightair-pack build",
                    p,
                ),
                Err(e) => c.fail(format!("is damaged: {e}"), p),
            }
        }
        if f.family == Family::Json
            && let Err(e) = serde_json::from_slice::<serde_json::Value>(&f.bytes)
        {
            c.fail(format!("isn't valid JSON: {e}"), p);
        }
        if f.family == Family::Font
            && let Err(e) = font_ok(&f.bytes)
        {
            c.fail(e, p);
        }
        if f.family == Family::Glb
            && let Err(e) = glb_ok(&f.bytes)
        {
            c.fail(e, p);
        }
        if f.family.is_audio() || f.family.is_video() {
            c.note("is probed and partly decoded by the run, not here", p);
        }
    }
    // A helper's programs: every target named must be a program of the right platform.
    for content in package.manifest.contents.iter().filter(|c| c.kind == Kind::Helper) {
        for (target, program) in &content.entry {
            let Some(f) = files.items.iter().find(|f| f.path == *program) else { continue };
            let want = match Target::parse(target).and_then(Target::platform) {
                Some(lightair_packages::Platform::Macos) => Family::MachO,
                Some(lightair_packages::Platform::Windows) => Family::Pe,
                Some(lightair_packages::Platform::Linux) => Family::Elf,
                _ => continue,
            };
            if f.family != want && f.family != Family::Shebang {
                c.fail(format!("is the {target} program, and its contents are {}", f.family.name()), Some(&f.path));
            }
        }
    }
    c.finish("Every file is what its name says, allowed where it is, and readable.")
}

fn font_ok(bytes: &[u8]) -> Result<(), String> {
    let face = ttf_parser::Face::parse(bytes, 0).map_err(|e| format!("can't be read as a font: {e}"))?;
    let raw = face.raw_face();
    let mut end = 0usize;
    for r in raw.table_records {
        end = end.max(r.offset as usize + r.length as usize);
    }
    if !bytes.starts_with(b"ttcf") && bytes.len() > end + 3 {
        return Err("has data after its last table".into());
    }
    if face.permissions() == Some(ttf_parser::Permissions::Restricted) {
        return Err("is marked \"restricted licence embedding\": its licence doesn't allow sharing it".into());
    }
    Ok(())
}

fn glb_ok(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 12 {
        return Err("is shorter than a glTF header".into());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if version != 2 {
        return Err(format!("is glTF version {version}, and Lightair reads version 2"));
    }
    if len != bytes.len() {
        return Err("isn't the length its header says (data hidden after it, or cut short)".into());
    }
    Ok(())
}

fn permission_check(package: &Package, files: &Files) -> (Check, Vec<Code>) {
    let mut c = Check::new("permissions");
    let m = &package.manifest;
    // code may use what it declares now and what it may ask for later
    let granted: Vec<Permission> = m.parsed_permissions().into_iter().chain(m.parsed_optional()).collect();
    let mut code_out = Vec::new();
    let mut code_size = 0usize;
    let is_script = |k: &Option<Kind>| matches!(k, Some(Kind::Extension));
    let is_widget = |k: &Option<Kind>| matches!(k, Some(Kind::Widget));

    // Templates: web sources' addresses in their JSON must be declared too.
    for f in files.items.iter().filter(|f| f.kind == Some(Kind::Template) && f.family == Family::Json) {
        let mut hosts = BTreeSet::new();
        code::hosts_in(&String::from_utf8_lossy(&f.bytes), &mut hosts);
        for host in hosts {
            if !granted.iter().any(|g| g.allows_host(&host)) {
                c.fail(
                    format!("reaches {host} (a web source in the template), which the manifest doesn't declare (add network:{host})"),
                    Some(&f.path),
                );
            }
        }
    }

    for f in &files.items {
        if !(is_widget(&f.kind) || is_script(&f.kind)) || f.family != Family::Text {
            continue;
        }
        let ext = filetype::extension(&f.path);
        let Ok(text) = std::str::from_utf8(&f.bytes) else { continue };
        let mut reach = Reach::default();
        match ext.as_str() {
            "js" | "mjs" => code::scan_js(text, &mut reach),
            "html" | "htm" | "svg" => code::scan_html(text, &mut reach),
            "css" => code::scan_css(text, &mut reach),
            _ => continue,
        }
        if code_size + text.len() <= MAX_CODE {
            code_size += text.len();
            code_out.push(Code { path: f.path.clone(), text: text.to_string() });
        }
        let p = Some(f.path.as_str());
        for host in &reach.hosts {
            if !granted.iter().any(|g| g.allows_host(host)) {
                c.fail(format!("reaches {host}, which the manifest doesn't declare (add network:{host})"), p);
            }
        }
        let dynamic: BTreeSet<&String> = reach.dynamic.iter().collect();
        for d in dynamic {
            if is_script(&f.kind) {
                c.fail(format!("builds code at runtime ({d}), which scripts can't"), p);
            } else {
                c.note(format!("builds code at runtime ({d}): the run checks it longer"), p);
            }
        }
        let computed: BTreeSet<&String> = reach.computed.iter().collect();
        for d in computed {
            c.note(format!("connects with an address built at runtime ({d}): the run checks where it goes"), p);
        }
        // TODO(stage D): map calls into Lightair's script API to the permissions they need, once the
        // extension runtime defines that API.
    }

    // Helpers: their programs' powers against the permissions.
    let full = |target: Target| m.full_access_on(target);
    for content in m.contents.iter().filter(|c| c.kind == Kind::Helper) {
        let mut programs: BTreeSet<&str> = content.entry.values().map(String::as_str).collect();
        for f in files.items.iter().filter(|f| f.kind == Some(Kind::Helper) && f.family.is_executable()) {
            programs.insert(f.path.as_str());
        }
        for path in programs {
            let Some(f) = files.items.iter().find(|f| f.path == path) else { continue };
            if !matches!(f.family, Family::MachO | Family::Pe | Family::Elf) {
                continue;
            }
            let target = content
                .entry
                .iter()
                .find(|(_, prog)| prog.as_str() == path)
                .and_then(|(t, _)| Target::parse(t))
                .unwrap_or(match f.family {
                    Family::MachO => Target::MacosArm64,
                    Family::Pe => Target::WindowsX64,
                    _ => Target::LinuxX64,
                });
            let p = Some(path);
            let b = match binary::read(&f.bytes) {
                Ok(b) => b,
                Err(e) => {
                    c.fail(format!("can't be read as a program: {e}"), p);
                    continue;
                }
            };
            if full(target) {
                c.note("asks for full access here, so what it uses isn't limited: the admin reviews it", p);
                continue;
            }
            for (what, names) in binary::POWERS {
                let hit = names
                    .iter()
                    .find(|n| b.imports.iter().any(|i| i.contains(**n)) || b.libraries.iter().any(|l| l.contains(**n)));
                if let Some(name) = hit {
                    c.fail(
                        format!("{what} itself ({name}); sandboxed helpers ask Lightair instead, or need full access"),
                        p,
                    );
                }
            }
            for e in &b.entitlements {
                if !binary::ALLOWED_ENTITLEMENTS.contains(&e.as_str()) {
                    c.fail(format!("has the entitlement {e}, which a sandboxed helper can't have"), p);
                }
            }
            if target.platform() == Some(lightair_packages::Platform::Macos) && b.signed != Some(true) {
                c.note("isn't signed with a Developer ID (the catalog needs a notarized macOS helper)", p);
            }
        }
    }
    if m.highest_tier() == Some(Tier::Native) && m.contents.iter().any(|c| c.kind == Kind::Helper) {
        c.note("the helper's run in the app's sandbox happens on the macOS and Windows runners", None);
    }
    (c.finish("The code reaches only what the manifest declares."), code_out)
}

fn secret_check(files: &Files) -> Check {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            ("a private key", r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP |ENCRYPTED )?PRIVATE KEY"),
            ("an AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            ("a GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})\b"),
            ("an Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
            ("an OpenAI API key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}"),
            ("a Google API key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
            ("a Slack token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
            ("a Stripe secret key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{20,}"),
            ("a Twitch or Discord bot token", r"\b[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27,}"),
        ]
        .into_iter()
        .map(|(n, r)| (n, Regex::new(r).unwrap()))
        .collect()
    });
    let mut c = Check::new("secrets");
    for f in &files.items {
        if f.family.is_image() || f.family.is_audio() || f.family.is_video() || f.family == Family::Font {
            continue;
        }
        let text = String::from_utf8_lossy(&f.bytes);
        for (name, re) in patterns {
            if re.is_match(&text) {
                c.fail(
                    format!("holds what looks like {name}: remove it from the package and revoke it"),
                    Some(&f.path),
                );
            }
        }
    }
    c.finish("No keys or tokens were left in the package.")
}

fn licence_check(package: &Package, files: &Files) -> Check {
    let mut c = Check::new("licence");
    let m: &Manifest = &package.manifest;
    if m.licence.trim().is_empty() {
        c.fail("the manifest has no licence: give an SPDX licence such as MIT or CC-BY-4.0", None);
    } else if let Err(e) = spdx::Expression::parse(m.licence.trim()) {
        c.fail(format!("\"{}\" isn't a valid SPDX licence expression ({e})", m.licence), None);
    }
    let has_code = files.items.iter().any(|f| {
        f.family.is_executable()
            || f.family == Family::Wasm
            || (matches!(f.kind, Some(Kind::Widget | Kind::Extension | Kind::Helper))
                && matches!(filetype::extension(&f.path).as_str(), "js" | "mjs" | "html" | "htm"))
    });
    let has_licence_file = files.items.iter().any(|f| {
        let name = f.path.to_ascii_uppercase();
        !f.path.contains('/')
            && (name.starts_with("LICENSE") || name.starts_with("LICENCE") || name.starts_with("COPYING"))
    });
    if has_code && !has_licence_file {
        c.fail("the package holds code, so it needs a LICENSE (or LICENCE) file at its top", None);
    }
    let ofl = m.licence.contains("OFL");
    for f in files.items.iter().filter(|f| f.family == Family::Font) {
        if let Ok(face) = ttf_parser::Face::parse(&f.bytes, 0) {
            let mentions = face.names().into_iter().filter(|n| n.name_id == 13 || n.name_id == 14).any(|n| {
                n.to_string().is_some_and(|s| {
                    s.contains("Open Font License") || s.contains("OFL") || s.contains("scripts.sil.org")
                })
            });
            if ofl && !mentions {
                c.note(
                    "the manifest says OFL, and the font doesn't mention the Open Font License: check its licence",
                    Some(&f.path),
                );
            }
        }
    }
    c.finish("The licence is valid, with a licence file where there's code.")
}
