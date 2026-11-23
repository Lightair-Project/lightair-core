//! The `.lairava` file (docs/pngtuber-format.md): what's saved opens again as it was, files from
//! other writers and older versions open, and a broken file is an error, never a panic.

use std::collections::HashMap;
use std::io::{Cursor, Write};

use lightair_pngtuber::file::{self, Error, encode_png};
use lightair_pngtuber::{Anchor, Avatar, Blink, Expression, Layer, Mic, Motion, Picture, Role};

fn layer(id: &str, role: Role, frame: u8) -> Layer {
    Layer {
        id: id.into(),
        name: id.into(),
        image: format!("layers/{id}.png"),
        x: 10.0,
        y: 20.5,
        width: 30.0,
        height: 40.0,
        pivot: [25.0, 60.5],
        role,
        frame,
        motion: 1.0,
        opacity: 1.0,
    }
}

/// A small picture whose pixels differ, so a mix-up between layers shows.
fn picture(seed: u8) -> Picture {
    let (width, height) = (3, 2);
    let rgba = (0..width * height * 4).map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed)).collect();
    Picture { width, height, rgba }
}

/// A ZIP holding these entries, as another program might write it.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn png_of(p: &Picture) -> Vec<u8> {
    encode_png(p).unwrap()
}

#[test]
fn a_saved_avatar_opens_as_it_was() {
    let mut a = Avatar::new("Mochi", 1024, 768);
    let mut back = layer("bg", Role::Part, 0);
    back.motion = 0.0;
    back.opacity = 0.5;
    a.layers = vec![
        back,
        layer("eyes-open", Role::EyesOpen, 0),
        layer("eyes-shut", Role::EyesClosed, 0),
        layer("mouth-0", Role::Mouth, 0),
        layer("mouth-2", Role::Mouth, 2),
        layer("talk", Role::Talking, 0),
        layer("idle", Role::Quiet, 0),
        layer("blush", Role::Extra, 0),
    ];
    a.anchors = vec![
        Anchor { name: "Head".into(), x: 512.0, y: 260.0, layer: Some("bg".into()) },
        Anchor { name: "Feet".into(), x: 512.0, y: 768.0, layer: None },
    ];
    a.expressions = vec![
        Expression { name: "Shy".into(), show: vec!["blush".into()], hide: vec![], costume: false },
        Expression { name: "Hat".into(), show: vec![], hide: vec!["idle".into()], costume: true },
    ];
    a.mic = Mic { open_db: -30.0, close_db: -45.0, gate_db: -60.0, attack_ms: 12.0, release_ms: 300.0 };
    a.motion = Motion { bounce: 0.0, squash: 2.0, breathing: 0.5, hop: 1.5 };
    a.blink = Blink { every_s: 0.0, length_ms: 90.0, double: 1.0 };
    let pictures: Vec<Picture> = (0..a.layers.len()).map(|i| picture(i as u8 * 11)).collect();
    let pngs: HashMap<String, Vec<u8>> =
        a.layers.iter().zip(&pictures).map(|(l, p)| (l.image.clone(), png_of(p))).collect();

    let mut out = Cursor::new(Vec::new());
    file::write(&mut out, &a, &pngs).unwrap();
    out.set_position(0);
    let loaded = file::read(out).unwrap();

    assert_eq!(loaded.avatar, a);
    assert_eq!(loaded.pictures, pictures, "each layer gets its own picture back, in order");
}

#[test]
fn a_version_1_file_from_another_writer_opens_with_the_defaults() {
    // only what the format requires, no optional fields, and fields this reader doesn't know
    let json = br#"{
        "format": "lightair-pngtuber", "version": 1, "name": "Min", "width": 64, "height": 64,
        "layers": [ { "id": "body", "name": "Body", "image": "pics/body.png",
                      "x": 0, "y": 0, "width": 64, "height": 64, "pivot": [32, 64],
                      "glow": 0.3 } ],
        "sparkles": { "every_s": 2 }
    }"#;
    let body = picture(5);
    let bytes = zip_of(&[("avatar.json", json), ("pics/body.png", &png_of(&body))]);

    let loaded = file::read(Cursor::new(bytes)).expect("unknown fields are ignored");

    let a = &loaded.avatar;
    assert_eq!((a.name.as_str(), a.width, a.height), ("Min", 64, 64));
    let l = &a.layers[0];
    assert_eq!((l.role, l.frame, l.motion, l.opacity), (Role::Part, 0, 1.0, 1.0));
    assert!(a.anchors.is_empty() && a.expressions.is_empty());
    let defaults = Avatar::new("", 1, 1);
    assert_eq!((a.mic, a.motion, a.blink), (defaults.mic, defaults.motion, defaults.blink));
    assert_eq!(loaded.pictures, vec![body]);
}

