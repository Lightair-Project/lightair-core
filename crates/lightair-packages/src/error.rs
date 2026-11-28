//! Errors and manifest problems, worded to be shown to the user as they are.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// What [`Error`] results carry.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a package couldn't be read, installed, removed or written.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or writing a file or folder failed.
    #[error("couldn't {action} {}: {source}", path.display())]
    Io {
        /// What was being done ("read", "write", "make the folder", …).
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The package, or a manifest written for one, has problems; each is a sentence.
    #[error("{}", describe(problems))]
    Invalid { problems: Vec<Problem> },
    /// The file or folder isn't a Lightair package at all.
    #[error("that isn't a Lightair package: {0}")]
    NotAPackage(String),
    /// The `.airpkg` (or `.lwidget`) can't be read as a zip, or contradicts itself.
    #[error("the package file is damaged: {0}")]
    Damaged(String),
    /// The package holds something that could write outside its folder (a path with `..`, an
    /// absolute path, a link) or a name some systems can't store.
    #[error("the package can't be opened safely: {0}")]
    Unsafe(String),
    /// The package is over one of the [`Limits`](crate::Limits).
    #[error("the package is too large: {0}")]
    TooLarge(String),
    /// The same id and version is installed with different contents.
    #[error(
        "{id} {version} is already installed with different contents; a changed package needs a new version number"
    )]
    VersionConflict { id: String, version: String },
    /// The package (or version) isn't in the store.
    #[error("{id} {version} isn't installed")]
    NotInstalled { id: String, version: String },
    /// A file's contents changed between reading the package and installing it.
    #[error("{path} changed while it was being installed; try again")]
    ChangedDuringInstall { path: String },
    /// A JSON file (a project's `packages.json`, an install record) couldn't be read.
    #[error("{} isn't valid: {message}", path.display())]
    Json { path: PathBuf, message: String },
    /// An export was asked to write into a folder that already holds files.
    #[error("{} already exists and isn't empty", .0.display())]
    NotEmpty(PathBuf),
}

impl Error {
    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Error {
        Error::Io { action, path: path.into(), source }
    }

    /// The manifest problems, when that's what this error is.
    pub fn problems(&self) -> &[Problem] {
        match self {
            Error::Invalid { problems } => problems,
            _ => &[],
        }
    }
}

fn describe(problems: &[Problem]) -> String {
    match problems {
        [] => "the package isn't valid".into(),
        [one] => one.to_string(),
        many => {
            let list: Vec<String> = many.iter().map(|p| p.to_string()).collect();
            format!("the package has {} problems: {}", many.len(), list.join("; "))
        }
    }
}

