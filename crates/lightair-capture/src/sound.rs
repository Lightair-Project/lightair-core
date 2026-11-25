//! The sound the computer plays (`lightair_model::kinds::DESKTOP_AUDIO`) or one app plays
//! (`APP_AUDIO`), through Core Audio process taps (macOS 14.2 and later).
//!
//! A process tap is Core Audio's own: no picture is involved, so the window server does no work
//! for it (a ScreenCaptureKit stream's sound has the display composed all along). The tap is
//! read through a private aggregate device whose I/O callback hands the samples over with their
//! host times. macOS asks the user the first time ("System Audio Recording").
//!
//! Microphones and audio interfaces aren't here: the desktop's audio engine captures those
//! itself.

use crate::{Error, SoundSink};

#[cfg(target_os = "macos")]
use crate::macos::sound as imp;
#[cfg(not(target_os = "macos"))]
use crate::other::sound as imp;

/// Whose sound to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Everything the computer plays, mixed to stereo; Lightair's own sound left out (so
    /// monitoring doesn't come back in) unless `include_own`.
    System { include_own: bool },
    /// One app's sound (by bundle id, its helper processes included), mixed to stereo. Followed
    /// as it quits and opens again: silent while it isn't running.
    App(String),
}

/// An app that has sound on Core Audio now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundApp {
    pub bundle: String,
    pub pid: i32,
    /// It's playing something right now.
    pub playing: bool,
}

/// Whether process taps work here (macOS 14.2 and later).
pub fn supported() -> bool {
    imp::supported()
}

/// The processes Core Audio knows to have sound, by bundle id (one entry an app, helpers
/// folded into the app they belong to where their bundle id says so).
pub fn apps() -> Vec<SoundApp> {
    imp::apps()
}

/// A running capture of the computer's or an app's sound. Captures until dropped.
pub struct SoundCapture {
    inner: imp::Capture,
}

impl SoundCapture {
    /// Starts capturing into `sink`. For [`Target::System`] the tap is made before this returns
    /// (its failure is returned); for [`Target::App`] it's made, and made again as the app's
    /// processes come and go, on a thread of the capture's own.
    pub fn start(target: &Target, sink: impl SoundSink) -> Result<SoundCapture, Error> {
        Ok(SoundCapture { inner: imp::Capture::start(target, Box::new(sink))? })
    }

    /// The tap's format now: (frames a second, channels); `None` while there's no tap (an app
    /// that isn't running).
    pub fn format(&self) -> Option<(u32, u32)> {
        self.inner.format()
    }
}

impl std::fmt::Debug for SoundCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundCapture").field("format", &self.format()).finish()
    }
}
