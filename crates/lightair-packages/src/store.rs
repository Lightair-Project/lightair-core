//! The user's installed packages: `<data dir>/Packages/<id>/<version>/`, one folder per
//! version so shows using different versions don't conflict.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Problem, Result};
use crate::hash::{CopyError, Sha256, copy_hashing};
use crate::manifest::{Manifest, PackageId, Platform, Range};
use crate::package::{FileEntry, Package};
use crate::paths::{join, now_secs, unique_name, write_atomic};

/// The store's folder inside the app's data directory.
pub const STORE_FOLDER: &str = "Packages";

/// The install record's file name, inside each installed version's folder. It starts with a dot,
/// so reading the folder as a package skips it.
pub const RECORD_FILE: &str = ".lightair-install.json";

const STAGING_PREFIX: &str = ".staging-";
const TRASH_PREFIX: &str = ".trash-";

/// Where an installed package came from. Kept in its install record and copied into the lock
/// files of the shows that use it, so a show opened elsewhere can say where to get it. Only a
/// file's name is kept, never the folder it was in on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Source {
    /// A `.airpkg` file.
    File { name: String },
    /// A package folder.
    Folder { name: String },
    /// A `.lwidget` widget file, installed as a package of one widget.
    Lwidget { name: String },
    /// The catalog (a download address).
    Catalog { url: String },
    /// A git repository at a tag or commit.
    Git { url: String, rev: String },
    /// Made on this computer with Export as package.
    Exported,
    #[serde(other)]
    Unknown,
}

impl Source {
    /// A file or folder source named after `path`'s last part.
    pub fn for_path(path: &Path) -> Source {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if path.is_dir() { Source::Folder { name } } else { Source::File { name } }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::File { name } | Source::Folder { name } | Source::Lwidget { name } => {
                write!(f, "the file {name}")
            }
            Source::Catalog { url } => write!(f, "the catalog ({url})"),
            Source::Git { url, rev } => write!(f, "{url} at {rev}"),
            Source::Exported => f.write_str("exported on a computer"),
            Source::Unknown => f.write_str("an unknown source"),
        }
    }
}

/// What the store keeps about each installed version, beside its files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub id: PackageId,
    pub version: Version,
    /// See [`content_sha256`](crate::content_sha256).
    pub content_sha256: Sha256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<Sha256>,
    /// The files' total size.
    pub size: u64,
    pub source: Source,
    /// When it was installed, in seconds since 1970.
    pub installed_at: u64,
    /// Every file installed, with its size and checksum.
    pub files: BTreeMap<String, FileEntry>,
}

/// An installed version of a package.
#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: Manifest,
    /// Its folder: `<data dir>/Packages/<id>/<version>/`.
    pub path: PathBuf,
    pub record: InstallRecord,
}

impl Installed {
    pub fn id(&self) -> &PackageId {
        &self.manifest.id
    }

    pub fn version(&self) -> &Version {
        &self.manifest.version
    }

    /// What stops this Lightair using it (for example after an update moved the package API
    /// on): the app lists it switched off, with these reasons, rather than deleting it.
    pub fn problems(&self) -> Vec<Problem> {
        self.manifest.check_supported()
    }

    /// Whether this Lightair can use it.
    pub fn is_usable(&self) -> bool {
        self.problems().is_empty()
    }

    /// The folder of one of its contents.
    pub fn content_path(&self, rel: &str) -> PathBuf {
        join(&self.path, rel)
    }
}

/// The result of an install.
#[derive(Debug, Clone)]
pub struct Install {
    pub installed: Installed,
    /// The same version with the same contents was installed already; nothing was written.
    pub already_installed: bool,
    /// Its dependencies no installed version satisfies (to offer next).
    pub missing_dependencies: Vec<(PackageId, Range)>,
}

/// Something wrong with an installed version's files, found by [`Store::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    Missing(String),
    Changed(String),
}

impl fmt::Display for Damage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Damage::Missing(p) => write!(f, "{p} is missing"),
            Damage::Changed(p) => write!(f, "{p} has changed since it was installed"),
        }
    }
}

