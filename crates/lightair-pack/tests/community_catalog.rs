//! A community catalog made with `lightair-pack catalog` is one the app trusts and reads: the
//! package added to it verifies down to the file, and a revocation reaches the app.

use std::path::Path;
use std::process::Command;

use lightair_packages::Package;
use lightair_packages::catalog::*;

fn pack(args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_lightair-pack")).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "lightair-pack {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn widget_package(dir: &Path) {
    std::fs::create_dir_all(dir.join("widgets/w")).unwrap();
    std::fs::write(
        dir.join("widgets/w/index.html"),
        "<!doctype html><html><body>hi<script src=main.js></script></body></html>",
    )
    .unwrap();
    std::fs::write(dir.join("widgets/w/main.js"), "document.body.dataset.on = '1';").unwrap();
    std::fs::write(dir.join("LICENSE"), "MIT License").unwrap();
    std::fs::write(
        dir.join("lightair-package.json"),
        r#"{"id":"io.github.nova.neon","name":"Neon","version":"1.0.0","lightair":">=1.0 <2","licence":"MIT",
            "contents":[{"kind":"widget","path":"widgets/w"}]}"#,
    )
    .unwrap();
}

fn read(dir: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn a_package_added_to_a_community_catalog_verifies_down_to_its_file() {
    let t = tempfile::tempdir().unwrap();
    let (cat, key, src, built) =
        (t.path().join("catalog"), t.path().join("key.json"), t.path().join("src"), t.path().join("built"));
    widget_package(&src);
    pack(&["build", src.to_str().unwrap(), "--out", built.to_str().unwrap()]);
    let airpkg = std::fs::read_dir(&built)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "airpkg"))
        .unwrap();
    pack(&["catalog", "init", cat.to_str().unwrap(), "--name", "Test Community", "--key", key.to_str().unwrap()]);
    pack(&[
        "catalog",
        "add",
        cat.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--file",
        airpkg.to_str().unwrap(),
        "--url",
        "https://example.test/neon.airpkg",
        "--publisher",
        "nova",
    ]);

    let now = now_unix();
    let mut st = CatalogState::new("https://example.test/", &read(&cat, "root/1.json"), false, now).unwrap();
    assert!(matches!(
        st.apply_timestamp(&read(&cat, "timestamp.json"), now).unwrap(),
        TimestampOutcome::NewSnapshot(_)
    ));
    st.apply_snapshot(&read(&cat, "snapshot.json"), now).unwrap();
    let shard_name = shard_of("io.github.nova.neon");
    let shard = st.check_shard(&shard_name, &read(&cat, &format!("shards/{shard_name}.json"))).unwrap();
    let listing = shard.packages.get("io.github.nova.neon").expect("the package is listed");
    let attestation = st.verify_attestation(&listing.versions["1.0.0"].attestation).unwrap();
    let file = attestation.file_for(Target::current()).expect("a file for this computer");
    let package = Package::open(&airpkg).unwrap();
    assert_eq!(attestation.check_package(file, &package), Ok(()));
}

#[test]
fn a_revocation_made_with_the_tool_reaches_the_app() {
    let t = tempfile::tempdir().unwrap();
    let (cat, key) = (t.path().join("catalog"), t.path().join("key.json"));
    pack(&["catalog", "init", cat.to_str().unwrap(), "--name", "Test Community", "--key", key.to_str().unwrap()]);
    let now = now_unix();
    let mut st = CatalogState::new("https://example.test/", &read(&cat, "root/1.json"), false, now).unwrap();
    st.apply_timestamp(&read(&cat, "timestamp.json"), now).unwrap();
    st.apply_snapshot(&read(&cat, "snapshot.json"), now).unwrap();
    st.apply_revoked(&read(&cat, "revoked.json")).unwrap();

    let evil = lightair_packages::Sha256::of(b"evil package");
    pack(&[
        "catalog",
        "revoke",
        cat.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
        "--sha256",
        &evil.to_string(),
        "--reason",
        "Steals keys.",
    ]);
    st.apply_timestamp(&read(&cat, "timestamp.json"), now).unwrap();
    let changes = st.apply_snapshot(&read(&cat, "snapshot.json"), now).unwrap();
    assert!(changes.revoked, "the new list didn't say the revoked list changed");
    let list = st.apply_revoked(&read(&cat, "revoked.json")).unwrap();
    let v = semver::Version::new(1, 0, 0);
    let q = RevocationQuery {
        id: "x.y",
        version: &v,
        publisher: None,
        archive_sha256: None,
        content_sha256: &evil,
        from_this_catalog: true,
    };
    assert_eq!(list.find(&q, false).map(|e| e.reason.as_str()), Some("Steals keys."));
}
