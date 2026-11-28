//! A catalog's chain of trust as an app sees it: a root it trusts, then a timestamp, a list, its
//! shards and an attestation, each only accepted when signed by the right keys, not older than
//! what was seen (a replay) and not expired (a freeze).

use std::collections::BTreeMap;

use lightair_packages::catalog::*;
use lightair_packages::{Package, Sha256};

/// 2027-01-15, a fixed "now".
const NOW: u64 = 1_800_000_000;
const DAY: u64 = 86_400;

struct Keys {
    root: SecretKey,
    catalog: SecretKey,
    timestamp: SecretKey,
}

fn keys(seed: u8) -> Keys {
    Keys {
        root: SecretKey::from_seed(&[seed; 32]),
        catalog: SecretKey::from_seed(&[seed + 1; 32]),
        timestamp: SecretKey::from_seed(&[seed + 2; 32]),
    }
}

fn root_payload(version: u64, k: &Keys, expires: u64) -> Root {
    let mut map = BTreeMap::new();
    for key in [&k.root, &k.catalog, &k.timestamp] {
        map.insert(key.public().id(), key.public().entry());
    }
    let role = |key: &SecretKey| Role { keys: vec![key.public().id()], threshold: 1 };
    Root {
        format: 1,
        version,
        expires: format_rfc3339(expires),
        name: "Test".into(),
        keys: map,
        roles: Roles { root: role(&k.root), catalog: role(&k.catalog), timestamp: role(&k.timestamp) },
    }
}

fn root_bytes(k: &Keys) -> Vec<u8> {
    Envelope::sign_json("root", &root_payload(1, k, NOW + 365 * DAY), &[&k.root]).to_json()
}

fn timestamp(k: &Keys, version: u64, snapshot: &[u8], snap_version: u64, expires: u64) -> Vec<u8> {
    let t = Timestamp {
        format: 1,
        version,
        expires: format_rfc3339(expires),
        snapshot: FileRef::of(snapshot, Some(snap_version)),
    };
    Envelope::sign_json("timestamp", &t, &[&k.timestamp]).to_json()
}

fn snapshot(k: &Keys, version: u64, shard: &[u8], revoked: &[u8], revoked_version: u64) -> Vec<u8> {
    let mut shards = BTreeMap::new();
    for d in "0123456789abcdef".chars() {
        shards.insert(d.to_string(), FileRef::of(shard, None));
    }
    let s = Snapshot {
        format: 1,
        version,
        expires: format_rfc3339(NOW + 7 * DAY),
        shards,
        revoked: FileRef::of(revoked, Some(revoked_version)),
    };
    Envelope::sign_json("snapshot", &s, &[&k.catalog]).to_json()
}

fn revoked(k: &Keys, version: u64) -> Vec<u8> {
    Envelope::sign_json("revoked", &Revoked { format: 1, version, entries: vec![] }, &[&k.catalog]).to_json()
}

const SHARD: &[u8] = br#"{"format":1,"packages":{}}"#;

/// A state that has accepted timestamp 1 and its list.
fn current(k: &Keys) -> CatalogState {
    let mut st = CatalogState::new("https://example.test/catalog/v1", &root_bytes(k), false, NOW).unwrap();
    let rev = revoked(k, 1);
    let snap = snapshot(k, 1, SHARD, &rev, 1);
    st.apply_timestamp(&timestamp(k, 1, &snap, 1, NOW + DAY), NOW).unwrap();
    st.apply_snapshot(&snap, NOW).unwrap();
    st
}

#[test]
fn a_root_signed_by_its_own_root_key_is_trusted() {
    let k = keys(1);
    assert!(CatalogState::new("https://x.test/", &root_bytes(&k), false, NOW).is_ok());
}

