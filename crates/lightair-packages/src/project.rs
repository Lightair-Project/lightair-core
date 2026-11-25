//! A show's packages: `packages.json` (what it needs, by id and version range) and
//! `packages-lock.json` (the exact version, checksum, source and size of each one resolved), and
//! resolving them against the installed store.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Problem, Result};
use crate::hash::Sha256;
use crate::manifest::{PackageId, Platform, Range};
use crate::store::{Installed, Source, write_json};

/// The file in a show's folder listing the packages it needs.
pub const PROJECT_FILE: &str = "packages.json";

/// The file in a show's folder pinning each package's exact version and checksum.
pub const LOCK_FILE: &str = "packages-lock.json";

/// The format number both files are written with.
const FORMAT: u32 = 1;

fn format_one() -> u32 {
    FORMAT
}

/// `packages.json`: the packages a show needs, by id and version range.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirements {
    #[serde(default = "format_one")]
    pub format: u32,
    #[serde(default)]
    pub packages: BTreeMap<PackageId, Range>,
}

/// `packages-lock.json`: the version each package (and each dependency) resolved to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default = "format_one")]
    pub format: u32,
    #[serde(default)]
    pub packages: BTreeMap<PackageId, Locked>,
}

/// One package pinned in a lock file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locked {
    pub version: Version,
    /// The package's [content checksum](crate::content_sha256).
    pub sha256: Sha256,
    #[serde(default = "unknown_source")]
    pub source: Source,
    /// Its size unpacked, to say how much opening the show elsewhere will install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

fn unknown_source() -> Source {
    Source::Unknown
}

impl Locked {
    /// The lock entry for an installed version.
    pub fn of(installed: &Installed) -> Locked {
        Locked {
            version: installed.version().clone(),
            sha256: installed.record.content_sha256,
            source: installed.record.source.clone(),
            size: Some(installed.record.size),
        }
    }
}

/// Which packages a resolve may move to a newer installed version than the lock names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Update {
    /// Keep every locked version (the default: a show doesn't change under the user).
    #[default]
    None,
    /// Take the newest installed version each range allows.
    All,
    /// Update these; keep the rest.
    Only(BTreeSet<PackageId>),
}

impl Update {
    fn includes(&self, id: &PackageId) -> bool {
        match self {
            Update::None => false,
            Update::All => true,
            Update::Only(ids) => ids.contains(id),
        }
    }
}

/// How to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Leave out versions that don't work here (`None`: don't check).
    pub platform: Option<Platform>,
    pub update: Update,
}

impl Default for ResolveOptions {
    fn default() -> ResolveOptions {
        ResolveOptions { platform: Platform::current(), update: Update::None }
    }
}

/// A package resolved to an installed version.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub id: PackageId,
    pub version: Version,
    /// Its installed folder.
    pub path: PathBuf,
    pub sha256: Sha256,
    /// The package that needs it, or `None` when the show lists it itself.
    pub required_by: Option<PackageId>,
    /// Whether the version came from the lock file (rather than being newly chosen).
    pub from_lock: bool,
}

/// Why a package the show needs can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissingReason {
    /// No version of it is installed (or not the locked one).
    NotInstalled,
    /// Versions are installed, but none the range allows.
    NoMatchingVersion { installed: Vec<Version> },
    /// The versions the range allows don't work on this platform.
    NotForThisPlatform,
    /// The version the range allows can't be used by this Lightair.
    NotUsable(Vec<Problem>),
}

/// A package the show needs that isn't available. The app lists these when the show opens, with
/// their sizes and sources when the lock file knows them, and offers to install them; objects
/// from them stay in the show as placeholders.
#[derive(Debug, Clone)]
pub struct Missing {
    pub id: PackageId,
    pub range: Range,
    pub required_by: Option<PackageId>,
    /// What the lock file pinned, if anything: version, checksum, source and size.
    pub locked: Option<Locked>,
    pub reason: MissingReason,
}

/// Two packages need versions of a third that no one version satisfies.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub id: PackageId,
    /// The version chosen first.
    pub picked: Version,
    /// The range it doesn't satisfy.
    pub range: Range,
    pub required_by: Option<PackageId>,
}

/// An installed version whose contents differ from what the lock file recorded (same version
/// number, different files).
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub id: PackageId,
    pub version: Version,
    pub locked: Sha256,
    pub installed: Sha256,
}

/// What a show's packages resolve to on this computer.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    pub resolved: Vec<Resolved>,
    pub missing: Vec<Missing>,
    pub conflicts: Vec<Conflict>,
    pub mismatched: Vec<Mismatch>,
    /// The lock file to save: the resolved versions, plus what was locked for the missing ones.
    pub lock: Lock,
}

impl Resolution {
    /// Whether everything resolved, with no conflicts or changed contents.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.conflicts.is_empty() && self.mismatched.is_empty()
    }

    /// The installed folder `id` resolved to.
    pub fn path_of(&self, id: &PackageId) -> Option<&Path> {
        self.resolved.iter().find(|r| &r.id == id).map(|r| r.path.as_path())
    }

    /// The missing packages' total size as far as the lock file knows it, and how many have no
    /// known size.
    pub fn missing_size(&self) -> (u64, usize) {
        let mut known = 0;
        let mut unknown = 0;
        for m in &self.missing {
            match m.locked.as_ref().and_then(|l| l.size) {
                Some(s) => known += s,
                None => unknown += 1,
            }
        }
        (known, unknown)
    }
}

