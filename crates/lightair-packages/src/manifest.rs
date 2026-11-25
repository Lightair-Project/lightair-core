//! The manifest, `lightair-package.json`: its types, parsing, structural checks and the check
//! that this Lightair can use the package.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use semver::{Op, Version, VersionReq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Problem, Result};
use crate::paths::clean_path;

/// The package API this Lightair implements. A package's `lightair` range must match it.
pub const PACKAGE_API: &str = "1.0.0";

/// [`PACKAGE_API`] as a version.
pub fn package_api() -> Version {
    Version::parse(PACKAGE_API).expect("PACKAGE_API is a valid version")
}

/// The highest tier this Lightair installs: data and shaders.
const SUPPORTED_TIER: Tier = Tier::Shader;

/// The longest a package's name may be, in characters.
const MAX_NAME: usize = 100;

/// The longest a package id may be.
const MAX_ID: usize = 128;

/// A package id: a reverse-domain name (`com.example.neon-pack`), at least two parts separated by
/// dots, each of lower-case ASCII letters, digits, hyphens and underscores, starting with a
/// letter or digit. Unique, and never reused for a different package. Also used as a folder
/// name in the store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackageId(String);

impl PackageId {
    /// `id` if it's a valid package id.
    pub fn new(id: &str) -> Result<PackageId, Problem> {
        let parts: Vec<&str> = id.split('.').collect();
        let part_ok = |p: &&str| {
            !p.is_empty()
                && p.as_bytes()[0].is_ascii_alphanumeric()
                && !p.ends_with('-')
                && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        };
        if id.len() > MAX_ID || parts.len() < 2 || !parts.iter().all(part_ok) {
            return Err(Problem::BadId(id.to_string()));
        }
        Ok(PackageId(id.to_string()))
    }

    /// An id for a package made on this computer from `name`: `local.<slug of name>`.
    pub fn local(name: &str) -> PackageId {
        PackageId(format!("local.{}", crate::slug(name)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PackageId {
    type Err = Problem;

    fn from_str(s: &str) -> Result<PackageId, Problem> {
        PackageId::new(s)
    }
}

impl Serialize for PackageId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PackageId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<PackageId, D::Error> {
        let text = String::deserialize(d)?;
        PackageId::new(&text).map_err(serde::de::Error::custom)
    }
}

/// A semver version range, kept as it was written so saving it back doesn't change it.
///
/// Both forms are accepted: comma-separated (`>=1.0, <2`) and space-separated (`>=1.0 <2`).
/// Alternatives (`||`) and hyphen ranges (`1.0 - 2.0`) aren't.
/// The operators are Cargo's: `^1.2` and a bare `1.2` allow anything compatible (up to 2.0),
/// `~1.2` up to 1.3, `=1.2.0` only that version, and `*` any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    text: String,
    req: VersionReq,
}

impl Range {
    /// `text` as a range, or why it isn't one.
    pub fn parse(text: &str) -> Result<Range, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("it's empty".into());
        }
        if t.contains("||") {
            return Err("alternatives (||) aren't supported".into());
        }
        let tokens: Vec<&str> = t.split_whitespace().collect();
        if tokens.contains(&"-") {
            return Err("hyphen ranges aren't supported; write >=1.0 <=2.0 instead".into());
        }
        let normalised = if t.contains(',') {
            t.to_string()
        } else {
            // Space-separated comparators: an operator written apart from its version joins it.
            let mut parts: Vec<String> = Vec::new();
            let mut pending = String::new();
            for tok in tokens {
                if tok.chars().all(|c| "<>=^~".contains(c)) {
                    pending.push_str(tok);
                } else {
                    parts.push(format!("{pending}{tok}"));
                    pending.clear();
                }
            }
            if !pending.is_empty() {
                parts.push(pending);
            }
            parts.join(", ")
        };
        let req = VersionReq::parse(&normalised).map_err(|e| e.to_string())?;
        Ok(Range { text: t.to_string(), req })
    }

    /// `^version`: this version or any later one with the same major version (minor, for 0.x).
    pub fn caret(version: &Version) -> Range {
        Range::parse(&format!("^{version}")).expect("a caret range of a version parses")
    }

    /// `=version`: exactly this version.
    pub fn exact(version: &Version) -> Range {
        Range::parse(&format!("={version}")).expect("an exact range of a version parses")
    }

