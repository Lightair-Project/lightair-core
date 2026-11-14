//! Where Lightair keeps things: its data (projects, preferences, caches) and, by default,
//! recordings.

use std::path::PathBuf;

/// Lightair's own data: `~/Library/Application Support/Lightair` on macOS,
/// `%APPDATA%\Lightair` on Windows.
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("Lightair")
}

/// Project packages.
pub fn projects_dir() -> PathBuf {
    data_dir().join("Projects")
}

/// Where recordings go unless a project says otherwise: `~/Movies/Lightair` on macOS, the
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
        assert!(super::projects_dir().ends_with("Lightair/Projects") || super::projects_dir().ends_with("Lightair\\Projects"));
        assert!(super::recordings_dir().file_name().is_some_and(|n| n == "Lightair"));
    }
}
