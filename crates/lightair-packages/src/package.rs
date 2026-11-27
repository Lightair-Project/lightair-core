//! Reading a package from a folder or a `.airpkg` (zip): every file listed and hashed, nothing
//! allowed outside the package, sizes held to [`Limits`].

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Problem, Result};
use crate::hash::{CopyError, Sha256, copy_hashing};
use crate::manifest::{Manifest, Tier};
use crate::paths::{clean_path, is_junk_name, is_junk_path, join};

/// How large a package may be. The defaults allow big media packs while refusing archives
/// built to fill the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Files in the package (folders and skipped clutter don't count).
    pub max_files: usize,
    /// The largest one file may be, unpacked.
    pub max_file_size: u64,
    /// All files together, unpacked (also the largest a `.airpkg` itself may be).
    pub max_total_size: u64,
    /// The longest a path inside the package may be, in bytes.
    pub max_path_len: usize,
    /// The deepest a path may be nested, in folders.
    pub max_depth: usize,
    /// The largest the manifest may be.
    pub max_manifest_size: u64,
    /// The highest tier of content accepted ([`SUPPORTED_TIER`](crate::SUPPORTED_TIER) by default:
    /// what this Lightair runs).
    pub max_tier: Tier,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_files: 20_000,
            max_file_size: 4 << 30,
            max_total_size: 16 << 30,
            max_path_len: 400,
            max_depth: 24,
            max_manifest_size: 1 << 20,
            max_tier: crate::SUPPORTED_TIER,
        }
    }
}

impl Limits {
    /// The default limits, accepting content of every tier (for the checks and authors' tools).
    pub fn every_tier() -> Limits {
        Limits { max_tier: Tier::Native, ..Limits::default() }
    }
}

/// One file in a package: its size and checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub size: u64,
    pub sha256: Sha256,
}

/// Where a package was read from.
#[derive(Debug, Clone)]
enum Origin {
    Folder(PathBuf),
    /// A zip; `prefix` is the folder inside it that holds the package ("" for the top).
    Archive {
        path: PathBuf,
        prefix: String,
    },
}

/// A package read from a folder or a `.airpkg`, its manifest checked, every file listed with its
/// size and SHA-256. Nothing has been written anywhere; [`Store::install`](crate::Store::install)
/// does that.
#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: Manifest,
    /// Every file, by its `/`-separated path inside the package (the manifest included; hidden
    /// files and system clutter left out).
    pub files: BTreeMap<String, FileEntry>,
    /// The files' total size, unpacked.
    pub size: u64,
    /// The checksum of the contents, the same whether the package came as a folder or an
    /// archive, and however the archive was compressed: see [`content_sha256`].
    pub content_sha256: Sha256,
    /// The checksum of the `.airpkg` file itself, when it came as one.
    pub archive_sha256: Option<Sha256>,
    origin: Origin,
    limits: Limits,
}

/// The checksum of a package's contents: SHA-256 over one line per file, sorted by path,
/// `<file sha256> <space> <space> <path> <newline>` (the format `sha256sum` prints).
pub fn content_sha256(files: &BTreeMap<String, FileEntry>) -> Sha256 {
    let mut text = String::new();
    for (path, entry) in files {
        text.push_str(&format!("{}  {path}\n", entry.sha256));
    }
    Sha256::of(text.as_bytes())
}

impl Package {
    /// Reads the package at `path`: a folder, or anything else as a `.airpkg` zip. Fails with
    /// [`Error::Invalid`] (every problem listed) when the manifest is wrong, a listed content
    /// path is missing, two files differ only by case, or this Lightair can't use the package
    /// ([`Manifest::check_supported`]).
    pub fn open(path: &Path) -> Result<Package> {
        Package::open_with(path, Limits::default())
    }

    /// [`Package::open`] with other limits.
    pub fn open_with(path: &Path, limits: Limits) -> Result<Package> {
        let meta = fs::metadata(path).map_err(|e| Error::io("open", path, e))?;
        if meta.is_dir() { open_folder(path, limits) } else { open_archive(path, limits) }
    }

    /// The file or folder it was read from.
    pub fn origin_path(&self) -> &Path {
        match &self.origin {
            Origin::Folder(p) => p,
            Origin::Archive { path, .. } => path,
        }
    }

    /// Whether it was read from a `.airpkg` (rather than a folder).
    pub fn is_archive(&self) -> bool {
        matches!(self.origin, Origin::Archive { .. })
    }