    /// The package API range a package made by this Lightair declares: `>=1.0 <2` for API 1.x.
    pub fn this_api() -> Range {
        let api = package_api();
        Range::parse(&format!(">={}.{} <{}", api.major, api.minor, api.major + 1)).expect("the API range parses")
    }

    /// Any version.
    pub fn any() -> Range {
        Range { text: "*".into(), req: VersionReq::STAR }
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.req.matches(version)
    }

    /// The parsed requirement.
    pub fn req(&self) -> &VersionReq {
        &self.req
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether every version this range allows is above `version` (so something older than the
    /// range was offered: "needs a newer …").
    fn is_above(&self, version: &Version) -> bool {
        self.req.comparators.iter().any(|c| {
            let floor = Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0));
            match c.op {
                Op::Greater => floor >= *version,
                Op::GreaterEq | Op::Exact | Op::Tilde | Op::Caret | Op::Wildcard => floor > *version,
                _ => false,
            }
        })
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for Range {
    type Err = String;

    fn from_str(s: &str) -> Result<Range, String> {
        Range::parse(s)
    }
}

impl Serialize for Range {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Range {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Range, D::Error> {
        let text = String::deserialize(d)?;
        Range::parse(&text).map_err(|e| serde::de::Error::custom(format!("\"{text}\" isn't a version range: {e}")))
    }
}

/// How much a kind of content can do, and so how much Lightair asks of it. Each tier is less
/// safe and less portable than the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Files Lightair already reads; nothing runs.
    Data = 1,
    /// WGSL shaders, run on the GPU inside the compositor.
    Shader = 2,
    /// JavaScript, in a sandbox.
    Script = 3,
    /// WebAssembly or a helper program.
    Native = 4,
}

/// A kind of content a package adds. Unknown kinds are kept (as [`Kind::Other`]) so a package
/// made for a newer Lightair can be read and its problem explained.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Scenes with placeholder objects bound to device roles.
    Template,
    /// Effect Stack looks, makeup, stickers, grades, ambiences, lights.
    Look,
    /// Stinger media with mattes and sound, transition presets.
    Transition,
    /// A widget's folder (its page, style, script and fields).
    Widget,
    /// Automation recipes.
    Recipe,
    EffectPreset,
    Theme,
    Sound,
    Font,
    Lut,
    Avatar,
    Media,
    ShaderEffect,
    ShaderTransition,
    ShaderSource,
    /// A script package's entry.
    Extension,
    WasmEffect,
    WasmSource,
    /// A helper program.
    Helper,
    /// A kind this Lightair doesn't know, as written.
    Other(String),
}

/// Every known kind with its name in the manifest, the folder exports put it in, and its noun
/// (singular, plural).
const KINDS: &[(Kind, &str, &str, &str, &str)] = &[
    (Kind::Template, "template", "templates", "template", "templates"),
    (Kind::Look, "look", "looks", "look", "looks"),
    (Kind::Transition, "transition", "transitions", "transition", "transitions"),
    (Kind::Widget, "widget", "widgets", "widget", "widgets"),
    (Kind::Recipe, "recipe", "recipes", "automation recipe", "automation recipes"),
    (Kind::EffectPreset, "effect-preset", "effect-presets", "effect preset", "effect presets"),
    (Kind::Theme, "theme", "themes", "theme", "themes"),
    (Kind::Sound, "sound", "sounds", "sound", "sounds"),
    (Kind::Font, "font", "fonts", "font", "fonts"),
    (Kind::Lut, "lut", "luts", "LUT", "LUTs"),
    (Kind::Avatar, "avatar", "avatars", "avatar", "avatars"),
    (Kind::Media, "media", "media", "media item", "media items"),
    (Kind::ShaderEffect, "shader-effect", "effects", "shader effect", "shader effects"),
    (Kind::ShaderTransition, "shader-transition", "shader-transitions", "shader transition", "shader transitions"),
    (Kind::ShaderSource, "shader-source", "sources", "shader source", "shader sources"),
    (Kind::Extension, "extension", "extensions", "extension", "extensions"),
    (Kind::WasmEffect, "wasm-effect", "wasm-effects", "WebAssembly effect", "WebAssembly effects"),
    (Kind::WasmSource, "wasm-source", "wasm-sources", "WebAssembly source", "WebAssembly sources"),
    (Kind::Helper, "helper", "helpers", "helper program", "helper programs"),
];

impl Kind {
    /// The kind named `name` in a manifest.
    pub fn parse(name: &str) -> Kind {
        KINDS.iter().find(|k| k.1 == name).map_or_else(|| Kind::Other(name.to_string()), |k| k.0.clone())
    }

