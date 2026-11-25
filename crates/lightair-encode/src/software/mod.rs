//! Encoders that run on the CPU, for machines or platforms without a hardware or system
//! encoder: H.264 through OpenH264 (the `openh264` feature) and Opus (on Windows, whose media
//! framework has no Opus encoder).

#[cfg(feature = "openh264")]
mod h264;
#[cfg(windows)]
mod opus;

#[cfg(feature = "openh264")]
pub use h264::{Frame, H264Encoder};
#[cfg(windows)]
pub use opus::OpusEncoder;