    /// The bytes of one of its files (`rel` as in [`Package::files`]), checked against the
    /// checksum read when the package was opened.
    pub fn read(&self, rel: &str) -> Result<Vec<u8>> {
        let entry = self.files.get(rel).ok_or_else(|| Error::NotAPackage(format!("{rel} isn't in the package")))?;
        let mut bytes = Vec::new();
        match &self.origin {
            Origin::Folder(root) => {
                let src = join(root, rel);
                File::open(&src)
                    .and_then(|f| f.take(entry.size + 1).read_to_end(&mut bytes))
                    .map_err(|e| Error::io("read", &src, e))?;
            }
            Origin::Archive { path, prefix } => {
                let file = File::open(path).map_err(|e| Error::io("open", path, e))?;
                let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| Error::Damaged(e.to_string()))?;
                let name = format!("{prefix}{rel}");
                let f = zip.by_name(&name).map_err(|e| Error::Damaged(format!("{rel}: {e}")))?;
                f.take(entry.size + 1).read_to_end(&mut bytes).map_err(|e| Error::Damaged(format!("{rel}: {e}")))?;
            }
        }
        if bytes.len() as u64 != entry.size || Sha256::of(&bytes) != entry.sha256 {
            return Err(Error::ChangedDuringInstall { path: rel.to_string() });
        }
        Ok(bytes)
    }

    /// Writes every file under `dest` (which must exist), each checked against its checksum.
    pub fn extract_to(&self, dest: &Path) -> Result<()> {
        self.copy_to(dest)
    }

    /// Writes every file of the package under `dest` (which must exist), checking each against
    /// the checksum read when the package was opened, so a package changed in between isn't
    /// installed half old, half new.
    pub(crate) fn copy_to(&self, dest: &Path) -> Result<()> {
        match &self.origin {
            Origin::Folder(root) => {
                for (rel, entry) in &self.files {
                    let src = join(root, rel);
                    let meta = fs::symlink_metadata(&src).map_err(|e| Error::io("read", &src, e))?;
                    if !meta.is_file() {
                        return Err(Error::ChangedDuringInstall { path: rel.clone() });
                    }
                    let file = File::open(&src).map_err(|e| Error::io("read", &src, e))?;
                    write_checked(file, dest, rel, entry)?;
                }
                Ok(())
            }
            Origin::Archive { path, prefix } => {
                let file = File::open(path).map_err(|e| Error::io("open", path, e))?;
                let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| Error::Damaged(e.to_string()))?;
                let mut written = 0usize;
                for i in 0..zip.len() {
                    let f = zip.by_index(i).map_err(|e| Error::Damaged(e.to_string()))?;
                    if f.is_dir() {
                        continue;
                    }
                    let name = f.name().map_err(|e| Error::Damaged(e.to_string()))?.to_string();
                    let Ok(clean) = clean_path(&name, self.limits.max_path_len, self.limits.max_depth) else {
                        continue;
                    };
                    let Some(rel) = clean.strip_prefix(prefix.as_str()) else { continue };
                    let Some(entry) = self.files.get(rel) else { continue };
                    write_checked(f, dest, rel, entry)?;
                    written += 1;
                }
                if written != self.files.len() {
                    return Err(Error::ChangedDuringInstall { path: path.display().to_string() });
                }
                Ok(())
            }
        }
    }
}

/// Copies `reader` to `dest/rel`, failing unless it has `entry`'s size and checksum.
fn write_checked(reader: impl Read, dest: &Path, rel: &str, entry: &FileEntry) -> Result<()> {
    let out_path = join(dest, rel);
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io("make the folder", parent, e))?;
    }
    let mut out = File::create(&out_path).map_err(|e| Error::io("write", &out_path, e))?;
    match copy_hashing(reader, Some(&mut out as &mut dyn Write), entry.size) {
        Ok((sha, size)) if sha == entry.sha256 && size == entry.size => Ok(()),
        Ok(_) | Err(CopyError::TooBig) => Err(Error::ChangedDuringInstall { path: rel.into() }),
        Err(CopyError::Read(e)) => Err(Error::Damaged(format!("{rel}: {e}"))),
        Err(CopyError::Write(e)) => Err(Error::io("write", &out_path, e)),
    }
}

/// Keeps the running totals while files are listed, enforcing the limits.
struct Tally<'a> {
    limits: &'a Limits,
    files: BTreeMap<String, FileEntry>,
    total: u64,
}

