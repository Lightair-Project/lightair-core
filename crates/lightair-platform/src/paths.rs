//! Where Lightair keeps things: its data (workspaces, preferences, caches) and, by default,
//! recordings.
//!
//! The answers come from a [`Paths`] backend. The desktop's own ([`DesktopPaths`]) is used unless
//! another is [`install`]ed first: a phone app installs one built from the folders only its
//! platform knows (its sandbox, an App Group), and a test can install one over a temporary
//! folder instead of writing to the real user folders.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Where things go.
pub trait Paths: Send + Sync {
    /// Lightair's own data.
    fn data_dir(&self) -> PathBuf;
    /// Workspace packages.
    fn workspaces_dir(&self) -> PathBuf {
        self.data_dir().join("Workspaces")
    }
    /// Recordings, unless a workspace says otherwise.
    fn recordings_dir(&self) -> PathBuf;
}

static BACKEND: OnceLock<Box<dyn Paths>> = OnceLock::new();

/// Uses `backend` for every path from now on. Only the first call counts, and only if it comes
/// before the first path is asked for; it returns `false` otherwise.
pub fn install(backend: Box<dyn Paths>) -> bool {
    BACKEND.set(backend).is_ok()
}

fn backend() -> &'static dyn Paths {
    BACKEND.get_or_init(|| Box::new(DesktopPaths)).as_ref()
}

/// Lightair's own data: `~/Library/Application Support/Lightair` on macOS,
/// `%APPDATA%\Lightair` on Windows (with the desktop backend).
pub fn data_dir() -> PathBuf {
    backend().data_dir()
}

/// Workspace packages.
pub fn workspaces_dir() -> PathBuf {
    backend().workspaces_dir()
}

/// Where recordings go unless a workspace says otherwise: `~/Movies/Lightair` on macOS, the
/// Videos folder's `Lightair` on Windows (with the desktop backend).
pub fn recordings_dir() -> PathBuf {
    backend().recordings_dir()
}

/// The desktop's folders, from the user's home.
pub struct DesktopPaths;

impl Paths for DesktopPaths {
    fn data_dir(&self) -> PathBuf {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("Lightair")
    }

    /// Before the rename the folder was `Projects`: on macOS it's renamed the first time it's
    /// looked for, a link left in its place (files elsewhere name paths in it); elsewhere it's
    /// used as it is.
    fn workspaces_dir(&self) -> PathBuf {
        let new = self.data_dir().join("Workspaces");
        let old = self.data_dir().join("Projects");
        if new.exists() || !old.is_dir() || old.is_symlink() {
            return new;
        }
        #[cfg(unix)]
        if std::fs::rename(&old, &new).is_ok() {
            let _ = std::os::unix::fs::symlink("Workspaces", &old);
            return new;
        }
        old
    }

    fn recordings_dir(&self) -> PathBuf {
        dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("Lightair")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_under_the_user() {
        let home = dirs::home_dir().unwrap();
        let d = DesktopPaths;
        assert!(d.data_dir().starts_with(&home) || cfg!(windows));
        assert!(d.workspaces_dir().ends_with("Lightair/Workspaces") || d.workspaces_dir().ends_with("Lightair\\Workspaces"));
        assert!(d.recordings_dir().file_name().is_some_and(|n| n == "Lightair"));
    }

    struct Sandbox(PathBuf);

    impl Paths for Sandbox {
        fn data_dir(&self) -> PathBuf {
            self.0.join("Lightair")
        }
        fn recordings_dir(&self) -> PathBuf {
            self.0.join("Recordings")
        }
    }

    #[test]
    fn an_installed_backend_answers_and_only_the_first_counts() {
        let root = std::env::temp_dir().join("lightair-paths-test");
        assert!(install(Box::new(Sandbox(root.clone()))));
        assert!(!install(Box::new(DesktopPaths)));
        assert_eq!(data_dir(), root.join("Lightair"));
        assert_eq!(workspaces_dir(), root.join("Lightair").join("Workspaces"));
        assert_eq!(recordings_dir(), root.join("Recordings"));
    }
}