/// One thing wrong with a manifest or a package's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// There's no `lightair-package.json` where it should be.
    NoManifest,
    /// The manifest isn't JSON, or a field has the wrong type.
    BadJson(String),
    /// The manifest is larger than any manifest needs to be.
    ManifestTooLarge,
    /// A required field is missing or empty.
    MissingField(&'static str),
    /// An id isn't a reverse-domain name.
    BadId(String),
    /// The name is longer than the panel can show.
    NameTooLong,
    /// The version isn't semver.
    BadVersion { text: String, reason: String },
    /// A version range (the `lightair` field or a dependency's) doesn't parse.
    BadRange { what: String, text: String, reason: String },
    /// The package was made for a package API this Lightair doesn't have.
    ApiOutOfRange { range: String, api: String, newer: bool },
    /// A platform name Lightair doesn't know.
    UnknownPlatform(String),
    /// The package lists itself as a dependency.
    DependsOnItself,
    /// The package adds nothing.
    NoContents,
    /// A content kind Lightair doesn't know.
    UnknownKind { kind: String, path: String },
    /// A content kind of a tier this Lightair doesn't install (scripts, native code).
    NeedsNewerKind { kind: String, path: String },
    /// A path that isn't a plain relative path inside the package.
    BadPath { path: String, reason: String },
    /// The same content path is listed twice.
    DuplicateContent(String),
    /// A content path names nothing in the package.
    MissingContent(String),
    /// A data or shader package asks for permissions.
    PermissionsNotAllowed(Vec<String>),
    /// Two files whose names differ only by case (one would overwrite the other on macOS and
    /// Windows).
    CaseClash(String, String),
    /// A permission that isn't one Lightair knows, or is written wrongly.
    BadPermission { permission: String, reason: String },
    /// Full access asked for by a package with no helper.
    FullAccessNotAllowed,
    /// Permissions only helper programs may ask for (`computer.control`, `apps.run`, `speech.read`).
    HelperOnly(Vec<String>),
    /// A content entry other than a helper names programs.
    EntryNotAllowed(String),
    /// A helper's entry names a target Lightair doesn't know.
    UnknownTarget(String),
    /// A helper names no program this file holds.
    NoEntryInFile(String),
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::NoManifest => {
                write!(f, "there's no {} at the top of the package", crate::MANIFEST_FILE)
            }
            Problem::BadJson(e) => write!(f, "{} isn't valid JSON: {e}", crate::MANIFEST_FILE),
            Problem::ManifestTooLarge => write!(f, "{} is too large", crate::MANIFEST_FILE),
            Problem::MissingField(field) => write!(f, "the manifest has no {field}"),
            Problem::BadId(id) => write!(
                f,
                "\"{id}\" isn't a valid package id: use a reverse-domain name of lower-case letters, digits and hyphens, such as com.example.neon-pack"
            ),
            Problem::NameTooLong => write!(f, "the name is too long (100 characters at most)"),
            Problem::BadVersion { text, reason } => {
                write!(f, "the version \"{text}\" isn't a semantic version such as 1.2.0 ({reason})")
            }
            Problem::BadRange { what, text, reason } => {
                write!(f, "the {what} range \"{text}\" isn't valid ({reason})")
            }
            Problem::ApiOutOfRange { range, api, newer: true } => {
                write!(f, "it needs a newer Lightair: it was made for package API {range}, and this Lightair has {api}")
            }
            Problem::ApiOutOfRange { range, api, newer: false } => {
                write!(f, "it was made for an older Lightair (package API {range}); this Lightair has {api}")
            }
            Problem::UnknownPlatform(p) => {
                write!(f, "\"{p}\" isn't a platform Lightair knows (macos, windows, linux, ios, android)")
            }
            Problem::DependsOnItself => write!(f, "the package lists itself as a dependency"),
            Problem::NoContents => write!(f, "the package doesn't list any contents"),
            Problem::UnknownKind { kind, path } => {
                write!(f, "\"{kind}\" (at {path}) isn't a kind of content Lightair knows")
            }
            Problem::NeedsNewerKind { kind, path } => write!(
                f,
                "{path} is {kind} content, which needs a newer Lightair: this one installs data and shader packages only"
            ),
            Problem::BadPath { path, reason } => write!(f, "the path \"{path}\" {reason}"),
            Problem::DuplicateContent(path) => write!(f, "{path} is listed in the contents twice"),
            Problem::MissingContent(path) => {
                write!(f, "{path} is listed in the contents but isn't in the package")
            }
            Problem::PermissionsNotAllowed(p) => write!(
                f,
                "shader packages can't ask for permissions, and data packages only for the network hosts of their widgets and templates (this one asks for {})",
                p.join(", ")
            ),
            Problem::CaseClash(a, b) => {
                write!(f, "{a} and {b} differ only by capital letters, so one would overwrite the other")
            }
            Problem::BadPermission { permission, reason } => {
                write!(f, "\"{permission}\" isn't a permission Lightair knows ({reason})")
            }
            Problem::FullAccessNotAllowed => write!(f, "only helper programs can ask for full access"),
            Problem::HelperOnly(p) => write!(f, "only helper programs can ask for {}", p.join(", ")),
            Problem::EntryNotAllowed(path) => {
                write!(f, "{path} names programs to run, but only helpers can")
            }
            Problem::UnknownTarget(t) => write!(
                f,
                "\"{t}\" isn't a platform Lightair knows for a helper (macos-arm64, macos-x64, windows-x64, windows-arm64, linux-x64)"
            ),
            Problem::NoEntryInFile(path) => {
                write!(f, "the helper at {path} has no program for any platform in this file")
            }
        }
    }
}