impl Tally<'_> {
    fn check_next(&self, rel: &str, size: u64) -> Result<()> {
        let l = self.limits;
        if self.files.len() >= l.max_files {
            return Err(Error::TooLarge(format!("it holds more than {} files", l.max_files)));
        }
        if size > l.max_file_size {
            return Err(Error::TooLarge(format!(
                "{rel} is {}, over the {} a file may be",
                bytes(size),
                bytes(l.max_file_size)
            )));
        }
        if self.total + size > l.max_total_size {
            return Err(Error::TooLarge(format!("its files come to more than {}", bytes(l.max_total_size))));
        }
        Ok(())
    }

    fn add(&mut self, rel: String, entry: FileEntry) {
        self.total += entry.size;
        self.files.insert(rel, entry);
    }
}

fn open_folder(root: &Path, limits: Limits) -> Result<Package> {
    let mut tally = Tally { limits: &limits, files: BTreeMap::new(), total: 0 };
    walk(root, "", 0, &mut tally)?;
    let manifest_path = root.join(crate::MANIFEST_FILE);
    let manifest_bytes = match tally.files.get(crate::MANIFEST_FILE) {
        Some(e) if e.size > limits.max_manifest_size => {
            return Err(Error::Invalid { problems: vec![Problem::ManifestTooLarge] });
        }
        Some(_) => fs::read(&manifest_path).map_err(|e| Error::io("read", &manifest_path, e))?,
        None => {
            return Err(Error::NotAPackage(format!("there's no {} in that folder", crate::MANIFEST_FILE)));
        }
    };
    finish(&manifest_bytes, tally, None, Origin::Folder(root.to_path_buf()), limits)
}

/// Lists and hashes the files under `dir` (`rel` is its path inside the package).
fn walk(dir: &Path, rel: &str, depth: usize, tally: &mut Tally) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| Error::io("read the folder", dir, e))?
        .collect::<std::io::Result<_>>()
        .map_err(|e| Error::io("read the folder", dir, e))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| Error::Unsafe(format!("the name {n:?} in {} isn't valid text", dir.display())))?;
        if is_junk_name(&name) {
            continue;
        }
        let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let clean = clean_path(&child_rel, tally.limits.max_path_len, tally.limits.max_depth)
            .map_err(|reason| Error::Unsafe(format!("\"{child_rel}\" {reason}")))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| Error::io("read", &path, e))?;
        if meta.file_type().is_symlink() {
            return Err(Error::Unsafe(format!("{clean} is a link; packages can't contain links")));
        }
        if meta.is_dir() {
            if depth + 1 >= tally.limits.max_depth {
                return Err(Error::Unsafe(format!(
                    "{clean} is nested more than {} folders deep",
                    tally.limits.max_depth
                )));
            }
            walk(&path, &clean, depth + 1, tally)?;
        } else if meta.is_file() {
            tally.check_next(&clean, meta.len())?;
            let file = File::open(&path).map_err(|e| Error::io("read", &path, e))?;
            let (sha256, size) = match copy_hashing(file, None, tally.limits.max_file_size) {
                Ok(r) => r,
                Err(CopyError::TooBig) => {
                    return Err(Error::TooLarge(format!("{clean} grew while it was read")));
                }
                Err(CopyError::Read(e) | CopyError::Write(e)) => {
                    return Err(Error::io("read", &path, e));
                }
            };
            tally.check_next(&clean, size)?;
            tally.add(clean, FileEntry { size, sha256 });
        } else {
            return Err(Error::Unsafe(format!("{clean} isn't an ordinary file or folder")));
        }
    }
    Ok(())
}

