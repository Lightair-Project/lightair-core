//! Camera mode's thermal governor: the short ladder a phone steps down when it gets
//! hot, so the camera keeps running instead of being stopped by the system.
//!
//! | Step | Action | Taken when |
//! |---|---|---|
//! | 0 | Nothing | Nominal |
//! | 1 | Encoder speed over quality, stabilisation off, Screen off suggested | Fair, or the battery under 10 % with no power |
//! | 2 | Frame rate down (60 → 30, 30 → 24) | Fair for 60 s, or the battery as above |
//! | 3 | Resolution one rung down (2160 → 1080 → 720 → 540), bitrate scaled to match | Serious |
//! | 4 | 720p at most, 15 fps, the screen off | Critical |
//! | 5 | The camera stops cleanly, with a message, before the system stops it | Critical for 60 s |
//!
//! Each step is taken only when the one before didn't bring the state back within 60 s (a new,
//! worse state takes its step at once). Steps are undone one at a time, every 120 s of nominal
//! (or fair, down to step 1). The state goes to the desktop in health.

use std::time::{Duration, Instant};

/// The phone's thermal state, as both platforms report it (iOS `ProcessInfo.thermalState`,
/// Android's thermal status mapped to the same four).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Thermal {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl Thermal {
    pub fn parse(s: &str) -> Thermal {
        match s {
            "fair" => Thermal::Fair,
            "serious" => Thermal::Serious,
            "critical" => Thermal::Critical,
            _ => Thermal::Nominal,
        }
    }
}

/// What the camera should do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub step: u8,
    /// Prefer encoding speed over quality.
    pub speed_priority: bool,
    pub stabilisation_off: bool,
    /// Suggest Screen off (step 1–3) or turn it on (step 4).
    pub screen_off: ScreenOff,
    /// The frame rate at most (`None`: as configured).
    pub max_fps: Option<u32>,
    /// The picture's height at most (`None`: as configured).
    pub max_height: Option<u32>,
    /// Stop the camera now (step 5).
    pub stop: bool,
    /// What to tell the person, and the desktop.
    pub message: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenOff {
    No,
    Suggest,
    Now,
}

/// One rung down from a height.
pub fn rung_below(height: u32) -> u32 {
    match height {
        h if h > 1080 => 1080,
        h if h > 720 => 720,
        _ => 540,
    }
}

/// A frame rate one step down.
pub fn fps_below(fps: u32) -> u32 {
    if fps > 30 { 30 } else { 24.min(fps) }
}

const HOLD: Duration = Duration::from_secs(60);
const UNDO: Duration = Duration::from_secs(120);

pub struct Governor {
    step: u8,
    /// When the current step was taken (the next waits 60 s from here).
    stepped: Instant,
    /// Since when it's been cool enough to undo a step.
    cool_since: Option<Instant>,
    /// The configured frame rate and height, for the limits' numbers.
    fps: u32,
    height: u32,
}

impl Governor {
    pub fn new(fps: u32, height: u32, now: Instant) -> Governor {
        Governor { step: 0, stepped: now, cool_since: None, fps, height }
    }

    /// The desktop changed the configuration.
    pub fn configure(&mut self, fps: u32, height: u32) {
        self.fps = fps;
        self.height = height;
    }

    /// Called about once a second with the phone's state; returns what to do.
    pub fn update(&mut self, thermal: Thermal, battery: f32, charging: bool, now: Instant) -> Limits {
        let low_battery = battery < 0.10 && !charging;
        // the step this state reaches at once, and where it may climb to while it lasts
        let (at_once, ceiling) = match thermal {
            Thermal::Nominal => (0, 0),
            Thermal::Fair => (1, 2),
            Thermal::Serious => (3, 3),
            Thermal::Critical => (4, 5),
        };
        let (at_once, ceiling) = if low_battery { (at_once.max(2), ceiling.max(2)) } else { (at_once, ceiling) };
        if at_once > self.step {
            self.step = at_once;
            self.stepped = now;
            self.cool_since = None;
        } else if self.step < ceiling && now.duration_since(self.stepped) >= HOLD {
            // the last step didn't bring it back within a minute
            self.step += 1;
            self.stepped = now;
            self.cool_since = None;
        } else if self.step > ceiling {
            // cooler than this step needs: undo one every two minutes
            let since = *self.cool_since.get_or_insert(now);
            if now.duration_since(since) >= UNDO {
                self.step -= 1;
                self.stepped = now;
                self.cool_since = Some(now);
            }
        } else {
            self.cool_since = None;
        }
        self.limits(low_battery)
    }