/// A show's packages: its folder, `packages.json` and `packages-lock.json`.
#[derive(Debug, Clone)]
pub struct Project {
    /// The show's folder (`My Show.lightair/`).
    pub dir: PathBuf,
    pub requirements: Requirements,
    pub lock: Lock,
}

impl Project {
    /// Reads a show's package files from its folder; a show without them needs no packages.
    pub fn load(dir: &Path) -> Result<Project> {
        Ok(Project {
            dir: dir.to_path_buf(),
            requirements: read_json(&dir.join(PROJECT_FILE))?.unwrap_or_default(),
            lock: read_json(&dir.join(LOCK_FILE))?.unwrap_or_default(),
        })
    }

    /// Writes both files (each in one step). A show that needs no packages and never had the
    /// files gets none.
    pub fn save(&self) -> Result<()> {
        let req_path = self.dir.join(PROJECT_FILE);
        let lock_path = self.dir.join(LOCK_FILE);
        let empty = self.requirements.packages.is_empty() && self.lock.packages.is_empty();
        if empty && !req_path.exists() && !lock_path.exists() {
            return Ok(());
        }
        write_json(&req_path, &self.requirements)?;
        write_json(&lock_path, &self.lock)
    }

    /// Records that the show uses `installed` (when something from it is added to the show):
    /// needs it at `^version` unless the show's range already allows this version, and locks
    /// this version. Whether anything changed.
    pub fn add(&mut self, installed: &Installed) -> bool {
        let id = installed.id().clone();
        let mut changed = false;
        match self.requirements.packages.get(&id) {
            Some(range) if range.matches(installed.version()) => {}
            _ => {
                self.requirements.packages.insert(id.clone(), Range::caret(installed.version()));
                changed = true;
            }
        }
        let locked = Locked::of(installed);
        if self.lock.packages.get(&id) != Some(&locked) {
            self.lock.packages.insert(id, locked);
            changed = true;
        }
        changed
    }

    /// Stops needing `id` (and drops its lock entry). Dependencies only it needed are left
    /// until the next [`Project::resolve`] and [`Project::apply`], or
    /// [`Project::remove_unused`]. Whether it was needed.
    pub fn remove(&mut self, id: &PackageId) -> bool {
        self.lock.packages.remove(id);
        self.requirements.packages.remove(id).is_some()
    }

