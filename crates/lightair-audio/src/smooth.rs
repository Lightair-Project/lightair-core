//! Click-free parameter changes: a gain moves to its target linearly over a short ramp.

/// A smoothed linear gain.
#[derive(Debug, Clone, Copy)]
pub struct Smoothed {
    current: f32,
    target: f32,
    /// Frames the ramp takes from one value to another.
    ramp: usize,
    /// Change per frame of the ramp in progress (fixed when the target is set, so it lands).
    step: f32,
}

impl Smoothed {
    pub fn new(value: f32, ramp: usize) -> Self {
        Self { current: value, target: value, ramp: ramp.max(1), step: 0.0 }
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
        self.step = (self.target - self.current) / self.ramp as f32;
    }

    /// Continues from where another one was (after a graph swap), ramping to this one's target.
    pub fn continue_from(&mut self, old: &Smoothed) {
        self.current = old.current;
        self.step = (self.target - self.current) / self.ramp as f32;
    }

    pub fn settled(&self) -> bool {
        self.current == self.target
    }

    /// The gain for each of the next `frames` frames, written to `out`.
    pub fn fill(&mut self, out: &mut [f32]) {
        if self.settled() {
            out.fill(self.current);
            return;
        }
        let step = self.step;
        if step == 0.0 {
            self.current = self.target; // a difference too small to ramp
            out.fill(self.current);
            return;
        }
        for g in out.iter_mut() {
            let next = self.current + step;
            self.current = if (step > 0.0 && next >= self.target) || (step < 0.0 && next <= self.target) {
                self.target
            } else {
                next
            };
            *g = self.current;
        }
    }
}
