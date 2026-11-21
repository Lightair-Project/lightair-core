//! A survey over real exports, not a test: with LIGHTAIR_SB_DIRS (folders of export strings, one per .txt/.sb, separated by `:`):
//! every one imported, and what didn't come over, counted.
//! `LIGHTAIR_SB_DIRS=… cargo run -p lightair-automation --example streamerbot_survey`
fn main() {
    let Ok(dirs) = std::env::var("LIGHTAIR_SB_DIRS") else { return };
    let (mut files, mut ok, mut actions, mut steps) = (0, 0, 0, 0);
    let mut notes: std::collections::BTreeMap<String, usize> = Default::default();
    let mut failed = Vec::new();
    for dir in dirs.split(':') {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "txt" || e == "sb") {
                continue;
            }
            files += 1;
            match lightair_automation::streamerbot::import(&std::fs::read_to_string(&path).unwrap_or_default()) {
                Ok(r) => {
                    ok += 1;
                    actions += r.automations.len();
                    steps += r.automations.iter().map(|a| a.steps.len()).sum::<usize>();
                    for n in r.notes {
                        let kind = n.split_once(": ").map_or(n.clone(), |(_, k)| k.to_string());
                        *notes.entry(kind).or_default() += 1;
                    }
                }
                Err(e) => failed.push(format!("{}: {e}", path.display())),
            }
        }
    }
    eprintln!("{ok}/{files} imported: {actions} automations, {steps} top-level steps");
    let mut top: Vec<_> = notes.into_iter().collect();
    top.sort_by_key(|t| std::cmp::Reverse(t.1));
    for (n, c) in top.iter().take(15) {
        eprintln!("  {c:4} × {n}");
    }
    for f in failed.iter().take(5) {
        eprintln!("FAILED {f}");
    }
}
