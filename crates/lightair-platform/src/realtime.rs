//! Real-time scheduling for the audio thread: without it, a busy computer (encoding, a game) can
//! starve the mixer and the stream drops out.

use std::time::Duration;

/// Asks the OS to schedule the calling thread as real-time audio that runs every `period`.
/// Returns false if refused (the thread keeps running at normal priority).
pub fn promote_audio_thread(period: Duration) -> bool {
    imp::promote(period)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::time::Duration;

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    #[repr(C)]
    struct TimeConstraint {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
        fn mach_thread_self() -> u32;
        fn thread_policy_set(thread: u32, flavor: u32, info: *mut TimeConstraint, count: u32) -> i32;
    }
    const TIME_CONSTRAINT_POLICY: u32 = 2;
    const TIME_CONSTRAINT_COUNT: u32 = 4;

    /// As CoreAudio's I/O threads: a slot every period, up to half of it to compute, done within
    /// the period.
    pub fn promote(period: Duration) -> bool {
        unsafe {
            let mut tb = Timebase { numer: 0, denom: 0 };
            mach_timebase_info(&mut tb);
            let abs = |d: Duration| (d.as_nanos() as f64 * tb.denom.max(1) as f64 / tb.numer.max(1) as f64) as u32;
            let mut policy = TimeConstraint {
                period: abs(period),
                computation: abs(period / 2),
                constraint: abs(period),
                preemptible: 1,
            };
            thread_policy_set(mach_thread_self(), TIME_CONSTRAINT_POLICY, &mut policy, TIME_CONSTRAINT_COUNT) == 0
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    /// The Multimedia Class Scheduler's "Pro Audio" task, as DAWs use.
    pub fn promote(_period: Duration) -> bool {
        use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut index = 0u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
        !handle.is_null()
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn promote(_period: std::time::Duration) -> bool {
        false
    }
}
