//! `lightair-pack`: the authors' tool for Lightair packages and community catalogs.

mod args;
mod build;
mod catalog;
mod http;
mod keys;
mod publish;

use std::process::ExitCode;

use lightair_check::{Output, Verdict};

const USAGE: &str = "lightair-pack: build, check and publish Lightair packages, and run a community catalog

  lightair-pack build <folder> [--out <folder>]
      Packs a package folder into .airpkg files with normalised images (one per helper platform
      when it has several), then checks them.
  lightair-pack check <file.airpkg or folder> [--json <file>]
      The catalog's static checks.
  lightair-pack publish <folder or .airpkg …> --repo owner/name [--tag v1.2.0] [--notes <text>]
                        [--api <url>] [--no-register] [--force]
      An immutable GitHub Release in a public repository, then registered with Lightair's catalog
      (or --api's). Uses GITHUB_TOKEN, or the GitHub command line's sign-in.

  lightair-pack keys root [--out root-key.json]
      A root key, encrypted with a passphrase (LIGHTAIR_ROOT_PASSPHRASE, or asked).
  lightair-pack keys online --out <file>
      An online key (catalog or timestamp) for the catalog server's secrets.
  lightair-pack keys sign-root --version <n> [--name <name>] --root <key> … --catalog <key> …
                        --timestamp <key> … --sign-with <key file> … [--previous root/<n-1>.json]
                        [--root-threshold <n>] [--expires-days <days>] --out root/<n>.json

  lightair-pack catalog init <folder> --name <name> [--key <file>] [--days <n>]
  lightair-pack catalog add <folder> --key <file> (--release owner/repo@tag | --file <f> --url <u> …)
                        [--publisher <name>] [--repository <url> --commit <sha>] [--force]
  lightair-pack catalog sign <folder> --key <file> [--days <n>]
  lightair-pack catalog revoke <folder> --key <file> (--sha256 <s> | --id <id> [--versions <range>]
                        | --publisher <name>) --reason <text>
  lightair-pack catalog yank <folder> --key <file> --id <id> --version <v> [--undo]";

fn main() -> ExitCode {
    let mut args = args::Args::new(std::env::args().skip(1).collect());
    let result = match args.next_word().as_deref() {
        Some("build") => build::run(&mut args),
        Some("check") => check(&mut args),
        Some("publish") => publish::run(&mut args),
        Some("keys") => keys::run(&mut args),
        Some("catalog") => catalog::run(&mut args),
        _ => {
            println!("{USAGE}");
            return if args.has("--help") { ExitCode::SUCCESS } else { ExitCode::from(2) };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lightair-pack: {e}");
            ExitCode::from(1)
        }
    }
}

fn check(args: &mut args::Args) -> Result<(), String> {
    let path = args
        .positionals()
        .first()
        .cloned()
        .ok_or("usage: lightair-pack check <file.airpkg or folder> [--json <file>]")?;
    let output = lightair_check::static_checks(std::path::Path::new(&path), "");
    if let Some(out) = args.value("--json") {
        std::fs::write(&out, serde_json::to_vec_pretty(&output).unwrap_or_default()).map_err(|e| e.to_string())?;
    }
    if print_findings(&output) { Err("the package doesn't pass the checks".into()) } else { Ok(()) }
}

/// Prints every finding that isn't a pass; true when something failed.
pub(crate) fn print_findings(output: &Output) -> bool {
    for c in &output.checks {
        for r in &c.results {
            let tag = match r.verdict {
                Verdict::Pass => continue,
                Verdict::Note => "  note",
                Verdict::Fail => "  FAIL",
            };
            match &r.file {
                Some(f) => println!("{tag} {f}: {}", r.message),
                None => println!("{tag} {}", r.message),
            }
        }
    }
    let failed = output.verdict() == Verdict::Fail;
    println!("{}", if failed { "  Checks: failed." } else { "  Checks: passed." });
    failed
}
