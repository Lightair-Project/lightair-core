//! `lightair-pack catalog …`: community catalogs (design §3.7, catalog formats §2–§5). A folder
//! anyone can serve over HTTPS (GitHub Pages is enough), signed with the catalog's own key.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use lightair_packages::catalog::{
    Attestation, AttestedFile, CheckRef, Envelope, FORMAT, FileRef, ListedVersion, Listing, Revoked, RevokedEntry,
    Role, Roles, Root, SecretKey, Shard, Snapshot, SourceRef, Target, Timestamp, format_rfc3339, now_unix, shard_of,
};
use lightair_packages::{Limits, Package, Permission, Sha256};

use crate::args::Args;
use crate::http::{Http, github_token};
use crate::keys;

const DAY: u64 = 86_400;

pub fn run(args: &mut Args) -> Result<(), String> {
    match args.next_word().as_deref() {
        Some("init") => init(args),
        Some("add") => add(args),
        Some("sign") => {
            let (dir, key) = dir_and_key(args)?;
            resign(&dir, &key, args, false)?;
            println!("Signed {} again.", dir.display());
            Ok(())
        }
        Some("revoke") => revoke(args),
        Some("yank") => yank(args),
        _ => Err("usage: lightair-pack catalog init|add|sign|revoke|yank <folder> …".into()),
    }
}

fn dir_and_key(args: &Args) -> Result<(PathBuf, SecretKey), String> {
    let dir = args.positionals().first().map(PathBuf::from).ok_or("name the catalog's folder")?;
    let key = keys::load_secret(&args.path("--key").ok_or("give the catalog's key with --key <file>")?)?;
    Ok((dir, key))
}

/// Days the timestamp and the list stay valid (`--days`, 7 by default for a community catalog:
/// a scheduled Action runs `catalog sign` more often than that).
fn days(args: &Args) -> u64 {
    args.value("--days").and_then(|d| d.parse().ok()).unwrap_or(7).max(1)
}

fn init(args: &mut Args) -> Result<(), String> {
    let dir = args
        .positionals()
        .first()
        .map(PathBuf::from)
        .ok_or("usage: lightair-pack catalog init <folder> --name <name> [--key <file>]")?;
    let name = args.value("--name").ok_or("give the catalog a --name")?;
    if dir.join("root/1.json").exists() {
        return Err(format!("{} already holds a catalog", dir.display()));
    }
    let key = match args.path("--key").filter(|p| p.exists()) {
        Some(p) => keys::load_secret(&p)?,
        None => {
            let out = args.path("--key").unwrap_or_else(|| PathBuf::from("catalog-key.json"));
            if let (Ok(d), Ok(o)) = (
                fs::canonicalize(dir.parent().unwrap_or(Path::new("."))),
                fs::canonicalize(out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))),
            ) && o.starts_with(d.join(dir.file_name().unwrap_or_default()))
            {
                return Err("keep the key outside the catalog's folder: the folder is published".into());
            }
            let key = keys::generate()?;
            keys::write_private(&out, &serde_json::to_vec_pretty(&key.to_file()).unwrap())?;
            println!(
                "New catalog key {} written to {} (keep it secret, never in the repository).",
                key.public().id(),
                out.display()
            );
            key
        }
    };
    fs::create_dir_all(dir.join("root")).map_err(|e| e.to_string())?;
    fs::create_dir_all(dir.join("shards")).map_err(|e| e.to_string())?;
    let id = key.public().id();
    let role = Role { keys: vec![id.clone()], threshold: 1 };
    let root = Root {
        format: FORMAT,
        version: 1,
        expires: format_rfc3339(now_unix() + 5 * 365 * DAY),
        name: name.clone(),
        keys: BTreeMap::from([(id, key.public().entry())]),
        roles: Roles { root: role.clone(), catalog: role.clone(), timestamp: role },
    };
    write(&dir.join("root/1.json"), &Envelope::sign_json("root", &root, &[&key]).to_json())?;
    for d in "0123456789abcdef".chars() {
        let path = dir.join(format!("shards/{d}.json"));
        write(&path, &shard_bytes(&Shard { format: FORMAT, packages: BTreeMap::new() }))?;
    }
    let revoked = Revoked { format: FORMAT, version: 1, entries: Vec::new() };
    write(&dir.join("revoked.json"), &Envelope::sign_json("revoked", &revoked, &[&key]).to_json())?;
    write(&dir.join(".nojekyll"), b"")?;
    resign(&dir, &key, args, true)?;
    println!("Catalog \"{name}\" made in {}.", dir.display());
    println!("Its key's fingerprint, for your catalog's page: {}", key.public().fingerprint());
    Ok(())
}