    fn limits(&self, low_battery: bool) -> Limits {
        let s = self.step;
        Limits {
            step: s,
            speed_priority: s >= 1,
            stabilisation_off: s >= 1,
            screen_off: match s {
                0 => ScreenOff::No,
                1..=3 => ScreenOff::Suggest,
                _ => ScreenOff::Now,
            },
            max_fps: match s {
                0 | 1 => None,
                2 | 3 => Some(fps_below(self.fps)),
                _ => Some(15.min(self.fps)),
            },
            max_height: match s {
                0..=2 => None,
                3 => Some(rung_below(self.height)),
                _ => Some(720.min(self.height)),
            },
            stop: s >= 5,
            message: match s {
                0 => None,
                1 | 2 if low_battery => Some("The battery is low: the camera is saving power"),
                1 => Some("The phone is warm: Screen off helps it cool down"),
                2 => Some("The phone is warm: a lower frame rate"),
                3 => Some("The phone is hot: a lower resolution"),
                4 => Some("The phone is very hot: 720p at 15 fps, screen off"),
                _ => Some("Lightair stopped the camera before the phone overheats"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_down_when_heat_lasts_and_back_when_it_cools() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut g = Governor::new(60, 1080, t0);
        assert_eq!(g.update(Thermal::Nominal, 0.8, false, at(0)).step, 0);
        // fair: step 1 at once, step 2 if it lasts a minute
        let l = g.update(Thermal::Fair, 0.8, false, at(1));
        assert_eq!((l.step, l.screen_off, l.max_fps), (1, ScreenOff::Suggest, None));
        assert_eq!(g.update(Thermal::Fair, 0.8, false, at(30)).step, 1);
        let l = g.update(Thermal::Fair, 0.8, false, at(61));
        assert_eq!((l.step, l.max_fps), (2, Some(30)));
        // fair never goes past 2
        assert_eq!(g.update(Thermal::Fair, 0.8, false, at(200)).step, 2);
        // serious: a rung down at once
        let l = g.update(Thermal::Serious, 0.8, false, at(201));
        assert_eq!((l.step, l.max_height), (3, Some(720)));
        // critical: 720p, 15 fps, screen off; a minute more and it stops
        let l = g.update(Thermal::Critical, 0.8, false, at(202));
        assert_eq!(
            (l.step, l.max_fps, l.max_height, l.screen_off, l.stop),
            (4, Some(15), Some(720), ScreenOff::Now, false)
        );
        assert!(g.update(Thermal::Critical, 0.8, false, at(263)).stop);
        // cooling: one step back every two minutes, not faster
        assert_eq!(g.update(Thermal::Nominal, 0.8, false, at(300)).step, 5);
        assert_eq!(g.update(Thermal::Nominal, 0.8, false, at(419)).step, 5);
        assert_eq!(g.update(Thermal::Nominal, 0.8, false, at(420)).step, 4);
        assert_eq!(g.update(Thermal::Nominal, 0.8, false, at(540)).step, 3);
        // fair holds it at 1 or 2 on the way back, nominal goes to 0
        let mut s = 540;
        while g.step > 2 {
            s += 120;
            g.update(Thermal::Fair, 0.8, false, at(s));
        }
        assert_eq!(g.step, 2);
        for _ in 0..4 {
            s += 120;
            g.update(Thermal::Nominal, 0.8, false, at(s));
        }
        assert_eq!(g.step, 0);
    }

    #[test]
    fn a_low_battery_saves_power_without_heat() {
        let t0 = Instant::now();
        let mut g = Governor::new(30, 1080, t0);
        let l = g.update(Thermal::Nominal, 0.05, false, t0);
        assert_eq!((l.step, l.max_fps), (2, Some(24)));
        assert!(l.message.is_some(), "the person is told why");
        // plugged in: it recovers
        let mut s = 0;
        for _ in 0..3 {
            s += 121;
            g.update(Thermal::Nominal, 0.05, true, t0 + Duration::from_secs(s));
        }
        assert_eq!(g.step, 0);
    }

    #[test]
    fn rungs_and_rates() {
        assert_eq!((rung_below(2160), rung_below(1080), rung_below(720)), (1080, 720, 540));
        assert_eq!((fps_below(60), fps_below(30), fps_below(25)), (30, 24, 24));
    }
}
