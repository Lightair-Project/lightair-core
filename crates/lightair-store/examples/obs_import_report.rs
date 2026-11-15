//! Dry run: prints what importing an OBS scene collection would do, without writing anything.
//! `cargo run -p lightair-store --example obs_import_report -- path/to/collection.json`

fn main() {
    let path = std::env::args().nth(1).expect("usage: obs_import_report <collection.json>");
    let json = std::fs::read_to_string(&path).expect("readable file");
    let workspace = lightair_core::Workspace::new("Dry run");
    match lightair_store::import_obs_collection(&json, &workspace) {
        Ok((_, report)) => {
            println!("{}\n", report.summary());
            for (title, items) in [
                ("Converted", &report.converted),
                ("Replaced by built-in features", &report.replaced),
                ("Kept as plugins", &report.kept_plugin),
                ("Needs attention", &report.attention),
            ] {
                println!("{title}:");
                for i in items {
                    println!("  - {i}");
                }
            }
        }
        Err(e) => eprintln!("{e}"),
    }
}