#[test]
fn a_file_from_a_newer_version_is_refused() {
    let json =
        br#"{ "format": "lightair-pngtuber", "version": 2, "name": "Next", "width": 8, "height": 8, "layers": [] }"#;
    let r = file::read(Cursor::new(zip_of(&[("avatar.json", json)])));
    assert!(matches!(r, Err(Error::Version(2))), "{r:?}");
}

#[test]
fn another_format_in_a_zip_is_refused() {
    let json = br#"{ "format": "something-else", "version": 1, "name": "X", "width": 8, "height": 8, "layers": [] }"#;
    let r = file::read(Cursor::new(zip_of(&[("avatar.json", json)])));
    assert!(matches!(&r, Err(Error::Format(f)) if f == "something-else"), "{r:?}");
}

#[test]
fn broken_files_are_errors_not_panics() {
    let good_json = br#"{ "format": "lightair-pngtuber", "version": 1, "name": "B", "width": 8, "height": 8,
        "layers": [ { "id": "a", "name": "A", "image": "layers/a.png", "x": 0, "y": 0, "width": 8, "height": 8, "pivot": [4, 8] } ] }"#;
    let good_png = png_of(&picture(1));
    let whole = zip_of(&[("avatar.json", good_json), ("layers/a.png", &good_png)]);

    let read = |bytes: Vec<u8>| file::read(Cursor::new(bytes));
    let r = read(b"just some text, not a zip".to_vec());
    assert!(matches!(r, Err(Error::Zip(_))), "not a zip: {r:?}");
    let r = read(whole[..whole.len() / 2].to_vec());
    assert!(r.is_err(), "cut short: {r:?}");
    let r = read(zip_of(&[("layers/a.png", &good_png)]));
    assert!(matches!(&r, Err(Error::Missing(p)) if p == "avatar.json"), "no avatar.json: {r:?}");
    let r = read(zip_of(&[("avatar.json", b"{ not json")]));
    assert!(matches!(r, Err(Error::Json(_))), "bad json: {r:?}");
    let r = read(zip_of(&[("avatar.json", good_json)]));
    assert!(matches!(&r, Err(Error::Missing(p)) if p == "layers/a.png"), "picture missing: {r:?}");
    let r = read(zip_of(&[("avatar.json", good_json), ("layers/a.png", b"\x89PNG but not really")]));
    assert!(matches!(&r, Err(Error::Picture(p, _)) if p == "layers/a.png"), "picture broken: {r:?}");
}

#[test]
fn a_picture_wider_than_8192_is_refused() {
    let json = br#"{ "format": "lightair-pngtuber", "version": 1, "name": "W", "width": 8, "height": 8,
        "layers": [ { "id": "a", "name": "A", "image": "a.png", "x": 0, "y": 0, "width": 8, "height": 8, "pivot": [4, 8] } ] }"#;
    let wide = Picture { width: 8193, height: 1, rgba: vec![0; 8193 * 4] };
    let r = file::read(Cursor::new(zip_of(&[("avatar.json", json), ("a.png", &png_of(&wide))])));
    assert!(matches!(&r, Err(Error::Picture(p, _)) if p == "a.png"), "{r:?}");
}

#[test]
fn opaque_and_grey_pictures_open_as_rgba() {
    let encode = |color: png::ColorType, w: u32, data: &[u8]| {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, 1);
        e.set_color(color);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(data).unwrap();
        out
    };
    let rgb = file::decode_png(&encode(png::ColorType::Rgb, 2, &[1, 2, 3, 4, 5, 6])).unwrap();
    assert_eq!(rgb.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
    let grey = file::decode_png(&encode(png::ColorType::GrayscaleAlpha, 1, &[90, 40])).unwrap();
    assert_eq!(grey.rgba, [90, 90, 90, 40]);
}

#[test]
fn saving_without_a_layers_picture_is_an_error() {
    let mut a = Avatar::new("Gap", 8, 8);
    a.layers = vec![layer("a", Role::Part, 0), layer("b", Role::Part, 0)];
    let pngs = HashMap::from([("layers/a.png".to_string(), png_of(&picture(0)))]);
    let r = file::write(Cursor::new(Vec::new()), &a, &pngs);
    assert!(matches!(&r, Err(Error::Missing(p)) if p == "layers/b.png"), "{r:?}");
}
