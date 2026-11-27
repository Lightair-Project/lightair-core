//! **Export as package**: a set of items (scenes, widgets, looks, effect presets, automation
//! recipes, media, …), each a kind, a name and its files, written out as a package folder with
//! its manifest, and a package folder zipped into a `.airpkg`.
//!
//! The app turns what the user selected into [`ExportItem`]s (it knows the formats; this crate
//! doesn't read them). Each item becomes one content entry, in `<kind folder>/<slug of its
//! name>/` (`widgets/neon-lower-third/`, `looks/warm-grade/`, …). Conventions for an item's
//! files, so other Lightairs read them back the same way:
//!
//! | Kind | Files |
//! |---|---|
//! | `widget` | `index.html`, `style.css`, `main.js`, `fields.json` ([`ExportItem::widget`]) |
//! | `recipe` | `automation.json`: one automation as the automations store writes it |
//! | `template` | `scene.json` and the media it uses beside it |
//! | `look`, `effect-preset`, `transition`, `theme` | `<kind>.json` and any media it uses |
//! | `media`, `sound`, `font`, `lut` | the files themselves |

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use semver::Version;

use crate::error::{Error, Problem, Result};
use crate::hash::{CopyError, Sha256, copy_hashing};
use crate::manifest::{Content, Kind, Manifest, PackageId, Platform, Range};
use crate::package::Package;
use crate::paths::{clean_path, join, slug, unique_name};

/// The package-level details of an export, filled in by the user in the Export sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSpec {
    pub id: PackageId,
    pub name: String,
    pub version: Version,
    pub author: String,
    pub licence: String,
    pub description: String,
    /// Where it works; empty for everywhere (the usual case for data packages).
    pub platforms: Vec<Platform>,
    /// Other packages the exported items come from or need.
    pub dependencies: BTreeMap<PackageId, Range>,
}

impl ExportSpec {
    /// A spec with the defaults the Export sheet starts from: id `local.<slug of name>`,
    /// version 1.0.0, every platform, no dependencies.
    pub fn new(name: &str) -> ExportSpec {
        ExportSpec {
            id: PackageId::local(name),
            name: name.trim().to_string(),
            version: Version::new(1, 0, 0),
            author: String::new(),
            licence: String::new(),
            description: String::new(),
            platforms: Vec::new(),
            dependencies: BTreeMap::new(),
        }
    }
}

/// Where an exported file's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportData {
    /// Copied from a file on disk (media, a widget folder's files, …).
    Copy(PathBuf),
    /// Written as given (JSON the app serialised).
    Bytes(Vec<u8>),
}

/// One file of an item, by its `/`-separated path inside the item's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFile {
    pub path: String,
    pub data: ExportData,
}

/// One thing to export: a kind, a name (its folder is named after it) and its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportItem {
    pub kind: Kind,
    pub name: String,
    pub files: Vec<ExportFile>,
}

impl ExportItem {
    pub fn new(kind: Kind, name: &str) -> ExportItem {
        ExportItem { kind, name: name.to_string(), files: Vec::new() }
    }

    /// Adds a file copied from disk.
    pub fn copy(mut self, path: &str, from: &Path) -> ExportItem {
        self.files.push(ExportFile { path: path.into(), data: ExportData::Copy(from.into()) });
        self
    }

    /// Adds a file with the given bytes.
    pub fn bytes(mut self, path: &str, bytes: Vec<u8>) -> ExportItem {
        self.files.push(ExportFile { path: path.into(), data: ExportData::Bytes(bytes) });
        self
    }

    /// Adds `value` as a pretty-printed JSON file.
    pub fn json(self, path: &str, value: &serde_json::Value) -> ExportItem {
        let mut bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
        bytes.push(b'\n');
        self.bytes(path, bytes)
    }

