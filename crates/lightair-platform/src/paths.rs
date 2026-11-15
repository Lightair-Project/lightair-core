//! Where Lightair keeps things: its data (workspaces, preferences, caches) and, by default,
//! recordings.

use std::path::PathBuf;

/// Lightair's own data: `~/Library/Application Support/Lightair` on macOS,
/// `%APPDATA%\Lightair` on Windows.
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("Lightair")
}

/// Workspace packages. Before the rename the folder was `Projects`: on macOS it's renamed the
/// first time it's looked for, a link left in its place (files elsewhere name paths in it);
/// elsewhere it's used as it is.
pub fn workspaces_dir() -> PathBuf {
    let new = data_dir().join("Workspaces");
    let old = data_dir().join("Projects");
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

/// Where recordings go unless a workspace says otherwise: `~/Movies/Lightair` on macOS, the
/// Videos folder's `Lightair` on Windows.
pub fn recordings_dir() -> PathBuf {
    dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("Lightair")
}

#[cfg(test)]
mod tests {
    #[test]
    fn folders_are_under_the_user() {
        let home = dirs::home_dir().unwrap();
        assert!(super::data_dir().starts_with(&home) || cfg!(windows));
        assert!(super::workspaces_dir().ends_with("Lightair/Workspaces") || super::workspaces_dir().ends_with("Lightair\\Workspaces"));
        assert!(super::recordings_dir().file_name().is_some_and(|n| n == "Lightair"));
    }
}
