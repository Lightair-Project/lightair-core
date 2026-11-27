//! `lightair-pack publish`: a version as an immutable GitHub Release in a public repository
//! (catalog formats §3a), then registered with a catalog.

use std::fs;
use std::path::{Path, PathBuf};

use lightair_packages::{Limits, Package};
use serde_json::json;

use crate::args::Args;
use crate::http::{Http, github_token};

/// Lightair's catalog API.
const DEFAULT_API: &str = "https://packages.lightair.app";

pub fn run(args: &mut Args) -> Result<(), String> {
    let inputs = args.positionals();
    if inputs.is_empty() {
        return Err("usage: lightair-pack publish <folder or .airpkg …> --repo owner/name [--tag v1.2.0] [--api <url>] [--notes <text>] [--no-register]".into());
    }
    let repo = args.value("--repo").ok_or("name the GitHub repository with --repo owner/name")?;
    let token = github_token()?;
    let http = Http::new();

    // Build folders; take .airpkg files as they are.
    let scratch = std::env::temp_dir().join(format!("lightair-pack-publish-{}", std::process::id()));
    let mut files: Vec<PathBuf> = Vec::new();
    for i in &inputs {
        let p = Path::new(i);
        if p.is_dir() {
            for b in crate::build::build(p, &scratch)? {
                files.push(b.path);
            }
        } else {
            files.push(p.to_path_buf());
        }
    }
    let result = publish(&http, &token, &repo, &files, args);
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn publish(http: &Http, token: &str, repo: &str, files: &[PathBuf], args: &Args) -> Result<(), String> {
    let mut manifest = None;
    for f in files {
        let p = Package::open_with(f, Limits::every_tier()).map_err(|e| format!("{}: {e}", f.display()))?;
        let output = lightair_check::static_checks(f, "");
        if crate::print_findings(&output) && !args.has("--force") {
            return Err(format!("{} fails the checks; fix it first (or --force)", f.display()));
        }
        if manifest.is_none() {
            manifest = Some(p.manifest);
        }
    }
    let m = manifest.ok_or("nothing to publish")?;
    let tag = args.value("--tag").unwrap_or_else(|| format!("v{}", m.version));

    let info = http.get(&format!("https://api.github.com/repos/{repo}"), Some(token))?;
    if !info.ok() {
        return Err(format!("{repo}: {}", info.text()));
    }
    if info.json()["private"].as_bool() == Some(true) {
        return Err(format!("{repo} is private: packages must be in public repositories"));
    }

    let created = http.send_json(
        "POST",
        &format!("https://api.github.com/repos/{repo}/releases"),
        Some(token),
        &json!({
            "tag_name": tag,
            "name": format!("{} {}", m.name, m.version),
            "body": args.value("--notes").unwrap_or_default(),
            "draft": true,
        }),
    )?;
    if !created.ok() {
        return Err(format!("couldn't make the release: {}", created.text()));
    }
    let rel = created.json();
    let id = rel["id"].as_u64().ok_or("GitHub didn't say the release's id")?;
    let upload = |name: &str, kind: &str, bytes: &[u8]| -> Result<(), String> {
        let url = format!("https://uploads.github.com/repos/{repo}/releases/{id}/assets?name={name}");
        let r = http.upload(&url, token, kind, bytes)?;
        if r.ok() { Ok(()) } else { Err(format!("couldn't upload {name}: {}", r.text())) }
    };
    for f in files {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = fs::read(f).map_err(|e| e.to_string())?;
        upload(&name, "application/zip", &bytes)?;
        println!("Uploaded {name}");
    }
    upload(lightair_packages::MANIFEST_FILE, "application/json", &m.to_json())?;

    let published = http.send_json(
        "PATCH",
        &format!("https://api.github.com/repos/{repo}/releases/{id}"),
        Some(token),
        &json!({"draft": false}),
    )?;
    if !published.ok() {
        return Err(format!("couldn't publish the release: {}", published.text()));
    }
    let rel = published.json();
    println!("Published {}", rel["html_url"].as_str().unwrap_or_default());
    if rel["immutable"].as_bool() != Some(true) {
        println!(
            "note: the release isn't immutable. Turn on release immutability in the repository's settings (Settings › General › Releases) and publish a new version: catalogs refuse releases that can change."
        );
    }

    if args.has("--no-register") {
        return Ok(());
    }
    let api = args.value("--api").unwrap_or_else(|| DEFAULT_API.into());
    let r = http.send_json(
        "POST",
        &format!("{}/v1/versions", api.trim_end_matches('/')),
        Some(token),
        &json!({ "repository": repo, "tag": tag }),
    )?;
    if r.ok() {
        println!("Registered with {api}: the checks run now; follow them on the package's page.");
        if let Some(url) = r.json()["url"].as_str() {
            println!("{url}");
        }
        Ok(())
    } else {
        Err(format!("the release is published, and registering it with {api} failed: {}", r.text()))
    }
}