fn shard_bytes(s: &Shard) -> Vec<u8> {
    let mut b = serde_json::to_vec_pretty(s).unwrap();
    b.push(b'\n');
    b
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

fn read_env<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    let Ok(bytes) = fs::read(path) else { return Ok(None) };
    let env = Envelope::from_json(&bytes).map_err(|e| e.to_string())?;
    let payload = env.payload_bytes().map_err(|e| e.to_string())?;
    serde_json::from_slice(&payload).map(Some).map_err(|e| format!("{} isn't valid: {e}", path.display()))
}

/// Writes a new snapshot (from the shards and revoked list on disk) and a new timestamp.
fn resign(dir: &Path, key: &SecretKey, args: &Args, first: bool) -> Result<(), String> {
    let days = days(args);
    let now = now_unix();
    let old_snap: Option<Snapshot> = if first { None } else { read_env(&dir.join("snapshot.json"))? };
    let old_ts: Option<Timestamp> = if first { None } else { read_env(&dir.join("timestamp.json"))? };
    let revoked_bytes = fs::read(dir.join("revoked.json")).map_err(|e| e.to_string())?;
    let revoked: Revoked = read_env(&dir.join("revoked.json"))?.ok_or("the catalog has no revoked.json")?;
    let mut shards = BTreeMap::new();
    for d in "0123456789abcdef".chars() {
        let bytes = fs::read(dir.join(format!("shards/{d}.json"))).map_err(|e| e.to_string())?;
        shards.insert(d.to_string(), FileRef::of(&bytes, None));
    }
    let snapshot = Snapshot {
        format: FORMAT,
        version: old_snap.map_or(1, |s| s.version + 1),
        expires: format_rfc3339(now + days.max(7) * 4 * DAY),
        shards,
        revoked: FileRef::of(&revoked_bytes, Some(revoked.version)),
    };
    let snap_bytes = Envelope::sign_json("snapshot", &snapshot, &[key]).to_json();
    write(&dir.join("snapshot.json"), &snap_bytes)?;
    let ts = Timestamp {
        format: FORMAT,
        version: old_ts.map_or(1, |t| t.version + 1),
        expires: format_rfc3339(now + days * DAY),
        snapshot: FileRef::of(&snap_bytes, Some(snapshot.version)),
    };
    write(&dir.join("timestamp.json"), &Envelope::sign_json("timestamp", &ts, &[key]).to_json())?;
    Ok(())
}

/// One package file to add: a local path and where people download it.
struct Input {
    path: PathBuf,
    url: String,
}