    /// **Remove unused packages**: drops every package the show lists that isn't in `used`
    /// (the ids of packages something in the show still comes from, which the app works out),
    /// then the lock entries nothing still needs, following dependencies through `installed`.
    /// Returns the ids dropped from `packages.json`.
    pub fn remove_unused(&mut self, used: &BTreeSet<PackageId>, installed: &[Installed]) -> Vec<PackageId> {
        let removed: Vec<PackageId> =
            self.requirements.packages.keys().filter(|id| !used.contains(*id)).cloned().collect();
        for id in &removed {
            self.requirements.packages.remove(id);
        }

        // Everything still reachable: the remaining requirements and their dependencies.
        let mut reachable: BTreeSet<PackageId> = BTreeSet::new();
        let mut unknown_dependencies = false;
        let mut queue: VecDeque<(PackageId, Range)> =
            self.requirements.packages.iter().map(|(i, r)| (i.clone(), r.clone())).collect();
        while let Some((id, range)) = queue.pop_front() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            let locked = self.lock.packages.get(&id).map(|l| &l.version);
            let manifest = installed
                .iter()
                .filter(|i| i.id() == &id)
                .filter(|i| match locked {
                    Some(v) => i.version() == v,
                    None => range.matches(i.version()),
                })
                .max_by(|a, b| a.version().cmp(b.version()))
                .map(|i| &i.manifest);
            match manifest {
                Some(m) => queue.extend(m.dependencies.iter().map(|(i, r)| (i.clone(), r.clone()))),
                None => unknown_dependencies = true,
            }
        }
        // When some package's dependencies can't be known (it isn't installed here), keep the
        // lock entries that might be them rather than lose what another computer resolved.
        self.lock.packages.retain(|id, _| reachable.contains(id) || (unknown_dependencies && !removed.contains(id)));
        removed
    }

    /// Resolves the show's packages against `installed` (normally [`Store::list`](crate::Store::list)):
    /// each listed package and, in turn, its dependencies. A locked version is kept while the
    /// range allows it (unless `options.update` says to update); otherwise the newest installed
    /// version the range allows, usable here and on `options.platform`, is chosen. Nothing is
    /// installed or written: save [`Resolution::lock`] with [`Project::apply`] and
    /// [`Project::save`].
    ///
    /// Each package gets one version; a second range it doesn't satisfy is reported as a
    /// [`Conflict`] rather than searched around.
    pub fn resolve(&self, installed: &[Installed], options: &ResolveOptions) -> Resolution {
        let mut by_id: BTreeMap<&PackageId, Vec<&Installed>> = BTreeMap::new();
        for i in installed {
            by_id.entry(i.id()).or_default().push(i);
        }
        for list in by_id.values_mut() {
            list.sort_by(|a, b| a.version().cmp(b.version()));
        }

        let mut out = Resolution::default();
        let mut picked: BTreeMap<PackageId, Option<Version>> = BTreeMap::new();
        let mut queue: VecDeque<(PackageId, Range, Option<PackageId>)> =
            self.requirements.packages.iter().map(|(id, r)| (id.clone(), r.clone(), None)).collect();

        while let Some((id, range, required_by)) = queue.pop_front() {
            if let Some(choice) = picked.get(&id) {
                if let Some(v) = choice
                    && !range.matches(v)
                {
                    out.conflicts.push(Conflict { id, picked: v.clone(), range, required_by });
                }
                continue;
            }
            let candidates: &[&Installed] = by_id.get(&id).map_or(&[], |v| v.as_slice());
            let fits_platform = |i: &Installed| options.platform.is_none_or(|p| i.manifest.supports(p));
            let locked = if options.update.includes(&id) {
                None
            } else {
                self.lock.packages.get(&id).filter(|l| range.matches(&l.version))
            };

            let (choice, from_lock) = if let Some(l) = locked {
                let Some(inst) = candidates.iter().copied().find(|i| i.version() == &l.version) else {
                    out.lock.packages.insert(id.clone(), l.clone());
                    out.missing.push(Missing {
                        id: id.clone(),
                        range,
                        required_by,
                        locked: Some(l.clone()),
                        reason: MissingReason::NotInstalled,
                    });
                    picked.insert(id, None);
                    continue;
                };
                let reason = if !inst.is_usable() {
                    Some(MissingReason::NotUsable(inst.problems()))
                } else if !fits_platform(inst) {
                    Some(MissingReason::NotForThisPlatform)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    out.lock.packages.insert(id.clone(), l.clone());
                    out.missing.push(Missing { id: id.clone(), range, required_by, locked: Some(l.clone()), reason });
                    picked.insert(id, None);
                    continue;
                }
                if inst.record.content_sha256 != l.sha256 {
                    out.mismatched.push(Mismatch {
                        id: id.clone(),
                        version: l.version.clone(),
                        locked: l.sha256,
                        installed: inst.record.content_sha256,
                    });
                }
                // The lock entry stays as it was, so a changed package keeps being reported.
                out.lock.packages.insert(id.clone(), l.clone());
                (inst, true)
            } else {
                let in_range: Vec<&Installed> =
                    candidates.iter().copied().filter(|i| range.matches(i.version())).collect();
                let best = in_range.iter().copied().rfind(|i| i.is_usable() && fits_platform(i));
                let Some(inst) = best else {
                    let reason = if candidates.is_empty() {
                        MissingReason::NotInstalled
                    } else if in_range.is_empty() {
                        MissingReason::NoMatchingVersion {
                            installed: candidates.iter().map(|i| i.version().clone()).collect(),
                        }
                    } else if in_range.iter().any(|i| i.is_usable()) {
                        MissingReason::NotForThisPlatform
                    } else {
                        MissingReason::NotUsable(in_range.last().map(|i| i.problems()).unwrap_or_default())
                    };
                    let locked = self.lock.packages.get(&id).cloned();
                    if let Some(l) = &locked {
                        out.lock.packages.insert(id.clone(), l.clone());
                    }
                    out.missing.push(Missing { id: id.clone(), range, required_by, locked, reason });
                    picked.insert(id, None);
                    continue;
                };
                out.lock.packages.insert(id.clone(), Locked::of(inst));
                (inst, false)
            };

            for (dep, dep_range) in &choice.manifest.dependencies {
                queue.push_back((dep.clone(), dep_range.clone(), Some(id.clone())));
            }
            picked.insert(id.clone(), Some(choice.version().clone()));
            out.resolved.push(Resolved {
                id,
                version: choice.version().clone(),
                path: choice.path.clone(),
                sha256: choice.record.content_sha256,
                required_by,
                from_lock,
            });
        }

        // A missing package's dependencies can't be known here; keep what the lock had for
        // packages not reached, so they aren't forgotten before it's installed.
        if !out.missing.is_empty() {
            for (id, l) in &self.lock.packages {
                if !picked.contains_key(id) {
                    out.lock.packages.entry(id.clone()).or_insert_with(|| l.clone());
                }
            }
        }
        out
    }

    /// Takes a resolution's lock file as the show's (save with [`Project::save`]). Whether it
    /// changed.
    pub fn apply(&mut self, resolution: &Resolution) -> bool {
        if self.lock == resolution.lock {
            return false;
        }
        self.lock = resolution.lock.clone();
        true
    }
}

/// Reads a JSON file, or `None` if it isn't there.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Json { path: path.to_path_buf(), message: e.to_string() }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io("read", path, e)),
    }
}
