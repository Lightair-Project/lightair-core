//! Working on lightair-core and the apps at once (README, "Working on lightair-core and an app
//! together"). The apps sit beside this repo in one folder:
//!
//! - `cargo xtask dev on`: every app builds against this checkout instead of its pinned commit
//!   (each app's `cargo xtask core-link`), and gets a pre-commit guard that stops a linked
//!   Cargo.lock being committed.
//! - `cargo xtask dev off`: back to the pins (`core-unlink`).
//! - `cargo xtask dev status`: who's linked, pinned where, with what uncommitted.
//! - `cargo xtask ship [--push] [--only desktop,live,editor] [--dry-run]`: when a change is done.
//!   Checks and pushes lightair-core, then for each linked app (or those named): unlinks, pins it
//!   to lightair-core's HEAD, runs its check and commits the pin; with `--push`, pushes it too.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};

/// The apps, by short name and folder beside this repo.
const APPS: &[(&str, &str)] = &[("desktop", "lightair"), ("live", "lightair-live"), ("editor", "lightair-editor")];
const CORE_URL: &str = "github.com/MoonGoblinDev/lightair-core";
const HOOK_MARK: &str = "# lightair-core link guard";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

struct App {
    name: &'static str,
    dir: PathBuf,
}

fn apps() -> Vec<App> {
    APPS.iter()
        .map(|(name, folder)| App { name, dir: root().join("..").join(folder) })
        .filter(|a| a.dir.join("Cargo.toml").exists())
        .collect()
}

/// `cargo xtask <args>` in another repo, without this run's toolchain and package variables.
fn xtask(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("cargo");
    c.current_dir(dir).arg("xtask").args(args);
    for (k, _) in std::env::vars() {
        if k == "RUSTUP_TOOLCHAIN" || k == "CARGO" || k.starts_with("CARGO_PKG") || k == "CARGO_MANIFEST_DIR" || k == "CARGO_MANIFEST_PATH" {
            c.env_remove(k);
        }
    }
    c
}