/// The user's installed packages.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The store in `data_dir` (the app's own data directory; the store is its
    /// [`STORE_FOLDER`]). Makes the folder if need be and clears what an interrupted install or
    /// removal left behind.
    pub fn open(data_dir: &Path) -> Result<Store> {
        let root = data_dir.join(STORE_FOLDER);
        fs::create_dir_all(&root).map_err(|e| Error::io("make the folder", &root, e))?;
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(STAGING_PREFIX) || name.starts_with(TRASH_PREFIX) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        Ok(Store { root })
    }

    /// The store's folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a version is (or would be) installed.
    pub fn path_of(&self, id: &PackageId, version: &Version) -> PathBuf {
        self.root.join(id.as_str()).join(version.to_string())
    }

    /// Installs `package`, recorded as from `source`. The files are written to a hidden folder
    /// in the store, each checked against its checksum, then moved into place in one rename, so
    /// a version folder is either whole or absent. Installing the same version again with the
    /// same contents does nothing ([`Install::already_installed`]); with different contents it
    /// fails ([`Error::VersionConflict`]).
    pub fn install(&self, package: &Package, source: Source) -> Result<Install> {
        let m = &package.manifest;
        let problems = m.check_supported();
        if !problems.is_empty() {
            return Err(Error::Invalid { problems });
        }
        if let Some(existing) = self.get(&m.id, &m.version)? {
            if existing.record.content_sha256 != package.content_sha256 {
                return Err(Error::VersionConflict { id: m.id.to_string(), version: m.version.to_string() });
            }
            let missing_dependencies = self.missing_dependencies(&existing.manifest)?;
            return Ok(Install { installed: existing, already_installed: true, missing_dependencies });
        }

        let staging = self.root.join(unique_name(STAGING_PREFIX));
        fs::create_dir_all(&staging).map_err(|e| Error::io("make the folder", &staging, e))?;
        let result = (|| {
            package.copy_to(&staging)?;
            let record = InstallRecord {
                id: m.id.clone(),
                version: m.version.clone(),
                content_sha256: package.content_sha256,
                archive_sha256: package.archive_sha256,
                size: package.size,
                source,
                installed_at: now_secs(),
                files: package.files.clone(),
            };
            let json = serde_json::to_vec_pretty(&record).unwrap_or_default();
            let record_path = staging.join(RECORD_FILE);
            fs::write(&record_path, json).map_err(|e| Error::io("write", &record_path, e))?;
            let dest = self.path_of(&m.id, &m.version);
            let id_dir = self.root.join(m.id.as_str());
            fs::create_dir_all(&id_dir).map_err(|e| Error::io("make the folder", &id_dir, e))?;
            fs::rename(&staging, &dest).map_err(|e| Error::io("install into", &dest, e))?;
            Ok(Installed { manifest: m.clone(), path: dest, record })
        })();
        match result {
            Ok(installed) => {
                let missing_dependencies = self.missing_dependencies(&installed.manifest)?;
                Ok(Install { installed, already_installed: false, missing_dependencies })
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                Err(e)
            }
        }
    }

    /// Installs whatever is at `path`, as dropped on the app or opened: a package folder, a
    /// `.lwidget` (as a package of one widget, see [`crate::lwidget`]), or a `.airpkg`. A zip
    /// with no manifest that holds a widget's files is installed as a `.lwidget` would be.
    pub fn install_path(&self, path: &Path) -> Result<Install> {
        let is_lwidget = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("lwidget"));
        if is_lwidget && !path.is_dir() {
            return crate::lwidget::install(self, path);
        }
        match Package::open(path) {
            Ok(package) => self.install(&package, Source::for_path(path)),
            // A widget zipped under another extension (`.zip`) is still a widget.
            Err(e @ Error::NotAPackage(_)) if !path.is_dir() => crate::lwidget::install(self, path).map_err(|_| e),
            Err(e) => Err(e),
        }
    }

    /// Every installed version, sorted by id then version. Folders that aren't an installed
    /// version (no readable manifest and record) are skipped.
    pub fn list(&self) -> Result<Vec<Installed>> {
        let mut out = Vec::new();
        for id_name in self.dir_names(&self.root)? {
            let Ok(id) = PackageId::new(&id_name) else { continue };
            out.extend(self.versions(&id)?);
        }
        Ok(out)
    }

    /// The installed versions of `id`, oldest first.
    pub fn versions(&self, id: &PackageId) -> Result<Vec<Installed>> {
        let dir = self.root.join(id.as_str());
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Installed> = self
            .dir_names(&dir)?
            .into_iter()
            .filter_map(|v| Version::parse(&v).ok())
            .filter_map(|v| self.load(id, &v))
            .collect();
        out.sort_by(|a, b| a.version().cmp(b.version()));
        Ok(out)
    }

    /// One installed version, if it's there.
    pub fn get(&self, id: &PackageId, version: &Version) -> Result<Option<Installed>> {
        Ok(self.load(id, version))
    }

    /// The highest installed version `range` allows that this Lightair can use (and that works
    /// on `platform`, when one is given).
    pub fn best(&self, id: &PackageId, range: &Range, platform: Option<Platform>) -> Result<Option<Installed>> {
        Ok(self
            .versions(id)?
            .into_iter()
            .rev()
            .find(|i| range.matches(i.version()) && i.is_usable() && platform.is_none_or(|p| i.manifest.supports(p))))
    }

    /// Removes one installed version. Its folder is first renamed out of the way, so a removal
    /// that's interrupted never leaves a half-deleted version in place.
    pub fn remove(&self, id: &PackageId, version: &Version) -> Result<()> {
        let path = self.path_of(id, version);
        if !path.is_dir() {
            return Err(Error::NotInstalled { id: id.to_string(), version: version.to_string() });
        }
        let trash = self.root.join(unique_name(TRASH_PREFIX));
        fs::rename(&path, &trash).map_err(|e| Error::io("remove", &path, e))?;
        fs::remove_dir_all(&trash).map_err(|e| Error::io("remove", &trash, e))?;
        let id_dir = self.root.join(id.as_str());
        if self.dir_names(&id_dir).is_ok_and(|v| v.is_empty()) {
            let _ = fs::remove_dir_all(&id_dir);
        }
        Ok(())
    }

    /// Removes every installed version of `id`; how many there were.
    pub fn remove_all(&self, id: &PackageId) -> Result<usize> {
        let versions = self.versions(id)?;
        for v in &versions {
            self.remove(id, v.version())?;
        }
        Ok(versions.len())
    }

    /// Checks an installed version's files against its record: missing or changed files.
    /// (Files added since aren't reported; nothing reads them.)
    pub fn verify(&self, installed: &Installed) -> Result<Vec<Damage>> {
        let mut damage = Vec::new();
        for (rel, entry) in &installed.record.files {
            let path = join(&installed.path, rel);
            let Ok(file) = File::open(&path) else {
                damage.push(Damage::Missing(rel.clone()));
                continue;
            };
            match copy_hashing(file, None, entry.size) {
                Ok((sha, size)) if sha == entry.sha256 && size == entry.size => {}
                Ok(_) | Err(CopyError::TooBig) => damage.push(Damage::Changed(rel.clone())),
                Err(CopyError::Read(e) | CopyError::Write(e)) => {
                    return Err(Error::io("read", &path, e));
                }
            }
        }
        Ok(damage)
    }

    /// `manifest`'s dependencies that no installed, usable version satisfies.
    pub fn missing_dependencies(&self, manifest: &Manifest) -> Result<Vec<(PackageId, Range)>> {
        let mut out = Vec::new();
        for (id, range) in &manifest.dependencies {
            if self.best(id, range, None)?.is_none() {
                out.push((id.clone(), range.clone()));
            }
        }
        Ok(out)
    }

    /// Reads one installed version's manifest and record.
    fn load(&self, id: &PackageId, version: &Version) -> Option<Installed> {
        let path = self.path_of(id, version);
        let manifest = Manifest::from_json(&fs::read(path.join(crate::MANIFEST_FILE)).ok()?).ok()?;
        let record: InstallRecord = serde_json::from_slice(&fs::read(path.join(RECORD_FILE)).ok()?).ok()?;
        (manifest.id == *id && manifest.version == *version && record.id == *id).then_some(Installed {
            manifest,
            path,
            record,
        })
    }

    /// The names of the folders in `dir`, hidden ones left out.
    fn dir_names(&self, dir: &Path) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(dir).map_err(|e| Error::io("read the folder", dir, e))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && entry.file_type().is_ok_and(|t| t.is_dir()) {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// A new hidden folder in the store to prepare a package in (on the same disk as the store,
    /// so installing from it is a rename; cleared at the next open if left behind).
    pub(crate) fn scratch_dir(&self) -> Result<PathBuf> {
        let dir = self.root.join(unique_name(STAGING_PREFIX));
        fs::create_dir_all(&dir).map_err(|e| Error::io("make the folder", &dir, e))?;
        Ok(dir)
    }
}

/// Writes `value` as pretty JSON to `path` in one step.
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}
