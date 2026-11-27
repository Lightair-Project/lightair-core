//! `lightair-check`: the checker's command line (catalog formats §10). Each command writes its
//! JSON to `--json <file>` and exits 0 even when a check fails; a non-zero exit means the
//! checker itself couldn't run.

use std::path::PathBuf;
use std::process::ExitCode;

use lightair_check::{Output, Verdict};

const USAGE: &str = "usage:
  lightair-check static <file.airpkg> --json <out> [--url <download address>]
  lightair-check run <file.airpkg> --json <out> [--previews <dir>] [--deep]
  lightair-check helper-run <file.airpkg> --platform <platform> --json <out>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let has = |name: &str| args.iter().any(|a| a == name);
    let Some(file) = args.get(1).filter(|a| !a.starts_with("--")).map(PathBuf::from) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let output = match cmd.as_str() {
        "static" => lightair_check::static_checks(&file, &flag("--url").unwrap_or_default()),
        "run" => lightair_check::run(&file, has("--deep")),
        "helper-run" => match flag("--platform") {
            Some(p) => lightair_check::helper_run(&file, &p),
            None => {
                eprintln!("helper-run needs --platform\n{USAGE}");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    summarise(&output);
    match flag("--json") {
        Some(out) => match std::fs::write(&out, serde_json::to_vec_pretty(&output).unwrap_or_default()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("couldn't write {out}: {e}");
                ExitCode::from(1)
            }
        },
        None => {
            println!("{}", serde_json::to_string_pretty(&output).unwrap_or_default());
            ExitCode::SUCCESS
        }
    }
}

/// One line per finding that isn't a pass, on stderr.
fn summarise(output: &Output) {
    for c in &output.checks {
        for r in &c.results {
            let tag = match r.verdict {
                Verdict::Pass => continue,
                Verdict::Note => "note",
                Verdict::Fail => "FAIL",
            };
            match &r.file {
                Some(f) => eprintln!("{tag} [{}] {f}: {}", c.name, r.message),
                None => eprintln!("{tag} [{}] {}", c.name, r.message),
            }
        }
    }
    let v = output.verdict();
    eprintln!(
        "{}",
        match v {
            Verdict::Pass => "passed",
            Verdict::Note => "passed, with notes",
            Verdict::Fail => "failed",
        }
    );
}
