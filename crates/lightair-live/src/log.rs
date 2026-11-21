//! Chat history kept (docs/plans/live-integrations.md, 2.4): every event appended to a file per
//! day (JSON lines, `chat/2026-11-16.jsonl`), and the latest put back in the hub at start (so
//! the Chat and Activity panels show what came before a restart; nothing reacts to them again).
//! Test events aren't kept.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::event::ChatEvent;
use crate::hub::Hub;

/// The day's file (UTC).
fn file_for(dir: &Path, ts_ms: i64) -> PathBuf {
    let days = ts_ms.div_euclid(86_400_000);
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    dir.join(format!("{y:04}-{m:02}-{d:02}.jsonl"))
}

/// The latest `n` events kept (today's and yesterday's files).
pub fn recent(dir: &Path, n: usize) -> Vec<ChatEvent> {
    let now = crate::event::now_ms();
    let mut out: Vec<ChatEvent> = Vec::new();
    for day in [now - 86_400_000, now] {
        if let Ok(text) = std::fs::read_to_string(file_for(dir, day)) {
            out.extend(text.lines().filter_map(|l| serde_json::from_str(l).ok()));
        }
    }
    let start = out.len().saturating_sub(n);
    out.split_off(start)
}

/// Puts back the latest events, then keeps every new one.
pub fn start(hub: &Hub, dir: PathBuf) {
    hub.restore(recent(&dir, 300));
    let (wake, woken) = crossbeam_channel::bounded::<()>(1);
    hub.on_change(move || {
        let _ = wake.try_send(());
    });
    let hub = hub.clone();
    let mut last = hub.history().last().map_or(0, |e| e.id);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::thread::Builder::new().name("lightair-chat-log".into()).spawn(move || {
        while woken.recv().is_ok() {
            // a moment's batch per write
            std::thread::sleep(Duration::from_millis(500));
            let events = hub.since(last);
            let Some(newest) = events.last() else { continue };
            last = newest.id;
            let mut by_file: std::collections::BTreeMap<PathBuf, String> = Default::default();
            for e in events.iter().filter(|e| !e.test) {
                if let Ok(line) = serde_json::to_string(e) {
                    let buf = by_file.entry(file_for(&dir, e.ts_ms)).or_default();
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
            for (path, text) in by_file {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    let _ = f.write_all(text.as_bytes());
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_named_by_their_date() {
        let dir = Path::new("/x");
        assert_eq!(file_for(dir, 0), PathBuf::from("/x/1970-01-01.jsonl"));
        // 2026-11-16T13:00:00Z
        assert_eq!(file_for(dir, 1_794_834_000_000), PathBuf::from("/x/2026-11-16.jsonl"));
        // 2024-02-29
        assert_eq!(file_for(dir, 1_709_208_000_000), PathBuf::from("/x/2024-02-29.jsonl"));
    }

    #[test]
    fn kept_events_come_back_without_being_new() {
        let dir = std::env::temp_dir().join(format!("lightair-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Hub::new();
        start(&hub, dir.clone());
        hub.inject(
            crate::Channel { platform: "twitch".into(), channel: "x".into() },
            ChatEvent::new("twitch", "x", crate::Kind::Message),
        );
        // the writer batches for a moment: wait for the file, not a guessed time
        let start = std::time::Instant::now();
        while recent(&dir, 10).is_empty() {
            assert!(start.elapsed() < Duration::from_secs(5), "never written");
            std::thread::sleep(Duration::from_millis(10));
        }
        let again = Hub::new();
        again.restore(recent(&dir, 10));
        assert_eq!(again.history().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