    /// A widget from its code, in the files a widget folder is read from.
    pub fn widget(name: &str, html: &str, css: &str, js: &str, fields: &str) -> ExportItem {
        ExportItem::new(Kind::Widget, name)
            .bytes("index.html", html.as_bytes().to_vec())
            .bytes("style.css", css.as_bytes().to_vec())
            .bytes("main.js", js.as_bytes().to_vec())
            .bytes("fields.json", fields.as_bytes().to_vec())
    }
}

/// Writes `items` as a package folder at `dest` (which must not exist, or be empty) with a
/// manifest made from `spec`, then reads it back as a [`Package`] (checked like any other, with
/// its checksums). On failure nothing is left in `dest`.
///
/// Refused: no items, an item with no files, a kind this Lightair doesn't install, a file path
/// that leaves its item's folder, the same file twice, or a file to copy that isn't there.
pub fn write_folder(spec: &ExportSpec, items: &[ExportItem], dest: &Path) -> Result<Package> {
    let mut problems = Vec::new();
    if spec.name.trim().is_empty() {
        problems.push(Problem::MissingField("name"));
    }
    if items.is_empty() {
        problems.push(Problem::NoContents);
    }

    // Each item's folder, and its files' clean paths.
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut plan: Vec<(Content, Vec<(String, &ExportData)>)> = Vec::new();
    for item in items {
        let base = format!("{}/{}", item.kind.folder(), slug(&item.name));
        let mut folder = base.clone();
        let mut n = 2;
        while !taken.insert(folder.to_lowercase()) {
            folder = format!("{base}-{n}");
            n += 1;
        }
        if !item.kind.is_supported() {
            problems.push(match item.kind.tier() {
                None => Problem::UnknownKind { kind: item.kind.to_string(), path: folder.clone() },
                Some(_) => Problem::NeedsNewerKind { kind: item.kind.to_string(), path: folder.clone() },
            });
        }
        if item.files.is_empty() {
            problems.push(Problem::BadPath { path: folder.clone(), reason: "has no files".into() });
        }
        let mut seen = BTreeSet::new();
        let mut files = Vec::new();
        for f in &item.files {
            match clean_path(&f.path, 300, 16) {
                Ok(rel) => {
                    if !seen.insert(rel.to_lowercase()) {
                        problems.push(Problem::DuplicateContent(format!("{folder}/{rel}")));
                    }
                    if let ExportData::Copy(src) = &f.data
                        && !src.is_file()
                    {
                        problems.push(Problem::BadPath {
                            path: src.display().to_string(),
                            reason: "isn't a file that exists".into(),
                        });
                    }
                    files.push((format!("{folder}/{rel}"), &f.data));
                }
                Err(reason) => problems.push(Problem::BadPath { path: f.path.clone(), reason }),
            }
        }
        plan.push((Content::new(item.kind.clone(), folder), files));
    }

    let manifest = Manifest {
        id: spec.id.clone(),
        name: spec.name.trim().to_string(),
        version: spec.version.clone(),
        lightair: Range::this_api(),
        author: spec.author.trim().to_string(),
        licence: spec.licence.trim().to_string(),
        description: spec.description.trim().to_string(),
        platforms: spec.platforms.clone(),
        dependencies: spec.dependencies.clone(),
        contents: plan.iter().map(|(c, _)| c.clone()).collect(),
        permissions: Vec::new(),
    };
    if manifest.dependencies.contains_key(&manifest.id) {
        problems.push(Problem::DependsOnItself);
    }
    if !problems.is_empty() {
        return Err(Error::Invalid { problems });
    }

    let existed = dest.exists();
    if existed && fs::read_dir(dest).map_err(|e| Error::io("read the folder", dest, e))?.next().is_some() {
        return Err(Error::NotEmpty(dest.to_path_buf()));
    }
    let result = (|| {
        fs::create_dir_all(dest).map_err(|e| Error::io("make the folder", dest, e))?;
        for (_, files) in &plan {
            for (rel, data) in files {
                let out = join(dest, rel);
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent).map_err(|e| Error::io("make the folder", parent, e))?;
                }
                match data {
                    ExportData::Copy(src) => {
                        fs::copy(src, &out).map_err(|e| Error::io("copy", src, e))?;
                    }
                    ExportData::Bytes(b) => {
                        fs::write(&out, b).map_err(|e| Error::io("write", &out, e))?;
                    }
                }
            }
        }
        let manifest_path = dest.join(crate::MANIFEST_FILE);
        fs::write(&manifest_path, manifest.to_json()).map_err(|e| Error::io("write", &manifest_path, e))?;
        Package::open(dest)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(dest);
        if existed {
            let _ = fs::create_dir_all(dest);
        }
    }
    result
}