fn open_archive(path: &Path, limits: Limits) -> Result<Package> {
    let file = File::open(path).map_err(|e| Error::io("open", path, e))?;
    let len = file.metadata().map_err(|e| Error::io("read", path, e))?.len();
    if len > limits.max_total_size {
        return Err(Error::TooLarge(format!("the file is {}", bytes(len))));
    }
    let archive_sha256 = Sha256::of_reader(BufReader::new(&file)).map_err(|e| Error::io("read", path, e))?;
    let file = File::open(path).map_err(|e| Error::io("open", path, e))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|_| Error::NotAPackage("it isn't a folder or a .airpkg (zip) file".into()))?;
    if zip.len() > limits.max_files.saturating_mul(4) {
        return Err(Error::TooLarge(format!("it lists {} entries", zip.len())));
    }

    // Every file, by its clean path in the zip (before the package's folder is worked out).
    let mut tally = Tally { limits: &limits, files: BTreeMap::new(), total: 0 };
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| Error::Damaged(e.to_string()))?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().map_err(|e| Error::Damaged(e.to_string()))?.to_string();
        let clean = clean_path(&name, limits.max_path_len, limits.max_depth)
            .map_err(|reason| Error::Unsafe(format!("\"{name}\" {reason}")))?;
        if is_junk_path(&clean) {
            continue;
        }
        if f.is_symlink() {
            return Err(Error::Unsafe(format!("{clean} is a link; packages can't contain links")));
        }
        if tally.files.contains_key(&clean) {
            return Err(Error::Damaged(format!("it lists {clean} twice")));
        }
        let declared = f.size();
        tally.check_next(&clean, declared)?;
        let (sha256, size) = match copy_hashing(&mut f, None, declared) {
            Ok(r) => r,
            Err(CopyError::TooBig) => {
                return Err(Error::Damaged(format!("{clean} is larger than the archive says")));
            }
            Err(CopyError::Read(e) | CopyError::Write(e)) => {
                return Err(Error::Damaged(format!("{clean}: {e}")));
            }
        };
        if size != declared {
            return Err(Error::Damaged(format!("{clean} is shorter than the archive says")));
        }
        tally.add(clean, FileEntry { size, sha256 });
    }

    // The package is the zip's top, or the one folder at its top (a zip of the folder itself).
    let prefix = if tally.files.contains_key(crate::MANIFEST_FILE) {
        String::new()
    } else {
        let tops: std::collections::BTreeSet<&str> =
            tally.files.keys().map(|k| k.split('/').next().unwrap_or("")).collect();
        match tops.into_iter().collect::<Vec<_>>().as_slice() {
            [top] if tally.files.contains_key(&format!("{top}/{}", crate::MANIFEST_FILE)) => {
                format!("{top}/")
            }
            _ => {
                return Err(Error::NotAPackage(format!("there's no {} at the top of that file", crate::MANIFEST_FILE)));
            }
        }
    };
    if !prefix.is_empty() {
        tally.files = std::mem::take(&mut tally.files)
            .into_iter()
            .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|k| (k.to_string(), v)))
            .collect();
    }

    let manifest_entry = tally.files[crate::MANIFEST_FILE];
    if manifest_entry.size > limits.max_manifest_size {
        return Err(Error::Invalid { problems: vec![Problem::ManifestTooLarge] });
    }
    let mut manifest_bytes = Vec::new();
    zip.by_name(&format!("{prefix}{}", crate::MANIFEST_FILE))
        .map_err(|e| Error::Damaged(e.to_string()))?
        .take(limits.max_manifest_size)
        .read_to_end(&mut manifest_bytes)
        .map_err(|e| Error::Damaged(e.to_string()))?;
    let origin = Origin::Archive { path: path.to_path_buf(), prefix };
    finish(&manifest_bytes, tally, Some(archive_sha256), origin, limits)
}

/// Parses the manifest and checks it against the files found.
fn finish(
    manifest_bytes: &[u8],
    tally: Tally,
    archive_sha256: Option<Sha256>,
    origin: Origin,
    limits: Limits,
) -> Result<Package> {
    let Tally { files, total, .. } = tally;
    let manifest = Manifest::from_json(manifest_bytes)?;
    let mut problems = manifest.check_supported_up_to(limits.max_tier);

    for c in &manifest.contents {
        let folder = format!("{}/", c.path);
        if !files.contains_key(&c.path) && !files.keys().any(|k| k.starts_with(&folder)) {
            problems.push(Problem::MissingContent(c.path.clone()));
        }
        if c.kind == crate::Kind::Helper && !c.entry.values().any(|program| files.contains_key(program)) {
            problems.push(Problem::NoEntryInFile(c.path.clone()));
        }
    }

    let mut lower: BTreeMap<String, &String> = BTreeMap::new();
    for k in files.keys() {
        if let Some(other) = lower.insert(k.to_lowercase(), k) {
            problems.push(Problem::CaseClash(other.clone(), k.clone()));
        }
    }

    if !problems.is_empty() {
        return Err(Error::Invalid { problems });
    }
    let content_sha256 = content_sha256(&files);
    Ok(Package { manifest, files, size: total, content_sha256, archive_sha256, origin, limits })
}

/// A size in bytes for a message ("12.5 MB").
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: &[&str] = &["bytes", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} bytes") } else { format!("{v:.1} {}", UNITS[unit]) }
}