fn run(name: &str, cmd: &mut Command) -> anyhow::Result<()> {
    let line = format!("{} {}", cmd.get_program().to_string_lossy(), cmd.get_args().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
    eprintln!("[{name}] $ {line}");
    let status = cmd.status().with_context(|| format!("[{name}] {line} didn't start"))?;
    ensure!(status.success(), "[{name}] {line} failed");
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    ensure!(out.status.success(), "git {} in {}: {}", args.join(" "), dir.display(), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn linked(app: &App) -> bool {
    app.dir.join(".cargo/core-link.toml").exists()
}

/// The lightair-core commit an app's Cargo.toml pins.
fn pinned(app: &App) -> Option<String> {
    let text = std::fs::read_to_string(app.dir.join("Cargo.toml")).ok()?;
    let line = text.lines().find(|l| l.contains(CORE_URL) && l.contains("rev = \""))?;
    let start = line.find("rev = \"")? + 7;
    Some(line[start..].split('"').next()?.to_string())
}

/// Uncommitted files in an app, leaving out Cargo.lock while it's linked (linking rewrites it).
fn uncommitted(app: &App) -> anyhow::Result<Vec<String>> {
    let lock_is_link = linked(app);
    Ok(git(&app.dir, &["status", "--porcelain"])?
        .lines()
        .map(|l| l[3..].to_string())
        .filter(|f| !(lock_is_link && f == "Cargo.lock"))
        .collect())
}

fn install_hook(app: &App) -> anyhow::Result<()> {
    let hooks = app.dir.join(git(&app.dir, &["rev-parse", "--git-path", "hooks"])?);
    let hook = hooks.join("pre-commit");
    let guard = format!(
        "{HOOK_MARK} (from `cargo xtask dev on` in lightair-core): while linked, Cargo.lock points at a\n\
         # local checkout; `cargo xtask ship` in lightair-core pins and commits it.\n\
         if [ -f .cargo/core-link.toml ] && git diff --cached --name-only | grep -qx Cargo.lock; then\n  \
           echo \"Cargo.lock is staged while lightair-core is linked: unstage it (git restore --staged Cargo.lock);\" >&2\n  \
           echo \"\\`cargo xtask ship\\` in lightair-core pins and commits it.\" >&2\n  \
           exit 1\n\
         fi\n"
    );
    match std::fs::read_to_string(&hook) {
        Ok(text) if text.contains(HOOK_MARK) => return Ok(()),
        Ok(text) => {
            // someone else's hook: add the guard after its first line
            let (first, rest) = text.split_once('\n').unwrap_or((&text, ""));
            std::fs::write(&hook, format!("{first}\n{guard}{rest}"))?;
        }
        Err(_) => {
            std::fs::create_dir_all(&hooks)?;
            std::fs::write(&hook, format!("#!/bin/sh\n{guard}"))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    eprintln!("[{}] pre-commit guard installed", app.name);
    Ok(())
}

pub fn dev(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("on") => {
            for app in apps() {
                run(app.name, &mut xtask(&app.dir, &["core-link"]))?;
                install_hook(&app)?;
            }
            eprintln!("\nevery app builds against {} now; `cargo xtask ship` when the change is done", root().display());
            Ok(())
        }
        Some("off") => {
            for app in apps() {
                run(app.name, &mut xtask(&app.dir, &["core-unlink"]))?;
            }
            Ok(())
        }
        Some("status") | None => status(),
        Some(other) => bail!("cargo xtask dev on | off | status (not {other})"),
    }
}

fn status() -> anyhow::Result<()> {
    let head = git(&root(), &["rev-parse", "HEAD"])?;
    let pushed = git(&root(), &["merge-base", "--is-ancestor", "HEAD", "origin/main"]).is_ok();
    let dirty = git(&root(), &["status", "--porcelain"])?.lines().count();
    println!(
        "lightair-core  HEAD {}{}{}",
        &head[..12],
        if pushed { ", pushed" } else { ", not pushed" },
        if dirty > 0 { format!(", {dirty} uncommitted") } else { String::new() }
    );
    for app in apps() {
        let pin = pinned(&app).unwrap_or_default();
        let at = if pin == head { "at HEAD".to_string() } else { format!("pinned {}", &pin[..pin.len().min(12)]) };
        let link = if linked(&app) { "linked" } else { "pinned build" };
        let changes = uncommitted(&app)?.len();
        println!("{:<14} {link}, {at}{}", app.name, if changes > 0 { format!(", {changes} uncommitted") } else { String::new() });
    }
    Ok(())
}

pub fn ship(args: &[String]) -> anyhow::Result<()> {
    let push = args.iter().any(|a| a == "--push");
    let dry = args.iter().any(|a| a == "--dry-run");
    let only: Option<Vec<String>> = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1))
        .map(|list| list.split(',').map(|s| s.trim().to_string()).collect());
    let core = root();

    let dirty = git(&core, &["status", "--porcelain"])?;
    ensure!(dirty.is_empty(), "lightair-core has uncommitted changes; commit them first:\n{dirty}");
    let head = git(&core, &["rev-parse", "HEAD"])?;
    let short = &head[..12];

    let all = apps();
    let chosen: Vec<&App> = match &only {
        Some(names) => {
            for n in names {
                ensure!(all.iter().any(|a| a.name == n), "no app called {n} (desktop, live, editor)");
            }
            all.iter().filter(|a| names.iter().any(|n| n == a.name)).collect()
        }
        None => {
            let linked: Vec<&App> = all.iter().filter(|a| linked(a)).collect();
            ensure!(!linked.is_empty(), "no app is linked; name them: `cargo xtask ship --only desktop,live,editor`");
            linked
        }
    };
    for app in &chosen {
        let files = uncommitted(app)?;
        ensure!(files.is_empty(), "[{}] has uncommitted changes; commit them first: {}", app.name, files.join(", "));
    }

    eprintln!("shipping lightair-core {short} to: {}", chosen.iter().map(|a| a.name).collect::<Vec<_>>().join(", "));
    if dry {
        eprintln!("(dry run: nothing changed)");
        return Ok(());
    }

    run("core", &mut xtask(&core, &["check"]))?;
    if git(&core, &["merge-base", "--is-ancestor", "HEAD", "origin/main"]).is_err() {
        run("core", Command::new("git").arg("-C").arg(&core).args(["push", "origin", "main"]))?;
    } else {
        eprintln!("[core] {short} is already on GitHub");
    }

    for app in chosen {
        run(app.name, &mut xtask(&app.dir, &["core-unlink"]))?;
        if pinned(app).as_deref() == Some(head.as_str()) {
            eprintln!("[{}] already pinned to {short}", app.name);
        } else {
            run(app.name, &mut xtask(&app.dir, &["core-pin", &head]))?;
            // the desktop's pin doesn't run its check; the apps' do
            if app.name == "desktop" {
                run(app.name, &mut xtask(&app.dir, &["check"]))?;
            }
            run(
                app.name,
                Command::new("git").arg("-C").arg(&app.dir).args(["commit", "-am", &format!("Pin lightair-core {short}")]),
            )?;
        }
        if push {
            run(app.name, Command::new("git").arg("-C").arg(&app.dir).args(["push", "origin", "main"]))?;
        }
    }
    eprintln!(
        "\nshipped: lightair-core {short} is pushed and pinned{}",
        if push { "; the apps are pushed too" } else { "; push the apps when ready (or rerun with --push)" }
    );
    Ok(())
}