#[test]
fn a_root_signed_by_another_key_isnt_trusted() {
    let k = keys(1);
    let bytes = Envelope::sign_json("root", &root_payload(1, &k, NOW + DAY), &[&k.catalog]).to_json();
    let e = CatalogState::new("https://x.test/", &bytes, false, NOW).unwrap_err();
    assert!(matches!(e, TrustError::BadSignature { .. }), "{e:?}");
}

#[test]
fn an_expired_root_isnt_trusted() {
    let k = keys(1);
    let bytes = Envelope::sign_json("root", &root_payload(1, &k, NOW - 1), &[&k.root]).to_json();
    let e = CatalogState::new("https://x.test/", &bytes, false, NOW).unwrap_err();
    assert!(matches!(e, TrustError::Expired(_)), "{e:?}");
}

#[test]
fn a_new_root_needs_the_old_root_key_and_its_own() {
    let (old, new) = (keys(1), keys(10));
    let trusted = Root::trust_self_signed(&Envelope::from_json(&root_bytes(&old)).unwrap(), NOW).unwrap();
    let next = root_payload(2, &new, NOW + DAY);
    let only_new = Envelope::sign_json("root", &next, &[&new.root]);
    assert!(trusted.next(&only_new, NOW).is_err(), "a root signed only by the new key took over");
    let both = Envelope::sign_json("root", &next, &[&old.root, &new.root]);
    let rotated = trusted.next(&both, NOW).expect("rotation signed by both keys");
    assert_eq!(rotated.version, 2);
}

#[test]
fn a_root_skipping_a_version_isnt_accepted() {
    let k = keys(1);
    let trusted = Root::trust_self_signed(&Envelope::from_json(&root_bytes(&k)).unwrap(), NOW).unwrap();
    let skip = Envelope::sign_json("root", &root_payload(3, &k, NOW + DAY), &[&k.root]);
    assert!(trusted.next(&skip, NOW).is_err());
}

#[test]
fn a_threshold_of_two_needs_two_distinct_keys() {
    let (a, b) = (SecretKey::from_seed(&[7; 32]), SecretKey::from_seed(&[8; 32]));
    let mut map = BTreeMap::new();
    map.insert(a.public().id(), a.public().entry());
    map.insert(b.public().id(), b.public().entry());
    let role = Role { keys: vec![a.public().id(), b.public().id()], threshold: 2 };
    let twice_a = {
        let mut e = Envelope::sign("snapshot", b"{}", &[&a]);
        e.add_signature(&a);
        e
    };
    assert!(twice_a.verify("snapshot", &role, &map).is_err(), "one key counted twice");
    let both = Envelope::sign("snapshot", b"{}", &[&a, &b]);
    assert!(both.verify("snapshot", &role, &map).is_ok());
}

#[test]
fn a_signature_doesnt_carry_over_to_another_type() {
    let k = keys(1);
    let root = Root::trust_self_signed(&Envelope::from_json(&root_bytes(&k)).unwrap(), NOW).unwrap();
    // signed as a snapshot by the catalog key, presented as an attestation
    let mut env = Envelope::sign("snapshot", b"{}", &[&k.catalog]);
    env.kind = "attestation".into();
    assert!(env.verify("attestation", &root.roles.catalog, &root.keys).is_err());
}

