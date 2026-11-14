//! Result time ↔ source time, through the edit's clips (cuts) and their speeds.

use crate::edit::{Clip, Edit};

/// The result's length.
pub fn duration_ms(edit: &Edit) -> u64 {
    edit.clips.iter().map(clip_len).sum()
}

fn clip_len(c: &Clip) -> u64 {
    ((c.end_ms.saturating_sub(c.start_ms)) as f64 / c.speed.max(0.05) as f64).round() as u64
}

/// The source time shown at result time `t_ms` (`None` past the end).
pub fn source_at(edit: &Edit, t_ms: u64) -> Option<u64> {
    let mut at = 0u64;
    for c in &edit.clips {
        let len = clip_len(c);
        if t_ms < at + len {
            let into = (t_ms - at) as f64 * c.speed.max(0.05) as f64;
            return Some((c.start_ms + into.round() as u64).min(c.end_ms.saturating_sub(1)));
        }
        at += len;
    }
    None
}

/// The result time where source time `s_ms` is shown (`None` if it was cut).
pub fn result_at(edit: &Edit, s_ms: u64) -> Option<u64> {
    let mut at = 0u64;
    for c in &edit.clips {
        if c.start_ms <= s_ms && s_ms < c.end_ms {
            return Some(at + ((s_ms - c.start_ms) as f64 / c.speed.max(0.05) as f64).round() as u64);
        }
        at += clip_len(c);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{Clip, Edit};

    #[test]
    fn cuts_and_speed_map_both_ways() {
        let mut e = Edit::new(10_000);
        e.split(4_000);
        e.split(6_000);
        e.remove_clip_at(5_000); // 4–6 s cut
        e.clips[1].speed = 2.0; // 6–10 s twice as fast
        assert_eq!(e.clips, vec![Clip { start_ms: 0, end_ms: 4_000, speed: 1.0 }, Clip { start_ms: 6_000, end_ms: 10_000, speed: 2.0 }]);
        assert_eq!(duration_ms(&e), 6_000);
        assert_eq!(source_at(&e, 1_000), Some(1_000));
        assert_eq!(source_at(&e, 4_000), Some(6_000));
        assert_eq!(source_at(&e, 5_000), Some(8_000));
        assert_eq!(source_at(&e, 6_000), None);
        assert_eq!(result_at(&e, 5_000), None, "cut");
        assert_eq!(result_at(&e, 8_000), Some(5_000));
    }
}
