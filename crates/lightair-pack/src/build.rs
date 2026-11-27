//! `lightair-pack build`: a package folder to `.airpkg` files with normalised images, one per
//! helper target when the package has a helper for several, checked afterwards.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use lightair_check::filetype;
use lightair_check::normalise;
use lightair_packages::{Kind, Limits, Package};

use crate::args::Args;

/// What one build wrote.
pub struct Built {
    pub path: PathBuf,
    /// The target (`any`, `macos-arm64`, …).
    pub target: String,
}

pub fn run(args: &mut Args) -> Result<(), String> {
    let folder = args.positionals().first().cloned().ok_or("usage: lightair-pack build <folder> [--out <folder>]")?;
    let out = args.path("--out").unwrap_or_else(|| PathBuf::from("."));
    let built = build(Path::new(&folder), &out)?;
    let mut failed = false;
    for b in &built {
        println!("Built {} ({})", b.path.display(), b.target);
        let output = lightair_check::static_checks(&b.path, "");
        failed |= crate::print_findings(&output);
    }
    if failed { Err("the built package doesn't pass the checks yet".into()) } else { Ok(()) }
}

/// Builds the package folder `folder` into `out_dir`.
pub fn build(folder: &Path, out_dir: &Path) -> Result<Vec<Built>, String> {
    let package = Package::open_with(folder, Limits::every_tier()).map_err(|e| e.to_string())?;
    let m = &package.manifest;
    fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    if let (Ok(f), Ok(o)) = (fs::canonicalize(folder), fs::canonicalize(out_dir))
        && o.starts_with(&f)
    {
        return Err(
            "write the built files outside the package's folder (--out), or the next build packs them too".into()
        );
    }
    let base = format!("{}-{}", lightair_packages::slug(&m.name), m.version);

    // Targets: one file per helper target when there are several, else one file.
    let entries: Vec<(String, String)> = m
        .contents
        .iter()
        .filter(|c| c.kind == Kind::Helper)
        .flat_map(|c| c.entry.iter().map(|(t, p)| (t.clone(), p.clone())))
        .collect();
    let targets: BTreeSet<String> = entries.iter().map(|(t, _)| t.clone()).collect();
    let plan: Vec<(String, BTreeSet<String>)> = if targets.len() > 1 {
        targets
            .iter()
            .map(|t| {
                let mine: Vec<&String> = entries.iter().filter(|(tt, _)| tt == t).map(|(_, p)| p).collect();
                let mut skip = BTreeSet::new();
                for (other, program) in &entries {
                    if other == t {
                        continue;
                    }
                    let dir = program.rsplit_once('/').map(|(d, _)| format!("{d}/"));
                    match dir {
                        Some(d) if !mine.iter().any(|p| p.starts_with(&d)) => skip.insert(d),
                        _ => skip.insert(program.clone()),
                    };
                }
                (t.clone(), skip)
            })
            .collect()
    } else {
        vec![(targets.into_iter().next().unwrap_or_else(|| "any".into()), BTreeSet::new())]
    };
    let split = plan.len() > 1;

    let mut built = Vec::new();
    for (target, skip) in plan {
        let stage = out_dir.join(format!(".lightair-pack-{}-{}", std::process::id(), target));
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
        let result = (|| -> Result<PathBuf, String> {
            for rel in package.files.keys() {
                if skip.iter().any(|s| rel == s || (s.ends_with('/') && rel.starts_with(s.as_str()))) {
                    continue;
                }
                let bytes = package.read(rel).map_err(|e| e.to_string())?;
                let family = filetype::sniff(&bytes);
                let bytes = if family.is_image() {
                    let n = normalise::normalise(family, &bytes).map_err(|e| format!("{rel} {e}"))?;
                    if n != bytes {
                        println!("  normalised {rel} ({} → {} bytes)", bytes.len(), n.len());
                    }
                    n
                } else {
                    bytes
                };
                let dest = stage.join(rel);
                if let Some(d) = dest.parent() {
                    fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
            }
            let name = if split { format!("{base}-{target}.airpkg") } else { format!("{base}.airpkg") };
            let out = out_dir.join(name);
            lightair_packages::export::write_archive_with(&stage, &out, Limits::every_tier())
                .map_err(|e| e.to_string())?;
            Ok(out)
        })();
        let _ = fs::remove_dir_all(&stage);
        built.push(Built { path: result?, target });
    }
    Ok(built)
}