#[test]
fn a_changed_payload_byte_breaks_the_signature() {
    let k = keys(1);
    let root = Root::trust_self_signed(&Envelope::from_json(&root_bytes(&k)).unwrap(), NOW).unwrap();
    let mut env = Envelope::sign("snapshot", br#"{"a":1}"#, &[&k.catalog]);
    env.payload = base64_of(br#"{"a":2}"#);
    assert!(env.verify("snapshot", &root.roles.catalog, &root.keys).is_err());
}

fn base64_of(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[test]
fn an_older_timestamp_is_refused_as_a_replay() {
    let k = keys(1);
    let mut st = current(&k);
    let rev = revoked(&k, 1);
    let snap = snapshot(&k, 1, SHARD, &rev, 1);
    st.apply_timestamp(&timestamp(&k, 5, &snap, 1, NOW + DAY), NOW).unwrap();
    let e = st.apply_timestamp(&timestamp(&k, 4, &snap, 1, NOW + DAY), NOW).unwrap_err();
    assert!(matches!(e, TrustError::Rollback(_)), "{e:?}");
}

#[test]
fn an_expired_timestamp_is_refused_as_a_freeze() {
    let k = keys(1);
    let mut st = current(&k);
    let rev = revoked(&k, 1);
    let snap = snapshot(&k, 1, SHARD, &rev, 1);
    let e = st.apply_timestamp(&timestamp(&k, 2, &snap, 1, NOW - 1), NOW).unwrap_err();
    assert!(matches!(e, TrustError::Expired(_)), "{e:?}");
}

#[test]
fn a_timestamp_signed_by_the_catalog_key_is_refused() {
    let k = keys(1);
    let mut st = CatalogState::new("https://x.test/", &root_bytes(&k), false, NOW).unwrap();
    let t =
        Timestamp { format: 1, version: 1, expires: format_rfc3339(NOW + DAY), snapshot: FileRef::of(b"x", Some(1)) };
    let wrong = Envelope::sign_json("timestamp", &t, &[&k.catalog]).to_json();
    assert!(st.apply_timestamp(&wrong, NOW).is_err());
}

#[test]
fn a_list_that_isnt_the_one_the_timestamp_names_is_refused() {
    let k = keys(1);
    let mut st = CatalogState::new("https://x.test/", &root_bytes(&k), false, NOW).unwrap();
    let rev = revoked(&k, 1);
    let snap = snapshot(&k, 1, SHARD, &rev, 1);
    let other = snapshot(&k, 1, br#"{"format":1,"packages":{"a.b":null}}"#, &rev, 1);
    st.apply_timestamp(&timestamp(&k, 1, &snap, 1, NOW + DAY), NOW).unwrap();
    assert!(st.apply_snapshot(&other, NOW).is_err());
}

#[test]
fn a_changed_shard_byte_is_refused() {
    let k = keys(1);
    let st = current(&k);
    assert!(st.check_shard("3", SHARD).is_ok());
    assert!(st.check_shard("3", br#"{"format":1,"packages":{ }}"#).is_err());
}

#[test]
fn an_older_revoked_list_is_refused() {
    let k = keys(1);
    let mut st = current(&k);
    st.apply_revoked(&revoked(&k, 1)).unwrap();
    // a new list naming revoked list 2, then a timestamp/list pair trying to go back to 1
    let rev2 = revoked(&k, 2);
    let snap2 = snapshot(&k, 2, SHARD, &rev2, 2);
    st.apply_timestamp(&timestamp(&k, 2, &snap2, 2, NOW + DAY), NOW).unwrap();
    let changes = st.apply_snapshot(&snap2, NOW).unwrap();
    assert!(changes.revoked && changes.shards.is_empty(), "{changes:?}");
    st.apply_revoked(&rev2).unwrap();
    let rev1 = revoked(&k, 1);
    let snap3 = snapshot(&k, 3, SHARD, &rev1, 1);
    st.apply_timestamp(&timestamp(&k, 3, &snap3, 3, NOW + DAY), NOW).unwrap();
    let e = st.apply_snapshot(&snap3, NOW).unwrap_err();
    assert!(matches!(e, TrustError::Rollback(_)), "{e:?}");
}

#[test]
fn a_saved_state_keeps_refusing_replays_after_a_restart() {
    let k = keys(1);
    let mut st = current(&k);
    let rev = revoked(&k, 1);
    let snap = snapshot(&k, 1, SHARD, &rev, 1);
    st.apply_timestamp(&timestamp(&k, 9, &snap, 1, NOW + DAY), NOW).unwrap();
    let saved = serde_json::to_vec(&st).unwrap();
    let mut back: CatalogState = serde_json::from_slice(&saved).unwrap();
    assert!(back.apply_timestamp(&timestamp(&k, 8, &snap, 1, NOW + DAY), NOW).is_err());
}

// ---------------------------------------------------------------- attestations and packages

fn package(dir: &std::path::Path, permissions: &str) -> Package {
    std::fs::create_dir_all(dir.join("media")).unwrap();
    std::fs::write(dir.join("media/a.txt"), b"hello").unwrap();
    std::fs::write(
        dir.join("lightair-package.json"),
        format!(
            r#"{{"id":"io.github.nova.neon","name":"Neon","version":"1.0.0","lightair":">=1.0 <2",
               "contents":[{{"kind":"media","path":"media"}}],"permissions":[{permissions}]}}"#
        ),
    )
    .unwrap();
    let out = dir.with_extension("airpkg");
    lightair_packages::export::write_archive(dir, &out).unwrap();
    Package::open(&out).unwrap()
}

fn attestation_for(p: &Package, version: &str, permissions: &[&str]) -> (Attestation, AttestedFile) {
    let file = AttestedFile {
        platform: "any".into(),
        url: "https://github.com/nova/neon/releases/download/v1/neon.airpkg".into(),
        size: 0,
        archive_sha256: p.archive_sha256.unwrap(),
        content_sha256: p.content_sha256,
    };
    let a = Attestation {
        format: 1,
        id: "io.github.nova.neon".into(),
        version: version.into(),
        publisher: "nova".into(),
        tier: 1,
        permissions: permissions.iter().map(|s| s.to_string()).collect(),
        full_access: false,
        lightair: ">=1.0 <2".into(),
        files: vec![file.clone()],
        source: None,
        checks: vec![],
        reviewed: None,
        published: format_rfc3339(NOW),
    };
    (a, file)
}

#[test]
fn the_downloaded_package_matches_its_attestation() {
    let t = tempfile::tempdir().unwrap();
    let p = package(&t.path().join("neon"), "");
    let (a, f) = attestation_for(&p, "1.0.0", &[]);
    assert_eq!(a.check_package(&f, &p), Ok(()));
}

#[test]
fn a_package_whose_bytes_differ_from_the_attestation_is_refused_with_reasons() {
    let t = tempfile::tempdir().unwrap();
    let p = package(&t.path().join("neon"), "");
    let (a, mut f) = attestation_for(&p, "1.0.0", &[]);
    f.archive_sha256 = Sha256::of(b"something else");
    f.content_sha256 = Sha256::of(b"other contents");
    let why = a.check_package(&f, &p).unwrap_err();
    assert_eq!(why.len(), 2, "{why:?}");
}

#[test]
fn a_package_asking_for_more_than_the_catalog_listed_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let p = package(&t.path().join("neon"), "");
    let (a, f) = attestation_for(&p, "1.0.0", &["network:example.com"]);
    let why = a.check_package(&f, &p).unwrap_err();
    assert!(why.iter().any(|w| w.contains("permissions")), "{why:?}");
}

#[test]
fn a_package_of_another_version_than_listed_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let p = package(&t.path().join("neon"), "");
    let (a, f) = attestation_for(&p, "1.0.1", &[]);
    let why = a.check_package(&f, &p).unwrap_err();
    assert!(why.iter().any(|w| w.contains("version")), "{why:?}");
}

#[test]
fn an_attestation_signed_by_the_timestamp_key_isnt_believed() {
    let k = keys(1);
    let st = CatalogState::new("https://x.test/", &root_bytes(&k), false, NOW).unwrap();
    let t = tempfile::tempdir().unwrap();
    let p = package(&t.path().join("neon"), "");
    let (a, _) = attestation_for(&p, "1.0.0", &[]);
    assert!(st.verify_attestation(&Envelope::sign_json("attestation", &a, &[&k.timestamp])).is_err());
    assert!(st.verify_attestation(&Envelope::sign_json("attestation", &a, &[&k.catalog])).is_ok());
}

#[test]
fn a_helper_gets_its_platforms_file_and_others_get_the_any_file() {
    let file = |platform: &str| AttestedFile {
        platform: platform.into(),
        url: String::new(),
        size: 1,
        archive_sha256: Sha256::of(platform.as_bytes()),
        content_sha256: Sha256::of(platform.as_bytes()),
    };
    let mut a = attestation_for_files(vec![file("macos-arm64"), file("windows-x64")]);
    assert_eq!(a.file_for(Target::WindowsX64).map(|f| f.platform.as_str()), Some("windows-x64"));
    assert!(a.file_for(Target::LinuxX64).is_none(), "a helper without a Linux build offered one");
    a.files.push(file("any"));
    assert_eq!(a.file_for(Target::LinuxX64).map(|f| f.platform.as_str()), Some("any"));
}

fn attestation_for_files(files: Vec<AttestedFile>) -> Attestation {
    Attestation {
        format: 1,
        id: "io.github.nova.helper".into(),
        version: "1.0.0".into(),
        publisher: "nova".into(),
        tier: 4,
        permissions: vec![],
        full_access: false,
        lightair: ">=1.0 <2".into(),
        files,
        source: None,
        checks: vec![],
        reviewed: None,
        published: format_rfc3339(NOW),
    }
}

// ---------------------------------------------------------------- revocations

fn entry(sha: Option<Sha256>, id: Option<&str>, versions: Option<&str>, publisher: Option<&str>) -> RevokedEntry {
    RevokedEntry {
        sha256: sha,
        id: id.map(String::from),
        versions: versions.map(String::from),
        publisher: publisher.map(String::from),
        reason: "Malware".into(),
        at: format_rfc3339(NOW),
    }
}

fn query<'a>(content: &'a Sha256, version: &'a semver::Version, from_this_catalog: bool) -> RevocationQuery<'a> {
    RevocationQuery {
        id: "io.github.nova.neon",
        version,
        publisher: Some("nova"),
        archive_sha256: None,
        content_sha256: content,
        from_this_catalog,
    }
}

#[test]
fn lightairs_list_revokes_a_file_by_checksum_whatever_its_source() {
    let sha = Sha256::of(b"evil");
    let v = semver::Version::new(1, 0, 0);
    let list = Revoked { format: 1, version: 1, entries: vec![entry(Some(sha), None, None, None)] };
    assert!(list.find(&query(&sha, &v, false), true).is_some(), "a sideloaded copy escaped Lightair's list");
}

#[test]
fn a_community_list_doesnt_reach_packages_from_elsewhere() {
    let sha = Sha256::of(b"evil");
    let v = semver::Version::new(1, 0, 0);
    let list = Revoked { format: 1, version: 1, entries: vec![entry(Some(sha), None, None, None)] };
    assert!(list.find(&query(&sha, &v, false), false).is_none());
    assert!(list.find(&query(&sha, &v, true), false).is_some());
}

#[test]
fn an_id_and_range_entry_revokes_only_the_versions_in_range() {
    let sha = Sha256::of(b"fine");
    let list = Revoked {
        format: 1,
        version: 1,
        entries: vec![entry(None, Some("io.github.nova.neon"), Some("<1.4.2"), None)],
    };
    let (old, new) = (semver::Version::new(1, 4, 1), semver::Version::new(1, 4, 2));
    assert!(list.find(&query(&sha, &old, true), true).is_some());
    assert!(list.find(&query(&sha, &new, true), true).is_none());
}

#[test]
fn id_and_publisher_entries_dont_reach_packages_from_another_source() {
    let sha = Sha256::of(b"fine");
    let v = semver::Version::new(1, 0, 0);
    let list = Revoked {
        format: 1,
        version: 1,
        entries: vec![entry(None, Some("io.github.nova.neon"), None, None), entry(None, None, None, Some("nova"))],
    };
    assert!(list.find(&query(&sha, &v, false), true).is_none());
    assert!(list.find(&query(&sha, &v, true), true).is_some());
}
