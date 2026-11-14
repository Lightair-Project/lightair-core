//! A take's input log: written, read back, torn tails and pointer interpolation.

use std::time::Duration;

use lightair_store::input_log::{Entry, Log, Record, Writer};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn records_come_back_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.bin");
    let records = vec![
        Record { at: ms(0), entry: Entry::Display { x: 0.0, y: 0.0 } },
        Record { at: ms(0), entry: Entry::DisplaySize { w: 1512.0, h: 982.0 } },
        Record { at: ms(0), entry: Entry::Move { x: 100.0, y: 200.0 } },
        Record { at: ms(8), entry: Entry::Down { button: 0, x: 101.5, y: 200.0 } },
        Record { at: ms(90), entry: Entry::Up { button: 0, x: 101.5, y: 200.0 } },
        Record { at: ms(120), entry: Entry::KeyDown { code: 0x0F, modifiers: 8 } },
        Record { at: ms(130), entry: Entry::Shape(1) },
        Record { at: ms(200), entry: Entry::Scroll { dx: 0.0, dy: -3.0 } },
    ];
    let mut w = Writer::create(&path, true).unwrap();
    w.write(&records).unwrap();
    w.flush().unwrap();
    let log = Log::read(&path).unwrap();
    assert!(log.listening);
    assert_eq!(log.records, records);
}

#[test]
fn a_torn_tail_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.bin");
    let mut w = Writer::create(&path, false).unwrap();
    w.write(&[Record { at: ms(5), entry: Entry::Move { x: 1.0, y: 2.0 } }]).unwrap();
    w.flush().unwrap();
    drop(w);
    // a crash mid-record
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut f, &[1, 0, 0, 0, 9, 9]).unwrap();
    let log = Log::read(&path).unwrap();
    assert!(!log.listening);
    assert_eq!(log.records.len(), 1);
}

#[test]
fn the_pointer_is_interpolated_between_moves() {
    let log = Log {
        listening: false,
        records: vec![
            Record { at: ms(0), entry: Entry::Move { x: 0.0, y: 0.0 } },
            Record { at: ms(100), entry: Entry::Move { x: 100.0, y: 50.0 } },
        ],
    };
    assert_eq!(log.pointer_at(ms(50)), Some((50.0, 25.0)));
    assert_eq!(log.pointer_at(ms(500)), Some((100.0, 50.0)));
}
