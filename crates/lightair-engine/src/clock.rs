//! The program clock, locked to audio: program time counts samples at
//! 48 kHz; a slowly corrected line maps a sample count to the host clock, from the times the
//! master feed's sound was heard. Video frame `n` is due when that line reaches `n / fps`.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

pub const RATE: u32 = 48_000;

/// Nanoseconds on the host clock: `mach_absolute_time` (Apple), `CLOCK_BOOTTIME` (Android), the
/// performance counter (Windows, the clock its captures and encoders stamp with);
/// `CLOCK_MONOTONIC` elsewhere (tests).
pub fn host_now_ns() -> i64 {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
            fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
        }
        static BASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        let (n, d) = *BASE.get_or_init(|| {
            let mut i = [0u32; 2];
            unsafe { mach_timebase_info(&mut i) };
            (i[0].max(1), i[1].max(1))
        });
        (unsafe { mach_absolute_time() } as u128 * n as u128 / d as u128) as i64
    }
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn QueryPerformanceCounter(count: *mut i64) -> i32;
            fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
        }
        static FREQUENCY: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut f = 0;
            unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        });
        let mut count = 0;
        unsafe { QueryPerformanceCounter(&mut count) };
        (count as i128 * 1_000_000_000 / frequency as i128) as i64
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
    {
        let mut ts = libc_timespec { tv_sec: 0, tv_nsec: 0 };
        #[cfg(target_os = "android")]
        const CLOCK: i32 = 7; // CLOCK_BOOTTIME
        #[cfg(not(target_os = "android"))]
        const CLOCK: i32 = 1; // CLOCK_MONOTONIC
        unsafe { clock_gettime(CLOCK, &mut ts) };
        ts.tv_sec * 1_000_000_000 + ts.tv_nsec
    }
}

/// `CLOCK_MONOTONIC` now (what an Android encoder surface stamps frames with).
#[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
pub fn monotonic_now_ns() -> i64 {
    let mut ts = libc_timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(1, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// The monotonic clock now: on Windows the performance counter, as `host_now_ns` reads it.
#[cfg(windows)]
pub fn monotonic_now_ns() -> i64 {
    host_now_ns()
}

#[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
#[repr(C)]
struct libc_timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
unsafe extern "C" {
    fn clock_gettime(clock: i32, ts: *mut libc_timespec) -> i32;
}

/// Program samples to nanoseconds.
pub fn samples_to_ns(samples: u64) -> i64 {
    (samples as i128 * 1_000_000_000 / RATE as i128) as i64
}

/// Frame `n`'s program time at `fps`.
pub fn frame_ns(n: u64, fps: u32) -> i64 {
    (n as i128 * 1_000_000_000 / fps.max(1) as i128) as i64
}

pub struct ProgramClock {
    samples: AtomicU64,
    fit: Mutex<Fit>,
}

#[derive(Debug, Clone, Copy)]
struct Fit {
    /// Host time of program sample 0.
    origin_ns: f64,
    ns_per_sample: f64,
    observed: bool,
}

const NOMINAL: f64 = 1e9 / RATE as f64;

impl ProgramClock {
    /// A clock whose program time 0 is `start_ns` on the host clock.
    pub fn new(start_ns: i64) -> ProgramClock {
        ProgramClock {
            samples: AtomicU64::new(0),
            fit: Mutex::new(Fit { origin_ns: start_ns as f64, ns_per_sample: NOMINAL, observed: false }),
        }
    }

    /// Program samples produced so far.
    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::Acquire)
    }

    pub fn now_ns(&self) -> i64 {
        samples_to_ns(self.samples())
    }

    /// The mix thread produced `frames` more (it alone advances the count).
    pub fn produced(&self, frames: u64) {
        self.samples.fetch_add(frames, Ordering::AcqRel);
    }

    /// Program sample `at` was heard at `host_ns`: the line moves a little towards it (a
    /// first-order loop: a jittery callback moves it by 1/32 of its error; the rate follows the
    /// device's real clock over a few seconds).
    pub fn observe(&self, at: u64, host_ns: i64) {
        let mut f = self.fit.lock();
        if !f.observed {
            f.origin_ns = host_ns as f64 - at as f64 * f.ns_per_sample;
            f.observed = true;
            return;
        }
        let predicted = f.origin_ns + at as f64 * f.ns_per_sample;
        let err = host_ns as f64 - predicted;
        // a jump over 100 ms (a route change, an interruption): start the line again there
        if err.abs() > 100e6 {
            f.origin_ns = host_ns as f64 - at as f64 * f.ns_per_sample;
            return;
        }
        f.origin_ns += err / 32.0;
        if at > RATE as u64 {
            let rate_err = err / at as f64;
            f.ns_per_sample = (f.ns_per_sample + rate_err / 256.0).clamp(NOMINAL * 0.99, NOMINAL * 1.01);
        }
    }

    /// When program sample `at` is (or was) heard, on the host clock.
    pub fn host_at(&self, at: u64) -> i64 {
        let f = *self.fit.lock();
        (f.origin_ns + at as f64 * f.ns_per_sample) as i64
    }

    /// The program time being drawn now (ahead of `now_ns`, the sound mixed so far, by however
    /// late the microphone's sound arrives).
    pub fn drawn_ns(&self) -> i64 {
        samples_to_ns(self.sample_at(host_now_ns()))
    }

    /// The program sample being heard at `host_ns`.
    pub fn sample_at(&self, host_ns: i64) -> u64 {
        let f = *self.fit.lock();
        ((host_ns as f64 - f.origin_ns) / f.ns_per_sample).max(0.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_follows_a_jittery_device_without_drift() {
        // a device 0.1 % fast, its callbacks jittering by ±2 ms, for an hour of 10 ms blocks
        let clock = ProgramClock::new(0);
        let start = 5_000_000_000i64;
        let mut worst_late = 0.0f64;
        let mut seed = 1u64;
        for block in 0..360_000u64 {
            let at = block * 480;
            let truth = start as f64 + at as f64 * NOMINAL * 0.999;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter = ((seed >> 33) as f64 / (1u64 << 31) as f64 - 0.5) * 4e6;
            clock.observe(at, (truth + jitter) as i64);
            if block > 1000 {
                worst_late = worst_late.max((clock.host_at(at) as f64 - truth).abs());
            }
        }
        // the line stays within a millisecond of the device's real clock after settling
        assert!(worst_late < 1e6, "{worst_late}");
        // and video frame times are exact program times whatever the line does
        assert_eq!(frame_ns(30, 30), 1_000_000_000);
        assert_eq!(samples_to_ns(48_000), 1_000_000_000);
    }
}