/// Extensions of files that are compressed already, stored in a `.airpkg` as they are.
const STORED: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "heic", "mp4", "mov", "m4v", "webm", "mkv", "mp3", "m4a", "aac",
    "ogg", "opus", "flac", "zip", "airpkg", "lwidget", "woff", "woff2", "gz", "7z",
];

/// Zips the package folder `folder` into a `.airpkg` at `out` (replacing it), the package's files
/// at the top of the zip, and returns the file's SHA-256. The folder is read and checked as a
/// package first, and each file is checked again as it's written. The file appears at `out`
/// only when complete.
pub fn write_archive(folder: &Path, out: &Path) -> Result<Sha256> {
    write_archive_with(folder, out, crate::Limits::default())
}

/// [`write_archive`] with other limits ([`Limits::every_tier`](crate::Limits::every_tier) for
/// authors' tools packing scripts and helpers).
pub fn write_archive_with(folder: &Path, out: &Path, limits: crate::Limits) -> Result<Sha256> {
    let package = Package::open_with(folder, limits)?;
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(unique_name(".airpkg-"));
    let result = (|| {
        let file = File::create(&tmp).map_err(|e| Error::io("write", &tmp, e))?;
        let mut zip = zip::ZipWriter::new(BufWriter::new(file));
        for (rel, entry) in &package.files {
            let ext = rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
            let method = if STORED.contains(&ext.as_str()) {
                zip::CompressionMethod::Stored
            } else {
                zip::CompressionMethod::Deflated
            };
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(method)
                .unix_permissions(0o644)
                .large_file(entry.size >= u32::MAX as u64);
            zip.start_file(rel.as_str(), options).map_err(|e| Error::Damaged(e.to_string()))?;
            let src = join(folder, rel);
            let reader = File::open(&src).map_err(|e| Error::io("read", &src, e))?;
            match copy_hashing(reader, Some(&mut zip as &mut dyn Write), entry.size) {
                Ok((sha, size)) if sha == entry.sha256 && size == entry.size => {}
                Ok(_) | Err(CopyError::TooBig) => {
                    return Err(Error::ChangedDuringInstall { path: rel.clone() });
                }
                Err(CopyError::Read(e)) => return Err(Error::io("read", &src, e)),
                Err(CopyError::Write(e)) => return Err(Error::io("write", &tmp, e)),
            }
        }
        let mut writer = zip.finish().map_err(|e| Error::Damaged(e.to_string()))?;
        writer.flush().map_err(|e| Error::io("write", &tmp, e))?;
        drop(writer);
        let sha = Sha256::of_reader(File::open(&tmp).map_err(|e| Error::io("read", &tmp, e))?)
            .map_err(|e| Error::io("read", &tmp, e))?;
        fs::rename(&tmp, out).map_err(|e| Error::io("write", out, e))?;
        Ok(sha)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// [`write_folder`] then [`write_archive`]: exports `items` straight to a `.airpkg` at `out`,
/// through a hidden folder beside it that's removed afterwards. Returns the manifest written and
/// the file's SHA-256.
pub fn write_package_file(spec: &ExportSpec, items: &[ExportItem], out: &Path) -> Result<(Manifest, Sha256)> {
    let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let folder = dir.join(unique_name(".airpkg-folder-"));
    let result =
        write_folder(spec, items, &folder).and_then(|package| Ok((package.manifest, write_archive(&folder, out)?)));
    let _ = fs::remove_dir_all(&folder);
    result
}
