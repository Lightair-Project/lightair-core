//! Studio's thermal governor: the steps a phone takes down as it heats, in
//! a stated order, so the show keeps going instead of the system stopping the camera.
//!
//! | Step | What the engine does | Taken when |
//! |---|---|---|
//! | 1 | Preview at half its rate | Fair (or the battery under 10 % with no power) |
//! | 2 | (A second format off) | Fair for 60 s (or the battery as above) |
//! | 3 | (Costly effects off) | Serious |
//! | 4 | The stream's frame rate halved (60 → 30) | Serious for 60 s |
//! | 5 | The stream's bitrate down a rung (to 56 %, 1080p's to 720p's) | Serious for 120 s |
//! | 6 | Preview at a quarter of its rate | Critical |
//! | 7 | The recording stopped; after 60 s more, the stream ended cleanly | Critical for 60 s |
//!
//! A worse state takes its first step at once and climbs while it lasts; steps are undone one at
//! a time after 120 s of nominal (fair holds it at 1–2 at least).

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Thermal {
    Nominal,
    Fair,
    Serious,
    Critical,
}

/// What the engine does at a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    /// Draw the preview every nth frame.
    pub preview_every: u32,
    /// Send the encoders every nth frame.
    pub encode_every: u32,
    /// The stream's bitrate, in percent of the chosen.
    pub bitrate_percent: u32,
    /// Stop the recording's encode.
    pub stop_recording: bool,
    /// End the stream, cleanly.
    pub end_stream: bool,
}

pub struct Governor {
    pub step: u8,
    /// When the state last changed, and when a step was last taken or undone.
    since_state: Instant,
    since_step: Instant,
    state: Thermal,
    /// At step 7: when it was reached (the stream ends 60 s later if still critical).
    at_seven: Option<Instant>,
}

impl Governor {
    pub fn new(now: Instant) -> Governor {
        Governor {
            step: 0,
            since_state: now,
            since_step: now,
            state: Thermal::Nominal,
            at_seven: None,
        }
    }

    /// The state now; returns the step's message when it changed.
    pub fn update(
        &mut self,
        thermal: Thermal,
        battery: f32,
        charging: bool,
        now: Instant,
    ) -> Option<&'static str> {
        if thermal != self.state {
            self.state = thermal;
            self.since_state = now;
        }
        let held = now.duration_since(self.since_state);
        let low_battery = battery < 0.10 && !charging;
        // the step this state warrants now
        let mut want = match thermal {
            Thermal::Nominal => 0,
            Thermal::Fair if held >= Duration::from_secs(60) => 2,
            Thermal::Fair => 1,
            Thermal::Serious if held >= Duration::from_secs(120) => 5,
            Thermal::Serious if held >= Duration::from_secs(60) => 4,
            Thermal::Serious => 3,
            Thermal::Critical if held >= Duration::from_secs(60) => 7,
            Thermal::Critical => 6,
        };
        if low_battery {
            want = want.max(2);
        }
        let before = self.step;
        if want > self.step {
            self.step = want;
            self.since_step = now;
        } else if want < self.step
            && now.duration_since(self.since_step) >= Duration::from_secs(120)
        {
            // undone one at a time, every 120 s of a better state
            self.step -= 1;
            self.since_step = now;
        }
        if self.step == 7 {
            self.at_seven.get_or_insert(now);
        } else {
            self.at_seven = None;
        }
        (self.step != before).then(|| message(self.step))
    }

    pub fn effects(&self, now: Instant) -> Effects {
        let s = self.step;
        Effects {
            preview_every: if s >= 6 {
                4
            } else if s >= 1 {
                2
            } else {
                1
            },
            encode_every: if s >= 4 { 2 } else { 1 },
            bitrate_percent: if s >= 5 { 56 } else { 100 },
            stop_recording: s >= 7,
            end_stream: self
                .at_seven
                .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(60)),
        }
    }
}

pub fn message(step: u8) -> &'static str {
    match step {
        0 => "The phone has cooled down: everything is back.",
        1 => "The phone is warm: the preview runs at half speed to keep it cool.",
        2 => "The phone is still warm: keeping things light.",
        3 => "The phone is hot: costly effects are off.",
        4 => "The phone is hot: streaming at 30 frames a second until it cools.",
        5 => "The phone is hot: streaming at a lower quality until it cools.",
        6 => "The phone is very hot: the preview is slowed right down.",
        _ => "The phone is too hot: recording stopped to keep the stream going.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_steps_down_in_order_and_cooling_undoes_one_at_a_time() {
        let t0 = Instant::now();
        let mut g = Governor::new(t0);
        assert_eq!(g.update(Thermal::Fair, 0.8, false, t0), Some(message(1)));
        assert_eq!(g.effects(t0).preview_every, 2);
        g.update(Thermal::Fair, 0.8, false, t0 + Duration::from_secs(61));
        assert_eq!(g.step, 2);
        g.update(Thermal::Serious, 0.8, false, t0 + Duration::from_secs(62));
        assert_eq!(g.step, 3);
        g.update(Thermal::Serious, 0.8, false, t0 + Duration::from_secs(123));
        assert_eq!((g.step, g.effects(t0).encode_every), (4, 2));
        g.update(Thermal::Serious, 0.8, false, t0 + Duration::from_secs(183));
        assert_eq!((g.step, g.effects(t0).bitrate_percent), (5, 56));
        let c = t0 + Duration::from_secs(200);
        g.update(Thermal::Critical, 0.8, false, c);
        assert_eq!(g.step, 6);
        g.update(Thermal::Critical, 0.8, false, c + Duration::from_secs(60));
        assert!(g.effects(c + Duration::from_secs(60)).stop_recording);
        assert!(!g.effects(c + Duration::from_secs(60)).end_stream);
        g.update(Thermal::Critical, 0.8, false, c + Duration::from_secs(120));
        assert!(g.effects(c + Duration::from_secs(120)).end_stream);
        // cooling: one step every 120 s
        let n = c + Duration::from_secs(121);
        g.update(Thermal::Nominal, 0.8, false, n);
        assert_eq!(g.step, 7, "not at once");
        g.update(Thermal::Nominal, 0.8, false, n + Duration::from_secs(120));
        assert_eq!(g.step, 6);
        // a low battery holds step 2 whatever the heat
        let mut g = Governor::new(t0);
        g.update(Thermal::Nominal, 0.05, false, t0);
        assert_eq!(g.step, 2);
    }
}