    fn entry(&self) -> Option<&'static (Kind, &'static str, &'static str, &'static str, &'static str)> {
        KINDS.iter().find(|k| &k.0 == self)
    }

    /// Its name in a manifest (`effect-preset`, …).
    pub fn as_str(&self) -> &str {
        match self {
            Kind::Other(s) => s,
            k => k.entry().map_or("", |e| e.1),
        }
    }

    /// Its tier, or `None` for an unknown kind.
    pub fn tier(&self) -> Option<Tier> {
        Some(match self {
            Kind::Template
            | Kind::Look
            | Kind::Transition
            | Kind::Widget
            | Kind::Recipe
            | Kind::EffectPreset
            | Kind::Theme
            | Kind::Sound
            | Kind::Font
            | Kind::Lut
            | Kind::Avatar
            | Kind::Media => Tier::Data,
            Kind::ShaderEffect | Kind::ShaderTransition | Kind::ShaderSource => Tier::Shader,
            Kind::Extension => Tier::Script,
            Kind::WasmEffect | Kind::WasmSource | Kind::Helper => Tier::Native,
            Kind::Other(_) => return None,
        })
    }

    /// Whether this Lightair installs content of this kind.
    pub fn is_supported(&self) -> bool {
        self.tier().is_some_and(|t| t <= SUPPORTED_TIER)
    }

    /// The top-level folder an export puts this kind's items in.
    pub fn folder(&self) -> &str {
        match self {
            Kind::Other(s) => s,
            k => k.entry().map_or("other", |e| e.2),
        }
    }

    /// "1 widget", "3 effect presets".
    pub fn count(&self, n: usize) -> String {
        let (one, many) = match self {
            Kind::Other(s) => (s.as_str(), s.as_str()),
            k => k.entry().map_or(("item", "items"), |e| (e.3, e.4)),
        };
        format!("{n} {}", if n == 1 { one } else { many })
    }

    /// Every kind this Lightair knows, in tier order.
    pub fn all() -> impl Iterator<Item = Kind> {
        KINDS.iter().map(|k| k.0.clone())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Kind, D::Error> {
        Ok(Kind::parse(&String::deserialize(d)?))
    }
}

/// A platform a package works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Windows,
    Linux,
    Ios,
    Android,
}

