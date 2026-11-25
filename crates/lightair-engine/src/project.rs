//! Studio's projects on disk: each a lightair-store package in
//! `<data>/Studio/Projects/<id>/`, as the desktop keeps its workspaces.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use lightair_model::Workspace;

use crate::device::DeviceInfo;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("There's no project called that any more.")]
    NotFound,
    #[error("That folder isn't a Lightair project: {0}")]
    NotAProject(String),
    #[error("The project couldn't be read or written: {0}")]
    Store(String),
}

impl From<lightair_store::StoreError> for ProjectError {
    fn from(e: lightair_store::StoreError) -> Self {
        ProjectError::Store(e.to_string())
    }
}

impl From<std::io::Error> for ProjectError {
    fn from(e: std::io::Error) -> Self {
        ProjectError::Store(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub id: String,
    pub name: String,
    pub modified_ms: u64,
    pub scenes: u32,
    pub formats: Vec<(String, String, u32, u32, u32)>,
    /// Its picture (`thumbnail.jpg`), once there is one.
    pub thumbnail: Option<PathBuf>,
}

pub struct Projects {
    root: PathBuf,
    pub shared: PathBuf,
    pub cache: PathBuf,
    pub device: DeviceInfo,
}

impl Projects {
    /// `data`: the app's data folder; `shared`: where recordings and the Studio index go (the
    /// App Group container or the provider's folder; `data` without one).
    pub fn open(
        data: &Path,
        shared: Option<&Path>,
        cache: &Path,
        device: DeviceInfo,
    ) -> Result<Projects, ProjectError> {
        let root = data.join("Studio").join("Projects");
        fs::create_dir_all(&root)?;
        let shared = shared.unwrap_or(data).to_path_buf();
        // recordings an ended app left out of the Studio index (the Editor lists them again)
        match crate::record::recover(&shared) {
            Ok(0) => {}
            Ok(n) => tracing::info!(n, "recordings put back in the Studio index"),
            Err(e) => tracing::warn!(%e, "the Studio index"),
        }
        Ok(Projects { root, shared, cache: cache.to_path_buf(), device })
    }

    /// Studio's own folder (what's kept for every project: effect presets).
    pub fn app_dir(&self) -> PathBuf {
        self.root.parent().map_or_else(|| self.root.clone(), Path::to_path_buf)
    }

    pub fn dir(&self, id: &str) -> Result<PathBuf, ProjectError> {
        // ids are ours (uuids): nothing that climbs out of the folder
        if id.is_empty() || id.contains(['/', '\\', '.']) {
            return Err(ProjectError::NotFound);
        }
        let d = self.root.join(id);
        if lightair_store::is_workspace(&d) { Ok(d) } else { Err(ProjectError::NotFound) }
    }

    fn summary(id: &str, dir: &Path) -> Option<Summary> {
        let ws = lightair_store::portable::load_current(dir).ok()?;
        let modified_ms = fs::metadata(lightair_store::manifest(dir))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as u64);
        Some(Summary {
            id: id.to_string(),
            name: ws.name.clone(),
            modified_ms,
            scenes: ws.scenes.len() as u32,
            formats: ws
                .canvases
                .iter()
                .map(|c| (c.id.to_string(), c.name.clone(), c.width, c.height, c.fps_num / c.fps_den.max(1)))
                .collect(),
            thumbnail: Some(dir.join(crate::thumb::FILE)).filter(|p| p.is_file()),
        })
    }

    /// Newest first.
    pub fn list(&self) -> Vec<Summary> {
        let mut out: Vec<Summary> = fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let id = e.file_name().to_str()?.to_string();
                Self::summary(&id, &e.path())
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
        out
    }

    pub fn get(&self, id: &str) -> Result<Summary, ProjectError> {
        Self::summary(id, &self.dir(id)?).ok_or(ProjectError::NotFound)
    }

    fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn write_new(&self, ws: &Workspace) -> Result<Summary, ProjectError> {
        let id = Self::new_id();
        let dir = self.root.join(&id);
        lightair_store::save(&dir, ws)?;
        crate::thumb::render_soon(&dir);
        Self::summary(&id, &dir).ok_or(ProjectError::NotFound)
    }

    pub fn create(&self, name: &str) -> Result<Summary, ProjectError> {
        let (short, fps) = self.device.default_format();
        let name = if name.trim().is_empty() { "My studio" } else { name.trim() };
        self.write_new(&crate::templates::project(name, short, fps))
    }

    /// A desktop project's portable copy (lightair-store `portable::export`): its folder is
    /// copied in whole (its `assets/` included), under a new id.
    pub fn import_portable(&self, from: &Path) -> Result<Summary, ProjectError> {
        if !lightair_store::is_workspace(from) {
            return Err(ProjectError::NotAProject(from.display().to_string()));
        }
        let ws = lightair_store::portable::load_current(from)?;
        let id = Self::new_id();
        let dir = self.root.join(&id);
        copy_dir(from, &dir)?;
        // the copy as one snapshot (no journal of the desktop's left to replay), with a picture
        // of it of its own
        lightair_store::save(&dir, &ws)?;
        let _ = fs::remove_file(dir.join(crate::thumb::FILE));
        crate::thumb::render_soon(&dir);
        Self::summary(&id, &dir).ok_or(ProjectError::NotFound)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), ProjectError> {
        let dir = self.dir(id)?;
        let mut ws = lightair_store::portable::load_current(&dir)?;
        ws.name = name.trim().to_string();
        lightair_store::save(&dir, &ws)?;
        Ok(())
    }

    pub fn duplicate(&self, id: &str) -> Result<Summary, ProjectError> {
        let from = self.dir(id)?;
        let mut ws = lightair_store::portable::load_current(&from)?;
        ws.name = format!("{} copy", ws.name);
        let new = Self::new_id();
        let dir = self.root.join(&new);
        copy_dir(&from, &dir)?;
        lightair_store::save(&dir, &ws)?;
        Self::summary(&new, &dir).ok_or(ProjectError::NotFound)
    }

    pub fn delete(&self, id: &str) -> Result<(), ProjectError> {
        fs::remove_dir_all(self.dir(id)?)?;
        Ok(())
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_rename_duplicate_delete_import() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
        let a = p.create("Walk").unwrap();
        assert_eq!(a.name, "Walk");
        assert_eq!(a.scenes, 1);
        assert_eq!(a.formats[0].1, "Vertical");
        assert_eq!((a.formats[0].2, a.formats[0].3), (720, 1280));
        p.rename(&a.id, "Night walk").unwrap();
        let b = p.duplicate(&a.id).unwrap();
        assert_eq!(b.name, "Night walk copy");
        assert_eq!(p.list().len(), 2);
        p.delete(&b.id).unwrap();
        assert_eq!(p.list().len(), 1);
        assert!(p.dir("../x").is_err());
        // a portable copy of it comes back as a new project
        let out = tmp.path().join("export");
        lightair_store::portable::export(&p.dir(&a.id).unwrap(), &out).unwrap();
        let c = p.import_portable(&out).unwrap();
        assert_eq!(c.name, "Night walk");
        assert_ne!(c.id, a.id);
        assert!(p.import_portable(tmp.path()).is_err());
    }
}
