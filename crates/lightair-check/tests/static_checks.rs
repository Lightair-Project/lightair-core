//! The static checks as the catalog runs them on an uploaded package: what fails a package, what
//! only adds a note, and what passes.

use std::path::Path;

use lightair_check::{Output, Verdict, code, normalise};

/// A widget package folder: `files` are (path inside the widget, contents).
fn widget(dir: &Path, permissions: &str, licence: &str, files: &[(&str, &[u8])]) {
    std::fs::create_dir_all(dir.join("widgets/w")).unwrap();
    for (name, bytes) in files {
        let p = dir.join("widgets/w").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    std::fs::write(dir.join("LICENSE"), "MIT").unwrap();
    std::fs::write(
        dir.join("lightair-package.json"),
        format!(
            r#"{{"id":"io.github.nova.w","name":"W","version":"1.0.0","lightair":">=1.0 <2","licence":"{licence}",
               "contents":[{{"kind":"widget","path":"widgets/w"}}],"permissions":[{permissions}]}}"#
        ),
    )
    .unwrap();
}

fn check<'a>(out: &'a Output, name: &str) -> &'a lightair_check::Check {
    out.checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no {name} check in {out:?}"))
}

const PAGE: &[u8] = b"<!doctype html><html><body><div id=a></div><script src=main.js></script></body></html>";

#[test]
fn a_widget_reaching_a_host_it_didnt_declare_fails() {
    let t = tempfile::tempdir().unwrap();
    widget(t.path(), "", "MIT", &[("index.html", PAGE), ("main.js", b"fetch('https://collect.example.net/x');")]);
    let out = lightair_check::static_checks(t.path(), "file:");
    let c = check(&out, "permissions");
    assert_eq!(c.verdict(), Verdict::Fail, "{c:?}");
    assert!(c.results.iter().any(|r| r.message.contains("collect.example.net")), "{c:?}");
}

#[test]
fn a_widget_reaching_only_its_declared_hosts_passes_that_check() {
    let t = tempfile::tempdir().unwrap();
    widget(
        t.path(),
        r#""network:collect.example.net""#,
        "MIT",
        &[("index.html", PAGE), ("main.js", b"fetch('https://collect.example.net/x');")],
    );
    let out = lightair_check::static_checks(t.path(), "file:");
    assert_ne!(check(&out, "permissions").verdict(), Verdict::Fail, "{:?}", check(&out, "permissions"));
}

#[test]
fn code_built_at_runtime_in_a_widget_is_a_note_not_a_failure() {
    let t = tempfile::tempdir().unwrap();
    widget(t.path(), "", "MIT", &[("index.html", PAGE), ("main.js", b"eval(location.hash.slice(1));")]);
    let out = lightair_check::static_checks(t.path(), "file:");
    assert_eq!(check(&out, "permissions").verdict(), Verdict::Note, "{:?}", check(&out, "permissions"));
}

#[test]
fn a_cloud_key_left_in_a_package_fails_it() {
    let t = tempfile::tempdir().unwrap();
    let js = b"const k = 'AKIAABCDEFGHIJKLMNOP'; document.getElementById('a').textContent = 'hi';";
    widget(t.path(), "", "MIT", &[("index.html", PAGE), ("main.js", js)]);
    let out = lightair_check::static_checks(t.path(), "file:");
    assert_eq!(check(&out, "secrets").verdict(), Verdict::Fail, "{:?}", check(&out, "secrets"));
}

#[test]
fn an_invalid_licence_expression_fails() {
    let t = tempfile::tempdir().unwrap();
    widget(t.path(), "", "MIT AND", &[("index.html", PAGE), ("main.js", b"1;")]);
    let out = lightair_check::static_checks(t.path(), "file:");
    assert_eq!(check(&out, "licence").verdict(), Verdict::Fail);
}

#[test]
fn a_valid_licence_expression_passes() {
    let t = tempfile::tempdir().unwrap();
    widget(t.path(), "", "MIT OR Apache-2.0", &[("index.html", PAGE), ("main.js", b"1;")]);
    let out = lightair_check::static_checks(t.path(), "file:");
    assert_ne!(check(&out, "licence").verdict(), Verdict::Fail, "{:?}", check(&out, "licence"));
}

/// A 2×2 PNG, with a text chunk when `with_text`.
fn png(with_text: bool) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, 2, 2);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        if with_text {
            e.add_text_chunk("Comment".into(), "taken at 51.5N 0.1W".into()).unwrap();
        }
        let mut w = e.write_header().unwrap();
        w.write_image_data(&[255u8; 16]).unwrap();
    }
    out
}

#[test]
fn an_image_carrying_metadata_isnt_normalised_and_fails_the_files_check() {
    let t = tempfile::tempdir().unwrap();
    let image = png(true);
    widget(t.path(), "", "MIT", &[("index.html", PAGE), ("main.js", b"1;"), ("logo.png", &image)]);
    let out = lightair_check::static_checks(t.path(), "file:");
    assert_eq!(check(&out, "files").verdict(), Verdict::Fail, "{:?}", check(&out, "files"));
}

#[test]
fn normalising_an_image_drops_its_metadata_and_then_it_passes() {
    let image = png(true);
    let clean = normalise::normalise(lightair_check::filetype::Family::Png, &image).unwrap();
    assert!(!clean.windows(4).any(|w| w == b"tEXt"), "the text chunk survived");
    assert!(normalise::is_normalised(lightair_check::filetype::Family::Png, &clean).unwrap());
    assert_eq!(normalise::decode_check(lightair_check::filetype::Family::Png, &clean).unwrap(), (2, 2));
}

#[test]
fn a_site_named_in_a_comment_or_a_regex_isnt_taken_for_a_load() {
    let mut r = code::Reach::default();
    code::scan_js(
        "// see https://docs.example.org\nconst re = /https:\\/\\/old\\.example\\.com/; fetch('https://api.example.com/v1');",
        &mut r,
    );
    assert!(r.hosts.contains("api.example.com"), "{:?}", r.hosts);
    assert!(!r.hosts.contains("docs.example.org") && !r.hosts.contains("old.example.com"), "{:?}", r.hosts);
}

#[test]
fn a_fetch_of_a_computed_address_is_left_for_the_run() {
    let mut r = code::Reach::default();
    code::scan_js("const base = 'https://' + host; fetch(base + '/x');", &mut r);
    assert!(!r.computed.is_empty(), "{r:?}");
}
