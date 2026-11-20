//! `cargo xtask <command>`: lightair-core's chores.
//!
//! - `check`: what runs before a push (there's no hosted CI): clippy and every test.
//! - `fonts-catalog [--commit <sha>]`: regenerates the font catalogue from `google/fonts`
//!   (docs/fonts.md §4).
//! - `fonts-bundle [<folder>]`: downloads the bundled font set, checked against the catalogue
//!   (docs/fonts.md §3).

mod fonts;

use std::process::Command;

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    eprintln!("$ {cmd:?}");
    let status = cmd.status()?;
    anyhow::ensure!(status.success(), "{cmd:?} failed");
    Ok(())
}

fn check() -> anyhow::Result<()> {
    run(Command::new("cargo").args(["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]))?;
    run(Command::new("cargo").args(["test", "--workspace"]))?;
    eprintln!("lightair-core: all checks passed");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("check") => check(),
        Some("fonts-catalog") => {
            let commit = args.iter().position(|a| a == "--commit").and_then(|i| args.get(i + 1)).cloned();
            fonts::catalog(commit)
        }
        Some("fonts-bundle") => fonts::bundle(args.get(2).map(std::path::PathBuf::from)),
        _ => {
            eprintln!("usage: cargo xtask check | fonts-catalog [--commit <sha>] | fonts-bundle [<folder>]");
            std::process::exit(2);
        }
    }
}
