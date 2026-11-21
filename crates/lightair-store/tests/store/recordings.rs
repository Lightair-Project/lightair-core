//! The recordings index (docs/plans/screen-studio.md, "Library").

use std::fs;
use std::path::Path;

use lightair_store::recordings::{File, Index, Item, Kind, move_item, mp4_duration_ms};

fn item(path: &Path) -> Item {
    Item::new(Kind::Recording, "Show", vec![File::new(path.to_path_buf())], 1_700_000_000_000)
}

#[test]
fn entries_added_to_a_workspace_read_back_newest_first() {
    let workspace = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let mut first = item(&media.path().join("a.mp4"));
    first.markers.push(lightair_store::recordings::Marker { at_ms: 1500, label: "Intro".into() });
    Index::add_to(workspace.path(), first.clone()).unwrap();
    let clip = Item::new(Kind::Clip, "Clip", vec![File::new(media.path().join("c.mp4"))], 1_800_000_000_000);
    Index::add_to(workspace.path(), clip.clone()).unwrap();
    let index = Index::load(workspace.path());
    assert_eq!(index.get(first.id), Some(&first), "markers and all");
    let order: Vec<_> = index.newest_first().iter().map(|i| i.id).collect();
    assert_eq!(order, [clip.id, first.id]);
}

#[test]
fn a_search_finds_an_item_by_its_markers() {
    let media = tempfile::tempdir().unwrap();
    let mut it = item(&media.path().join("a.mp4"));
    it.markers.push(lightair_store::recordings::Marker { at_ms: 1500, label: "Intro".into() });
    assert!(it.matches("intro"));
    assert!(!it.matches("outro"));
}

#[test]
fn a_file_thats_not_on_disk_is_missing() {
    let media = tempfile::tempdir().unwrap();
    let here = media.path().join("a.mp4");
    fs::write(&here, b"x").unwrap();
    assert!(item(&here).missing().is_empty());
    assert_eq!(item(&media.path().join("never written.mp4")).missing().len(), 1);
}

#[test]
fn a_moved_file_is_found_by_locate() {
    let workspace = tempfile::tempdir().unwrap();
    let media = tempfile::tempdir().unwrap();
    let a = media.path().join("Lightair 2026-11-14 10-00-00.mp4");
    fs::write(&a, b"x").unwrap();
    let it = item(&a);
    Index::add_to(workspace.path(), it.clone()).unwrap();
    // the user moves it into a subfolder
    let moved = media.path().join("Archive").join("November");
    fs::create_dir_all(&moved).unwrap();
    fs::rename(&a, moved.join(a.file_name().unwrap())).unwrap();
    let mut index = Index::load(workspace.path());
    assert_eq!(index.get(it.id).unwrap().missing().len(), 1);
    assert_eq!(index.locate_in(media.path()), 1);
    assert!(index.get(it.id).unwrap().missing().is_empty());
    assert_eq!(index.get(it.id).unwrap().files[0].path, moved.join(a.file_name().unwrap()));
}

#[test]
fn trash_never_deletes_outright() {
    let media = tempfile::tempdir().unwrap();
    let a = media.path().join("a.mp4");
    fs::write(&a, b"x").unwrap();
    let mut index = Index::default();
    let it = item(&a);
    index.items.push(it.clone());
    // the Trash refuses: the file and the entry stay
    let refused = index.trash(it.id, |_| Err(std::io::Error::other("no Trash here")));
    assert!(refused.is_err());
    assert!(a.exists());
    assert!(index.get(it.id).is_some());
    // the Trash takes it: the file is there, not gone
    let bin = media.path().join("Trash");
    fs::create_dir_all(&bin).unwrap();
    index.trash(it.id, |p| fs::rename(p, bin.join(p.file_name().unwrap()))).unwrap();
    assert!(!a.exists() && bin.join("a.mp4").exists());
    assert!(index.get(it.id).is_none());
}

