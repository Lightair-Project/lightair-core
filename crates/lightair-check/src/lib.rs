//! The checks a Lightair package passes before a catalog lists it (design: `lightair-docs/
//! lightair/docs/plans/plugins.md` §4.3–§4.4; report format: `catalog-formats.md` §8, §10).
//!
//! - [`static_checks`]: the static checks: the archive and manifest, every file's real type and
//!   where it may be, images decoded and normalised, fonts and models read, the code's network
//!   hosts and runtime-built code against the declared permissions, helpers' programs (their
//!   imports, linked libraries and entitlements) against what a sandboxed helper may do, keys
//!   left in by mistake, and the licence.
//! - [`run`]: the headless run (templates and transitions drawn, shaders compiled, media probed,
//!   recipes dry-run); [`widgets::write_pages`]: the pages the widget run (headless Chromium, in
//!   the check workflows) opens; [`helper_run`]: a helper's run in the app's own sandbox, through
//!   the desktop's `lightair-helper-run`.
//! - [`normalise`]: the image normalisation `lightair-pack build` applies and the check expects.

pub mod binary;
mod checks;
pub mod code;
pub mod filetype;
pub mod normalise;
pub mod report;
mod run;
pub mod widgets;

use std::path::Path;

pub use report::{Check, Code, Finding, Measured, Observed, Output, Preview, Verdict};

/// The version every check reports (moves up when a check changes what it accepts).
pub const CHECKS_VERSION: &str = "1.0.0";

/// Every static check on the package at `path` (a `.airpkg` or a package folder); `url` is
/// where it came from, for the measured file.
pub fn static_checks(path: &Path, url: &str) -> Output {
    checks::run(path, url)
}

/// The headless run for tiers 1–3 (design §4.4): templates and transitions drawn with
/// `lightair-render` on whatever GPU adapter is here (frames checked, previews made, frame time
/// against a budget measured on the same adapter), shaders compiled, video and sound probed with
/// FFmpeg, LUTs read, recipes walked and dry-run against a fake studio. Widgets run in headless
/// Chromium (the check workflow's widget run); looks, avatars and scripts say they can't run here.
/// `previews`: also write the previews there as files.
pub fn run(path: &Path, deep: bool, previews: Option<&Path>) -> Output {
    run::run(path, deep, previews)
}

/// A helper's run in the app's own sandbox under a fake Lightair, on `platform` (design §4.4).
///
/// The run itself is the desktop's: `lightair-helper-run` (built from the desktop repo, with
/// `lightair-sandbox`), a separate program this one starts with the package's helper unpacked:
/// `runner`, else `LIGHTAIR_HELPER_RUN`, else beside this program, else on the `PATH`. Without it
/// the run reports, as a note, that it didn't happen.
pub fn helper_run(path: &Path, platform: &str, runner: Option<&Path>) -> Output {
    let mut c = Check::new(&format!("helper-run-{platform}"));
    let done = |c: Check| Output { checks: vec![c], observed: Some(Observed::default()), ..Output::default() };
    let package = match lightair_packages::Package::open_with(path, lightair_packages::Limits::every_tier()) {
        Ok(p) => p,
        Err(e) => {
            c.fail(e.to_string(), None);
            return done(c);
        }
    };
    let entry = package
        .manifest
        .contents
        .iter()
        .filter(|c| c.kind == lightair_packages::Kind::Helper)
        .find_map(|c| c.entry.get(platform).cloned());
    let Some(entry) = entry else {
        if package.manifest.contents.iter().any(|c| c.kind == lightair_packages::Kind::Helper) {
            c.fail(format!("the helper has no program for {platform}"), None);
        } else {
            c.fail("the package has no helper to run", None);
        }
        return done(c);
    };
    let Some(runner) = find_runner(runner) else {
        c.note("the desktop's helper runner (lightair-helper-run) isn't here, so the helper wasn't started", None);
        return done(c);
    };
    let dir = std::env::temp_dir().join(format!("lightair-check-helper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = package.extract_to(&dir) {
        c.fail(format!("couldn't unpack the package: {e}"), None);
        return done(c);
    }
    let json = dir.with_extension("json");
    let mut cmd = std::process::Command::new(&runner);
    cmd.arg("--package-dir").arg(&dir).arg("--entry").arg(&entry).arg("--id").arg(package.manifest.id.as_str());
    cmd.arg("--platform").arg(platform).arg("--json").arg(&json);
    for p in &package.manifest.permissions {
        cmd.arg("--permission").arg(p);
    }
    if let Ok(l) = std::env::var("LIGHTAIR_SANDBOX") {
        cmd.arg("--launcher").arg(l);
    }
    let status = cmd.status();
    let out = std::fs::read(&json).ok().and_then(|b| serde_json::from_slice::<Output>(&b).ok());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&json);
    match (status, out) {
        (Ok(_), Some(out)) => out,
        (Ok(s), None) => {
            c.fail(format!("the helper runner gave no report ({s})"), None);
            done(c)
        }
        (Err(e), _) => {
            c.fail(format!("the helper runner couldn't start: {e}"), None);
            done(c)
        }
    }
}

fn find_runner(given: Option<&Path>) -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "lightair-helper-run.exe" } else { "lightair-helper-run" };
    let candidates = [
        given.map(Path::to_path_buf),
        std::env::var_os("LIGHTAIR_HELPER_RUN").map(Into::into),
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(exe))),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file()).or_else(|| {
        std::env::var_os("PATH")
            .and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(exe)).find(|p| p.is_file()))
    })
}
