//! `.lwidget` files: a widget's page, style, script and fields zipped with a small
//! `manifest.json` (name, version, size, format). One installs as a package of one widget, so
//! widgets shared before packages existed keep working.
//!
//! The widget's files go into `widgets/<name>/` in the package, all in that one folder (a
//! widget is read from a flat folder of `index.html`, `style.css`, `main.js` and `fields.json`).

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;

use semver::Version;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::manifest::{Content, Kind, Manifest, PackageId, Range};
use crate::package::Package;
use crate::paths::{clean_path, is_junk_path, join, slug};
use crate::store::{Install, Source, Store};

/// The most files a `.lwidget` may hold.
const MAX_FILES: usize = 256;
/// The most a `.lwidget`'s files may come to, unpacked.
const MAX_TOTAL: u64 = 256 << 20;

/// The part of a `.lwidget`'s own `manifest.json` that's read.
#[derive(Debug, Default, Deserialize)]
struct WidgetManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

/// The id a `.lwidget` named `name` installs under: `local.widget.<slug of name>`.
pub fn package_id(name: &str) -> PackageId {
    PackageId::new(&format!("local.widget.{}", slug(name))).expect("a slug makes a valid id part")
}

/// Unpacks the `.lwidget` at `path` into `dest` (an empty folder) as a package folder: the
/// widget's files in `widgets/<slug>/` and a written `lightair-package.json`. The package takes
/// the widget's name and version from its `manifest.json` (the file's name and 1.0.0 when it
/// has none), or `version` when one is given. Returns the manifest written.
pub fn to_folder(path: &Path, dest: &Path, version: Option<Version>) -> Result<Manifest> {
    let file = File::open(path).map_err(|e| Error::io("open", path, e))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|_| Error::NotAPackage("that isn't a .lwidget (zip) file".into()))?;
    if zip.len() > MAX_FILES * 4 {
        return Err(Error::TooLarge(format!("it lists {} entries", zip.len())));
    }

    // Each file by its own name (the widget's folder is flat), the first of a name kept.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut names = BTreeSet::new();
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(|e| Error::Damaged(e.to_string()))?;
        if f.is_dir() {
            continue;
        }
        if f.is_symlink() {
            return Err(Error::Unsafe("it contains a link".into()));
        }
        let name = f.name().map_err(|e| Error::Damaged(e.to_string()))?.to_string();
        let clean = clean_path(&name, 400, 24).map_err(|reason| Error::Unsafe(format!("\"{name}\" {reason}")))?;
        if is_junk_path(&clean) {
            continue;
        }
        let base = clean.rsplit('/').next().unwrap_or(&clean).to_string();
        if !names.insert(base.to_lowercase()) {
            continue;
        }
        if files.len() >= MAX_FILES {
            return Err(Error::TooLarge(format!("it holds more than {MAX_FILES} files")));
        }
        let left = MAX_TOTAL.saturating_sub(total);
        let mut bytes = Vec::new();
        f.take(left + 1).read_to_end(&mut bytes).map_err(|e| Error::Damaged(format!("{clean}: {e}")))?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL {
            return Err(Error::TooLarge(format!("its files come to more than {}", crate::package::bytes(MAX_TOTAL))));
        }
        files.push((base, bytes));
    }

    let has_code = files.iter().any(|(n, _)| {
        let n = n.to_ascii_lowercase();
        n.ends_with(".html") || n.ends_with(".htm") || n.ends_with(".js")
    });
    if !has_code {
        return Err(Error::NotAPackage("there's no HTML or JS in that widget".into()));
    }

    let own: WidgetManifest = files
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("manifest.json"))
        .and_then(|(_, b)| serde_json::from_slice(b).ok())
        .unwrap_or_default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = [own.name.trim(), stem.trim(), "Widget"]
        .into_iter()
        .find(|n| !n.is_empty())
        .unwrap_or("Widget")
        .chars()
        .take(100)
        .collect::<String>();
    let version = version.or_else(|| Version::parse(own.version.trim()).ok()).unwrap_or_else(|| Version::new(1, 0, 0));

    if fs::read_dir(dest).map_err(|e| Error::io("read the folder", dest, e))?.next().is_some() {
        return Err(Error::NotEmpty(dest.to_path_buf()));
    }
    let folder = format!("{}/{}", Kind::Widget.folder(), slug(&name));
    for (file_name, bytes) in &files {
        let out = join(dest, &format!("{folder}/{file_name}"));
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io("make the folder", parent, e))?;
        }
        fs::write(&out, bytes).map_err(|e| Error::io("write", &out, e))?;
    }

    let file_name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let manifest = Manifest {
        id: package_id(&name),
        name,
        version,
        lightair: Range::this_api(),
        author: String::new(),
        licence: String::new(),
        description: format!("A widget imported from {file_name}"),
        platforms: Vec::new(),
        dependencies: Default::default(),
        contents: vec![Content::new(Kind::Widget, folder)],
        permissions: Vec::new(),
        optional_permissions: Vec::new(),
    };
    let manifest_path = dest.join(crate::MANIFEST_FILE);
    fs::write(&manifest_path, manifest.to_json()).map_err(|e| Error::io("write", &manifest_path, e))?;
    Ok(manifest)
}

/// Installs the `.lwidget` at `path` into `store` as a package of one widget. `.lwidget` files
/// don't change their version number when their code changes, so when this version is already
/// installed with different contents the patch number goes up until it's free (or the same
/// contents are found installed).
pub(crate) fn install(store: &Store, path: &Path) -> Result<Install> {
    let scratch = store.scratch_dir()?;
    let result = (|| {
        let mut version = None;
        let mut last = None;
        for _ in 0..1000 {
            let _ = fs::remove_dir_all(&scratch);
            fs::create_dir_all(&scratch).map_err(|e| Error::io("make the folder", &scratch, e))?;
            let manifest = to_folder(path, &scratch, version.take())?;
            let package = Package::open(&scratch)?;
            match store.get(&manifest.id, &manifest.version)? {
                Some(existing) if existing.record.content_sha256 != package.content_sha256 => {
                    let v = &manifest.version;
                    version = Some(Version::new(v.major, v.minor, v.patch + 1));
                    last = Some(manifest);
                }
                _ => {
                    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    return store.install(&package, Source::Lwidget { name });
                }
            }
        }
        let (id, version) = last.map(|m| (m.id.to_string(), m.version.to_string())).unwrap_or_default();
        Err(Error::VersionConflict { id, version })
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}