#[test]
fn duplicates_copy_their_files() {
    let media = tempfile::tempdir().unwrap();
    let a = media.path().join("a.mp4");
    fs::write(&a, b"take").unwrap();
    let mut index = Index::default();
    let it = item(&a);
    index.items.push(it.clone());
    let copy = index.duplicate(it.id).unwrap();
    assert_eq!(copy.title, "Show copy");
    assert_eq!(copy.files[0].path, media.path().join("a copy.mp4"));
    assert_eq!(fs::read(&copy.files[0].path).unwrap(), b"take");
    assert_eq!(index.duplicate(it.id).unwrap().files[0].path, media.path().join("a copy 2.mp4"));
}

#[test]
fn items_move_between_workspaces() {
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let it = item(Path::new("/nowhere/a.mp4"));
    Index::add_to(one.path(), it.clone()).unwrap();
    assert!(move_item(one.path(), two.path(), it.id).unwrap());
    assert!(Index::load(one.path()).items.is_empty());
    assert_eq!(Index::load(two.path()).items[0].id, it.id);
}

/// A minimal MP4: ftyp, then moov holding an mvhd of 90 000 ticks at 1000 per second.
fn tiny_mp4(path: &Path) {
    let mut mvhd = vec![0u8; 8 + 100];
    mvhd[..4].copy_from_slice(&(108u32).to_be_bytes());
    mvhd[4..8].copy_from_slice(b"mvhd");
    // version 0: flags, created, modified, timescale, duration
    mvhd[8 + 12..8 + 16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[8 + 16..8 + 20].copy_from_slice(&90_000u32.to_be_bytes());
    let mut moov = Vec::new();
    moov.extend_from_slice(&((8 + mvhd.len()) as u32).to_be_bytes());
    moov.extend_from_slice(b"moov");
    moov.extend_from_slice(&mvhd);
    let mut file = Vec::new();
    file.extend_from_slice(&16u32.to_be_bytes());
    file.extend_from_slice(b"ftypisom\0\0\0\0");
    file.extend_from_slice(&moov);
    fs::write(path, file).unwrap();
}

#[test]
fn existing_recordings_come_in_with_their_length() {
    let media = tempfile::tempdir().unwrap();
    tiny_mp4(&media.path().join("Old show.mp4"));
    fs::write(media.path().join("notes.txt"), b"not media").unwrap();
    assert_eq!(mp4_duration_ms(&media.path().join("Old show.mp4")), Some(90_000));
    let mut index = Index::default();
    let found = index.existing_in(media.path());
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].title.as_str(), found[0].duration_ms), ("Old show", Some(90_000)));
    index.items.extend(found);
    assert!(index.existing_in(media.path()).is_empty(), "already in the index");
}

#[test]
fn a_take_goes_out_as_a_package_and_comes_back_in() {
    let media = tempfile::tempdir().unwrap();
    let studio_take = media.path().join("Take 1.lightair-take");
    fs::create_dir_all(&studio_take).unwrap();
    fs::write(studio_take.join("screen.mp4"), b"v").unwrap();
    fs::write(studio_take.join("Audio - Mic.wav"), b"a").unwrap();
    fs::write(studio_take.join("input.bin"), b"i").unwrap();
    let json = serde_json::json!({"started_ms": 5, "duration_ms": 4000, "video": [{"name": "Screen", "file": "screen.mp4"}], "audio": ["Audio - Mic.wav"]});
    fs::write(studio_take.join("take.json"), serde_json::to_vec(&json).unwrap()).unwrap();
    let item = lightair_store::recordings::studio_take_item(&studio_take).unwrap();
    assert_eq!(item.kind, Kind::StudioTake);
    assert_eq!(item.files.len(), 4);
    assert_eq!(item.duration_ms, Some(4000));
    let edit = media.path().join("edit-source.json");
    fs::write(&edit, b"{}").unwrap();
    let out = tempfile::tempdir().unwrap();
    let package = lightair_store::recordings::export_package(&item, Some(&edit), out.path()).unwrap();
    assert!(package.join("edit.json").exists() && package.join("screen.mp4").exists());
    // brought in elsewhere: one item, the studio_take
    let found = Index::default().existing_in(out.path());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, Kind::StudioTake);
    assert!(found[0].files.iter().any(|f| f.path.ends_with("edit.json")));
}
