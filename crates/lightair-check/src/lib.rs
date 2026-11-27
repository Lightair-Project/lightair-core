//! The checks a Lightair package passes before a catalog lists it (design: `lightair-docs/
//! lightair/docs/plans/plugins.md` §4.3–§4.4; report format: `catalog-formats.md` §8, §10).
//!
//! - [`static_checks`]: the static checks: the archive and manifest, every file's real type and
//!   where it may be, images decoded and normalised, fonts and models read, the code's network
//!   hosts and runtime-built code against the declared permissions, helpers' programs (their
//!   imports, linked libraries and entitlements) against what a sandboxed helper may do, keys
//!   left in by mistake, and the licence.
//! - [`run`] and [`helper_run`]: the headless run and a helper's run in the app's sandbox. Their
//!   hooks are here; the runs themselves come with the desktop's headless renderer and helper
//!   sandbox (plugins plan, steps 5 and 7).
//! - [`normalise`]: the image normalisation `lightair-pack build` applies and the check expects.

pub mod binary;
mod checks;
pub mod code;
pub mod filetype;
pub mod normalise;
pub mod report;

use std::path::Path;

pub use report::{Check, Code, Finding, Measured, Observed, Output, Preview, Verdict};

/// The version every check reports (moves up when a check changes what it accepts).
pub const CHECKS_VERSION: &str = "1.0.0";

/// Every static check on the package at `path` (a `.airpkg` or a package folder); `url` is
/// where it came from, for the measured file.
pub fn static_checks(path: &Path, url: &str) -> Output {
    checks::run(path, url)
}

/// The headless run for tiers 1–3 (design §4.4).
///
/// HOOK: the run needs `lightair-render` headless on a software GPU (templates, looks,
/// transitions, avatars, with previews), headless Chromium behind a recording proxy (widgets),
/// a fake studio (recipes) and the extension runtime (scripts). Until those are wired here it
/// reports that it didn't run, as a note, so a catalog can tell a run that passed from one that
/// didn't happen.
pub fn run(path: &Path, deep: bool) -> Output {
    let mut c = Check::new(if deep { "run-deep" } else { "run" });
    match lightair_packages::Package::open_with(path, lightair_packages::Limits::every_tier()) {
        Ok(_) => c.note("the headless run isn't built yet: nothing was rendered or exercised", None),
        Err(e) => c.fail(e.to_string(), None),
    }
    Output { checks: vec![c], observed: Some(Observed::default()), ..Output::default() }
}

/// A helper's run in the app's own sandbox under a fake Lightair, on `platform` (design §4.4).
///
/// HOOK: needs the desktop's helper launcher and sandbox (plugins plan, step 5). Reports that it
/// didn't run, as a note.
pub fn helper_run(path: &Path, platform: &str) -> Output {
    let mut c = Check::new(&format!("helper-run-{platform}"));
    match lightair_packages::Package::open_with(path, lightair_packages::Limits::every_tier()) {
        Ok(p) if !p.manifest.contents.iter().any(|c| c.kind == lightair_packages::Kind::Helper) => {
            c.fail("the package has no helper to run", None)
        }
        Ok(_) => c.note("the helper's sandboxed run isn't built yet: the helper wasn't started", None),
        Err(e) => c.fail(e.to_string(), None),
    }
    Output { checks: vec![c], observed: Some(Observed::default()), ..Output::default() }
}
