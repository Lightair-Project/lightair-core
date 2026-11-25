//! How long to wait before connecting again: 0.5, 1, 2, 4, 8 s, then every 8 s,
//! back to 0.5 s once a connection has lasted a minute.

use std::time::{Duration, Instant};

const WAITS_MS: [u64; 5] = [500, 1000, 2000, 4000, 8000];
const STEADY: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct Backoff {
    step: usize,
    connected_at: Option<Instant>,
}

impl Backoff {
    /// A connection is up.
    pub fn connected(&mut self, now: Instant) {
        self.connected_at = Some(now);
    }

    /// The wait before the next attempt (a connection, if there was one, has just ended).
    pub fn next(&mut self, now: Instant) -> Duration {
        if self
            .connected_at
            .take()
            .is_some_and(|t| now.duration_since(t) >= STEADY)
        {
            self.step = 0;
        }
        let wait = WAITS_MS[self.step.min(WAITS_MS.len() - 1)];
        self.step += 1;
        Duration::from_millis(wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_double_to_8_s_and_start_again_after_a_steady_minute() {
        let t = Instant::now();
        let mut b = Backoff::default();
        let waits: Vec<u64> = (0..7).map(|_| b.next(t).as_millis() as u64).collect();
        assert_eq!(waits, [500, 1000, 2000, 4000, 8000, 8000, 8000]);
        // a short connection doesn't reset it
        b.connected(t);
        assert_eq!(b.next(t + Duration::from_secs(5)).as_millis(), 8000);
        b.connected(t);
        assert_eq!(b.next(t + Duration::from_secs(61)).as_millis(), 500);
    }
}