impl Platform {
    pub fn parse(name: &str) -> Option<Platform> {
        Some(match name {
            "macos" => Platform::Macos,
            "windows" => Platform::Windows,
            "linux" => Platform::Linux,
            "ios" => Platform::Ios,
            "android" => Platform::Android,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Macos => "macos",
            Platform::Windows => "windows",
            Platform::Linux => "linux",
            Platform::Ios => "ios",
            Platform::Android => "android",
        }
    }

    /// The platform this build runs on, if it's one packages name.
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "macos") {
            Some(Platform::Macos)
        } else if cfg!(target_os = "windows") {
            Some(Platform::Windows)
        } else if cfg!(target_os = "ios") {
            Some(Platform::Ios)
        } else if cfg!(target_os = "android") {
            Some(Platform::Android)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else {
            None
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing a package adds: a kind and a path in the package (normally a folder; a file is
/// accepted too).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    pub kind: Kind,
    pub path: String,
}

/// A package's manifest, checked for structure (see [`Manifest::from_json`]). Whether this
/// Lightair can use it is a separate check, [`Manifest::check_supported`], so an installed
/// package that a Lightair update leaves behind is still listed (switched off) rather than lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: PackageId,
    pub name: String,
    pub version: Version,
    /// The range of the package API it was made for.
    pub lightair: Range,
    pub author: String,
    pub licence: String,
    pub description: String,
    /// Where it works; empty means everywhere.
    pub platforms: Vec<Platform>,
    /// Other packages it needs, by id and range.
    pub dependencies: BTreeMap<PackageId, Range>,
    pub contents: Vec<Content>,
    /// What its code may do (scripts and native code only; data and shader packages have none).
    pub permissions: Vec<String>,
}

/// The manifest as written, every field optional, so missing and malformed fields are reported
/// together as [`Problem`]s rather than one parse error at a time.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RawManifest {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    lightair: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    author: String,
    #[serde(default, alias = "license", skip_serializing_if = "String::is_empty")]
    licence: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    contents: Vec<RawContent>,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RawContent {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    path: String,
}

/// The deepest and longest a content path may be (the same as the default
/// [`Limits`](crate::Limits)).
const PATH_LEN: usize = 400;
const PATH_DEPTH: usize = 24;

impl Manifest {
    /// Parses and checks a manifest's JSON for structure: a valid id, a name, a semver version,
    /// a valid `lightair` range, known platforms, valid dependency ids and ranges (not itself),
    /// at least one content entry, and content paths that stay inside the package and aren't
    /// listed twice. Every problem found is reported at once ([`Error::Invalid`]).
    ///
    /// Unknown kinds, kinds this Lightair doesn't install and an API range this Lightair is
    /// outside of aren't structural: [`Manifest::check_supported`] reports those.
    pub fn from_json(bytes: &[u8]) -> Result<Manifest> {
        let raw: RawManifest = serde_json::from_slice(bytes)
            .map_err(|e| Error::Invalid { problems: vec![Problem::BadJson(e.to_string())] })?;
        Manifest::from_raw(raw).map_err(|problems| Error::Invalid { problems })
    }

    fn from_raw(raw: RawManifest) -> Result<Manifest, Vec<Problem>> {
        let mut problems = Vec::new();

        let id = if raw.id.trim().is_empty() {
            problems.push(Problem::MissingField("id"));
            None
        } else {
            PackageId::new(raw.id.trim()).map_err(|p| problems.push(p)).ok()
        };

        let name = raw.name.trim().to_string();
        if name.is_empty() {
            problems.push(Problem::MissingField("name"));
        } else if name.chars().count() > MAX_NAME {
            problems.push(Problem::NameTooLong);
        }

        let version = if raw.version.trim().is_empty() {
            problems.push(Problem::MissingField("version"));
            None
        } else {
            Version::parse(raw.version.trim())
                .map_err(|e| problems.push(Problem::BadVersion { text: raw.version.clone(), reason: e.to_string() }))
                .ok()
        };

        let lightair = if raw.lightair.trim().is_empty() {
            problems.push(Problem::MissingField("lightair (the package API range it was made for)"));
            None
        } else {
            Range::parse(&raw.lightair)
                .map_err(|reason| {
                    problems.push(Problem::BadRange { what: "lightair".into(), text: raw.lightair.clone(), reason })
                })
                .ok()
        };

        let mut platforms = Vec::new();
        for p in &raw.platforms {
            match Platform::parse(p.trim()) {
                Some(p) if !platforms.contains(&p) => platforms.push(p),
                Some(_) => {}
                None => problems.push(Problem::UnknownPlatform(p.clone())),
            }
        }

        let mut dependencies = BTreeMap::new();
        for (dep, range) in &raw.dependencies {
            let dep_id = PackageId::new(dep.trim()).map_err(|p| problems.push(p)).ok();
            let dep_range = Range::parse(range)
                .map_err(|reason| {
                    problems.push(Problem::BadRange { what: format!("dependency {dep}"), text: range.clone(), reason })
                })
                .ok();
            if let (Some(dep_id), Some(dep_range)) = (dep_id, dep_range) {
                if Some(&dep_id) == id.as_ref() {
                    problems.push(Problem::DependsOnItself);
                } else {
                    dependencies.insert(dep_id, dep_range);
                }
            }
        }

        let mut contents = Vec::new();
        let mut seen = BTreeSet::new();
        if raw.contents.is_empty() {
            problems.push(Problem::NoContents);
        }
        for c in &raw.contents {
            if c.kind.trim().is_empty() {
                problems.push(Problem::MissingField("kind for every content entry"));
                continue;
            }
            match clean_path(&c.path, PATH_LEN, PATH_DEPTH) {
                Ok(path) => {
                    if !seen.insert(path.to_ascii_lowercase()) {
                        problems.push(Problem::DuplicateContent(path));
                    } else if path == crate::MANIFEST_FILE {
                        problems.push(Problem::BadPath { path, reason: "is the manifest itself".into() });
                    } else {
                        contents.push(Content { kind: Kind::parse(c.kind.trim()), path });
                    }
                }
                Err(reason) => problems.push(Problem::BadPath { path: c.path.clone(), reason }),
            }
        }

        let permissions: Vec<String> =
            raw.permissions.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();

        match (id, version, lightair) {
            (Some(id), Some(version), Some(lightair)) if problems.is_empty() => Ok(Manifest {
                id,
                name,
                version,
                lightair,
                author: raw.author.trim().to_string(),
                licence: raw.licence.trim().to_string(),
                description: raw.description.trim().to_string(),
                platforms,
                dependencies,
                contents,
                permissions,
            }),
            _ => Err(problems),
        }
    }

    /// What stops this Lightair using the package, empty if nothing: a `lightair` range that
    /// doesn't include [`PACKAGE_API`], unknown kinds, kinds of a tier this Lightair doesn't
    /// install (scripts, native code: "needs a newer Lightair"), and permissions asked for by a
    /// package with no code.
    pub fn check_supported(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        let api = package_api();
        if !self.lightair.matches(&api) {
            problems.push(Problem::ApiOutOfRange {
                range: self.lightair.to_string(),
                api: api.to_string(),
                newer: self.lightair.is_above(&api),
            });
        }
        for c in &self.contents {
            match c.kind.tier() {
                None => problems.push(Problem::UnknownKind { kind: c.kind.to_string(), path: c.path.clone() }),
                Some(t) if t > SUPPORTED_TIER => {
                    problems.push(Problem::NeedsNewerKind { kind: c.kind.to_string(), path: c.path.clone() })
                }
                Some(_) => {}
            }
        }
        if !self.permissions.is_empty() && self.highest_tier().is_some_and(|t| t <= Tier::Shader) {
            problems.push(Problem::PermissionsNotAllowed(self.permissions.clone()));
        }
        problems
    }

    /// The highest tier of its contents (`None` if any kind is unknown).
    pub fn highest_tier(&self) -> Option<Tier> {
        let mut highest = Tier::Data;
        for c in &self.contents {
            highest = highest.max(c.kind.tier()?);
        }
        Some(highest)
    }

    /// Whether it works on `platform` (a package naming no platforms works everywhere).
    pub fn supports(&self, platform: Platform) -> bool {
        self.platforms.is_empty() || self.platforms.contains(&platform)
    }

    /// How many of each kind it adds, in the manifest's order of first appearance.
    pub fn counts(&self) -> Vec<(Kind, usize)> {
        let mut out: Vec<(Kind, usize)> = Vec::new();
        for c in &self.contents {
            match out.iter_mut().find(|(k, _)| *k == c.kind) {
                Some((_, n)) => *n += 1,
                None => out.push((c.kind.clone(), 1)),
            }
        }
        out
    }

    /// A plain summary of what it adds, for the install prompt: "2 widgets, 1 transition and 1
    /// shader effect".
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self.counts().iter().map(|(k, n)| k.count(*n)).collect();
        match parts.as_slice() {
            [] => "nothing".into(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        }
    }

    /// The manifest as pretty-printed JSON, as written into a package.
    pub fn to_json(&self) -> Vec<u8> {
        let raw = RawManifest {
            id: self.id.to_string(),
            name: self.name.clone(),
            version: self.version.to_string(),
            lightair: self.lightair.to_string(),
            author: self.author.clone(),
            licence: self.licence.clone(),
            description: self.description.clone(),
            platforms: self.platforms.iter().map(|p| p.as_str().to_string()).collect(),
            dependencies: self.dependencies.iter().map(|(id, r)| (id.to_string(), r.to_string())).collect(),
            contents: self
                .contents
                .iter()
                .map(|c| RawContent { kind: c.kind.to_string(), path: c.path.clone() })
                .collect(),
            permissions: self.permissions.clone(),
        };
        let mut out = serde_json::to_vec_pretty(&raw).unwrap_or_default();
        out.push(b'\n');
        out
    }
}

impl Serialize for Manifest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let value: serde_json::Value = serde_json::from_slice(&self.to_json()).map_err(serde::ser::Error::custom)?;
        value.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Manifest, D::Error> {
        let raw = RawManifest::deserialize(d)?;
        Manifest::from_raw(raw).map_err(|problems| {
            let text: Vec<String> = problems.iter().map(|p| p.to_string()).collect();
            serde::de::Error::custom(text.join("; "))
        })
    }
}