fn add(args: &mut Args) -> Result<(), String> {
    let (dir, key) = dir_and_key(args)?;
    let mut inputs = Vec::new();
    let mut source = None;
    let mut publisher = args.value("--publisher");
    let scratch = std::env::temp_dir().join(format!("lightair-pack-{}", std::process::id()));
    if let Some(release) = args.value("--release") {
        let (inp, src, owner) = from_release(&release, &scratch)?;
        inputs = inp;
        source = Some(src);
        publisher.get_or_insert(owner);
    } else {
        let files = args.values("--file");
        let urls = args.values("--url");
        if files.is_empty() || files.len() != urls.len() {
            return Err("give --release owner/repo@tag, or each --file with its --url".into());
        }
        for (f, u) in files.into_iter().zip(urls) {
            inputs.push(Input { path: PathBuf::from(f), url: u });
        }
        if let (Some(r), Some(c)) = (args.value("--repository"), args.value("--commit")) {
            source = Some(SourceRef { repository: r, commit: c });
        }
    }
    let result = add_inputs(&dir, &key, args, &inputs, source, publisher);
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn add_inputs(
    dir: &Path,
    key: &SecretKey,
    args: &Args,
    inputs: &[Input],
    source: Option<SourceRef>,
    publisher: Option<String>,
) -> Result<(), String> {
    let mut files = Vec::new();
    let mut first: Option<Package> = None;
    let mut checks_passed = true;
    for input in inputs {
        let p = Package::open_with(&input.path, Limits::every_tier())
            .map_err(|e| format!("{}: {e}", input.path.display()))?;
        let output = lightair_check::static_checks(&input.path, &input.url);
        checks_passed &= output.verdict() != lightair_check::Verdict::Fail;
        crate::print_findings(&output);
        let platform = platform_of(&input.path, &output);
        let size = fs::metadata(&input.path).map_err(|e| e.to_string())?.len();
        files.push(AttestedFile {
            platform,
            url: input.url.clone(),
            size,
            archive_sha256: p.archive_sha256.ok_or("add .airpkg files, not folders")?,
            content_sha256: p.content_sha256,
        });
        if let Some(f) = &first {
            let (a, b) = (&f.manifest, &p.manifest);
            if a.id != b.id || a.version != b.version || a.permissions != b.permissions || a.lightair != b.lightair {
                return Err(
                    "every file of a version must have the same id, version, permissions and Lightair range".into()
                );
            }
        } else {
            first = Some(p);
        }
    }
    if !checks_passed && !args.has("--force") {
        return Err("the package fails the checks; fix it, or add --force to list it anyway".into());
    }
    let p = first.ok_or("nothing to add")?;
    let m = &p.manifest;
    let publisher = publisher.unwrap_or_else(|| if m.author.is_empty() { "unknown".into() } else { m.author.clone() });
    let attestation = Attestation {
        format: FORMAT,
        id: m.id.to_string(),
        version: m.version.to_string(),
        publisher: publisher.clone(),
        tier: m.highest_tier().map_or(0, |t| t as u8),
        permissions: m.permissions.clone(),
        optional_permissions: m.optional_permissions.clone(),
        full_access: m.parsed_permissions().iter().any(|p| matches!(p, Permission::FullAccess(_))),
        lightair: m.lightair.to_string(),
        files,
        source: source.clone(),
        checks: if checks_passed {
            vec![CheckRef { name: "static".into(), version: lightair_check::CHECKS_VERSION.into() }]
        } else {
            Vec::new()
        },
        reviewed: None,
        published: format_rfc3339(now_unix()),
    };
    let env = Envelope::sign_json("attestation", &attestation, &[key]);

    let shard_name = shard_of(m.id.as_str());
    let shard_path = dir.join(format!("shards/{shard_name}.json"));
    let mut shard = Shard::parse(&fs::read(&shard_path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let listing = shard.packages.entry(m.id.to_string()).or_insert_with(Listing::default);
    if listing.versions.contains_key(&m.version.to_string()) {
        return Err(format!("{} {} is already in the catalog; versions never change", m.id, m.version));
    }
    if !listing.publisher.is_empty() && listing.publisher != publisher {
        return Err(format!("{} is listed by {}, not {publisher}", m.id, listing.publisher));
    }
    listing.name = m.name.clone();
    listing.summary = m.summary();
    listing.description = m.description.clone();
    listing.publisher = publisher;
    listing.kinds = m.counts().iter().map(|(k, _)| k.to_string()).collect();
    listing.tier = attestation.tier;
    listing.platforms = m.platforms.iter().map(|p| p.as_str().to_string()).collect();
    listing.repository = source.map(|s| s.repository).or(listing.repository.take());
    listing.versions.insert(m.version.to_string(), ListedVersion { attestation: env, yanked: false, gone: false });
    listing.latest = latest(listing);
    write(&shard_path, &shard_bytes(&shard))?;
    resign(dir, key, args, false)?;
    println!("Added {} {} to {}.", m.id, m.version, dir.display());
    Ok(())
}

/// The newest version that isn't yanked or gone.
fn latest(l: &Listing) -> String {
    l.versions
        .iter()
        .filter(|(_, v)| !v.yanked && !v.gone)
        .filter_map(|(k, _)| semver::Version::parse(k).ok())
        .max()
        .map(|v| v.to_string())
        .unwrap_or_default()
}

/// A file's target: `<anything>-<platform>.airpkg` (catalog formats §3a), else what the check
/// measured (a helper's program), else `any`.
fn platform_of(path: &Path, output: &lightair_check::Output) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for t in Target::ALL {
        if t != Target::Any && stem.ends_with(&format!("-{}", t.as_str())) {
            return t.as_str().to_string();
        }
    }
    output.files.first().map(|f| f.platform.clone()).unwrap_or_else(|| "any".into())
}

/// Downloads a release's `.airpkg` assets; returns them, the source and the repository's owner.
fn from_release(spec: &str, scratch: &Path) -> Result<(Vec<Input>, SourceRef, String), String> {
    let (repo, tag) = spec.split_once('@').ok_or("write the release as owner/repo@tag")?;
    let (owner, _) = repo.split_once('/').ok_or("write the release as owner/repo@tag")?;
    let http = Http::new();
    let token = github_token().ok();
    let token = token.as_deref();
    let info = http.get(&format!("https://api.github.com/repos/{repo}"), token)?;
    if !info.ok() {
        return Err(format!("{repo}: {}", info.text()));
    }
    if info.json()["private"].as_bool() == Some(true) {
        return Err(format!("{repo} is private: packages must be in public repositories"));
    }
    let rel = http.get(&format!("https://api.github.com/repos/{repo}/releases/tags/{tag}"), token)?;
    if !rel.ok() {
        return Err(format!("{repo}@{tag}: {}", rel.text()));
    }
    let rel = rel.json();
    if rel["immutable"].as_bool() != Some(true) {
        println!("note: {repo}@{tag} isn't an immutable release; its files could still be changed");
    }
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let mut inputs = Vec::new();
    for asset in rel["assets"].as_array().cloned().unwrap_or_default() {
        let name = asset["name"].as_str().unwrap_or_default();
        if !name.ends_with(".airpkg") {
            continue;
        }
        let url = asset["browser_download_url"].as_str().unwrap_or_default().to_string();
        let path = scratch.join(name);
        http.download(&url, &path, 16 << 30)?;
        inputs.push(Input { path, url });
    }
    if inputs.is_empty() {
        return Err(format!("{repo}@{tag} has no .airpkg files"));
    }
    let commit = tag_commit(&http, repo, tag, token)?;
    Ok((inputs, SourceRef { repository: format!("https://github.com/{repo}"), commit }, owner.to_string()))
}

/// The commit a tag points at (through an annotated tag).
pub fn tag_commit(http: &Http, repo: &str, tag: &str, token: Option<&str>) -> Result<String, String> {
    let r = http.get(&format!("https://api.github.com/repos/{repo}/git/ref/tags/{tag}"), token)?;
    let v = r.json();
    let mut sha = v["object"]["sha"].as_str().unwrap_or_default().to_string();
    if v["object"]["type"].as_str() == Some("tag") {
        let t = http.get(&format!("https://api.github.com/repos/{repo}/git/tags/{sha}"), token)?.json();
        sha = t["object"]["sha"].as_str().unwrap_or_default().to_string();
    }
    if sha.len() == 40 { Ok(sha) } else { Err(format!("couldn't find the commit of {repo}@{tag}")) }
}

fn revoke(args: &mut Args) -> Result<(), String> {
    let (dir, key) = dir_and_key(args)?;
    let reason = args.value("--reason").ok_or("say why with --reason \"…\"")?;
    let mut entry = RevokedEntry {
        sha256: None,
        id: args.value("--id"),
        versions: args.value("--versions"),
        publisher: args.value("--publisher"),
        reason,
        at: format_rfc3339(now_unix()),
    };
    if let Some(s) = args.value("--sha256") {
        entry.sha256 = Some(s.parse::<Sha256>()?);
    }
    if entry.sha256.is_none() && entry.id.is_none() && entry.publisher.is_none() {
        return Err("revoke by --sha256, --id (with --versions) or --publisher".into());
    }
    let path = dir.join("revoked.json");
    let mut revoked: Revoked = read_env(&path)?.ok_or("the catalog has no revoked.json")?;
    revoked.version += 1;
    revoked.entries.push(entry);
    write(&path, &Envelope::sign_json("revoked", &revoked, &[&key]).to_json())?;
    resign(&dir, &key, args, false)?;
    println!("Revoked; every app checking {} switches it off.", dir.display());
    Ok(())
}

fn yank(args: &mut Args) -> Result<(), String> {
    let (dir, key) = dir_and_key(args)?;
    let id = args.value("--id").ok_or("name the package with --id")?;
    let version = args.value("--version").ok_or("name the version with --version")?;
    let path = dir.join(format!("shards/{}.json", shard_of(&id)));
    let mut shard = Shard::parse(&fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let listing = shard.packages.get_mut(&id).ok_or_else(|| format!("{id} isn't in the catalog"))?;
    let v = listing.versions.get_mut(&version).ok_or_else(|| format!("{id} {version} isn't in the catalog"))?;
    v.yanked = !args.has("--undo");
    listing.latest = latest(listing);
    write(&path, &shard_bytes(&shard))?;
    resign(&dir, &key, args, false)?;
    println!("{id} {version} {}.", if args.has("--undo") { "is offered again" } else { "is yanked" });
    Ok(())
}
