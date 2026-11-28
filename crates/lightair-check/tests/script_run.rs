//! The catalog's run of a script plugin: every command and step used once against a fake
//! Lightair, and any use outside the declared permissions fails the package.

use std::path::Path;

use lightair_check::Verdict;

fn script_package(dir: &Path, permissions: &str, main: &str) {
    std::fs::create_dir_all(dir.join("script")).unwrap();
    std::fs::write(dir.join("script/main.js"), main).unwrap();
    std::fs::write(dir.join("LICENSE"), "MIT").unwrap();
    std::fs::write(
        dir.join("lightair-package.json"),
        format!(
            r#"{{"id":"io.github.nova.s","name":"S","version":"1.0.0","lightair":">=1.0 <2","licence":"MIT",
               "contents":[{{"kind":"extension","path":"script"}}],"permissions":[{permissions}]}}"#
        ),
    )
    .unwrap();
}

fn run_check(dir: &Path) -> lightair_check::Check {
    let out = lightair_check::run(dir, false, None);
    out.checks.into_iter().find(|c| c.name == "run").expect("a run check")
}

#[test]
fn a_script_using_only_what_it_declared_passes() {
    let t = tempfile::tempdir().unwrap();
    script_package(
        t.path(),
        r#""project.read","project.write""#,
        r#"lightair.command({ id: "next", title: "Next scene", run: () => lightair.call("scene.switch", { scene: "Main" }) });
           lightair.step({ id: "count", title: "Count", run: () => lightair.call("scene.list", {}) });"#,
    );
    let c = run_check(t.path());
    assert_ne!(c.verdict(), Verdict::Fail, "{c:?}");
}

#[test]
fn a_script_calling_a_method_it_didnt_declare_fails_naming_it() {
    let t = tempfile::tempdir().unwrap();
    script_package(
        t.path(),
        r#""project.read""#,
        r#"lightair.command({ id: "go", title: "Go live", run: () => lightair.call("output.start", {}) });"#,
    );
    let c = run_check(t.path());
    assert_eq!(c.verdict(), Verdict::Fail, "{c:?}");
    assert!(c.results.iter().any(|r| r.message.contains("output.start")), "the failure names the method: {c:?}");
}
