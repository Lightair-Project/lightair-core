//! Lightair packages: versioned bundles of content (widgets, looks, transitions, templates,
//! automation recipes, effect presets, media, shaders and more) that are installed once per user,
//! side by side by version, and recorded by the shows that use them.
//!
//! A package is a folder, or a `.airpkg` file (a zip of that folder), with a manifest at its
//! root, [`MANIFEST_FILE`] (`lightair-package.json`). This crate is pure Rust and runs on every
//! platform; it doesn't draw anything or know about scenes. It covers:
//!
//! - **The manifest** ([`Manifest`]): parsing, structural checks with plain-English
//!   [`Problem`]s, and the check that this Lightair can use it ([`Manifest::check_supported`]):
//!   the package API range and the content tiers ([`Tier`]). This build installs data and shader
//!   packages; scripts and native code are refused as needing a newer Lightair.
//! - **Reading packages** ([`Package::open`]) from a folder or a `.airpkg`, refusing paths that
//!   leave the package, links and oversized contents ([`Limits`]), with the SHA-256 of the
//!   archive, of every file and of the whole content ([`Package::content_sha256`]).
//! - **The installed store** ([`Store`]): `<data dir>/Packages/<id>/<version>/`, with install,
//!   list, verify and remove. The data directory is passed in by the app.
//! - **A project's packages** ([`Project`]): `packages.json` (id → version range) and
//!   `packages-lock.json` (id → exact version, checksum, source and size), resolved against the
//!   store ([`Project::resolve`]), with what's missing reported.
//! - **Import and export** ([`lwidget`], [`export`]): a `.lwidget` installs as a package of one
//!   widget, and a set of files (scenes, widgets, looks, presets, media) is written out as a
//!   package folder or `.airpkg` with its manifest.
//!
//! Errors are [`Error`]s whose messages can be shown to the user as they are.

mod error;
pub mod export;
mod hash;
pub mod lwidget;
mod manifest;
mod package;
mod paths;
mod project;
mod store;

pub use error::{Error, Problem, Result};
pub use hash::Sha256;
pub use manifest::{Content, Kind, Manifest, PACKAGE_API, PackageId, Platform, Range, Tier, package_api};
pub use package::{FileEntry, Limits, Package, content_sha256};
pub use paths::slug;
pub use project::{
    Conflict, LOCK_FILE, Lock, Locked, Mismatch, Missing, MissingReason, PROJECT_FILE, Project, Requirements,
    Resolution, ResolveOptions, Resolved, Update,
};
pub use store::{Damage, Install, InstallRecord, Installed, RECORD_FILE, STORE_FOLDER, Source, Store};

/// The manifest's file name, at the root of every package.
pub const MANIFEST_FILE: &str = "lightair-package.json";

/// The extension of a package file (a zip of a package folder).
pub const ARCHIVE_EXTENSION: &str = "airpkg";
