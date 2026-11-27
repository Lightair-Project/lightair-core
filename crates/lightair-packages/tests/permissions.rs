//! What a package may ask for, by what it holds, and which tiers an app installs.

use lightair_packages::catalog::Target;
use lightair_packages::{Manifest, Package, Permission, Source, Store, Tier};

fn manifest(contents: &str, permissions: &str) -> Manifest {
    Manifest::from_json(
        format!(
            r#"{{"id":"io.github.nova.p","name":"P","version":"1.0.0","lightair":">=1.0 <2",
               "contents":[{contents}],"permissions":[{permissions}]}}"#
        )
        .as_bytes(),
    )
    .expect("structurally valid manifest")
}

fn allowed(m: &Manifest) -> bool {
    m.check_supported_up_to(Tier::Native).is_empty()
}

#[test]
fn a_widget_package_may_declare_the_hosts_its_page_reaches() {
    let m = manifest(r#"{"kind":"widget","path":"w"}"#, r#""network:api.example.com""#);
    assert!(allowed(&m), "{:?}", m.check_supported_up_to(Tier::Native));
}

#[test]
fn a_template_package_may_declare_hosts_for_its_web_sources() {
    let m = manifest(r#"{"kind":"template","path":"t"}"#, r#""network:*.example.com""#);
    assert!(allowed(&m), "{:?}", m.check_supported_up_to(Tier::Native));
}

#[test]
fn a_media_only_package_may_not_ask_for_the_network() {
    let m = manifest(r#"{"kind":"media","path":"m"}"#, r#""network:example.com""#);
    assert!(!allowed(&m));
}

#[test]
fn a_widget_package_may_not_ask_for_the_screen() {
    let m = manifest(r#"{"kind":"widget","path":"w"}"#, r#""screen.capture""#);
    assert!(!allowed(&m));
}

#[test]
fn a_shader_package_may_ask_for_nothing() {
    let m = manifest(r#"{"kind":"shader-effect","path":"s"}"#, r#""network:example.com""#);
    assert!(!allowed(&m));
}

#[test]
fn full_access_is_only_for_helpers() {
    let widget = manifest(r#"{"kind":"widget","path":"w"}"#, r#""full-access""#);
    assert!(!allowed(&widget));
    let helper =
        manifest(r#"{"kind":"helper","path":"h","entry":{"macos-arm64":"h/bin"},"modes":["run"]}"#, r#""full-access""#);
    assert!(allowed(&helper), "{:?}", helper.check_supported_up_to(Tier::Native));
}

#[test]
fn full_access_for_one_platform_applies_only_there() {
    let m = manifest(r#"{"kind":"helper","path":"h","entry":{"windows-x64":"h/x.exe"}}"#, r#""full-access:windows""#);
    assert!(m.full_access_on(Target::WindowsX64));
    assert!(!m.full_access_on(Target::MacosArm64));
}

#[test]
fn an_unknown_permission_is_refused_when_the_manifest_is_read() {
    let r = Manifest::from_json(
        br#"{"id":"io.github.nova.p","name":"P","version":"1.0.0","lightair":">=1.0 <2",
            "contents":[{"kind":"widget","path":"w"}],"permissions":["keylogger"]}"#,
    );
    assert!(r.is_err());
}

#[test]
fn only_helpers_may_name_a_program() {
    let r = Manifest::from_json(
        br#"{"id":"io.github.nova.p","name":"P","version":"1.0.0","lightair":">=1.0 <2",
            "contents":[{"kind":"widget","path":"w","entry":{"macos-arm64":"w/x"}}]}"#,
    );
    assert!(r.is_err());
}

#[test]
fn a_helper_naming_an_unknown_target_is_refused() {
    let r = Manifest::from_json(
        br#"{"id":"io.github.nova.p","name":"P","version":"1.0.0","lightair":">=1.0 <2",
            "contents":[{"kind":"helper","path":"h","entry":{"amiga-68k":"h/x"}}]}"#,
    );
    assert!(r.is_err());
}

#[test]
fn a_wildcard_host_covers_subdomains_and_not_lookalikes() {
    let p = Permission::parse("network:*.example.com").unwrap();
    assert!(p.allows_host("api.example.com"));
    assert!(p.allows_host("example.com"));
    assert!(!p.allows_host("evilexample.com"));
    assert!(!p.allows_host("example.com.evil.net"));
}

#[test]
fn a_permission_written_back_reads_the_same() {
    for text in [
        "network:api.example.com",
        "network:localhost:8001",
        "network.listen:1455",
        "full-access:windows",
        "device.hid:0fd9",
    ] {
        let p = Permission::parse(text).unwrap();
        assert_eq!(Permission::parse(&p.to_string()).unwrap(), p, "{text}");
    }
}

fn helper_package(dir: &std::path::Path) -> Package {
    std::fs::create_dir_all(dir.join("h")).unwrap();
    std::fs::write(dir.join("h/bin"), b"#!not really a program").unwrap();
    std::fs::write(
        dir.join("lightair-package.json"),
        br#"{"id":"io.github.nova.helper","name":"H","version":"1.0.0","lightair":">=1.0 <2",
            "contents":[{"kind":"helper","path":"h","entry":{"macos-arm64":"h/bin","macos-x64":"h/bin","windows-x64":"h/bin","linux-x64":"h/bin"}}]}"#,
    )
    .unwrap();
    Package::open_with(dir, lightair_packages::Limits::every_tier()).unwrap()
}

#[test]
fn a_store_installs_a_helper_only_when_asked_to_take_native_code() {
    let t = tempfile::tempdir().unwrap();
    let p = helper_package(&t.path().join("pkg"));
    let store = Store::open(&t.path().join("data")).unwrap();
    let src = || Source::Folder { name: "pkg".into() };
    assert!(store.install(&p, src()).is_err(), "a helper installed without asking for native code");
    assert!(store.install_with(&p, src(), Tier::Native).is_ok());
}
