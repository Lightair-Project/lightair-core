//! The platform encoders on macOS and iOS: VideoToolbox for pictures, AudioToolbox for sound.

mod audio;
mod video;

pub use audio::{AudioEncoder, AudioSettings, EncodedAudio};
pub use video::{EncodedVideo, Hardware, InputFormat, VideoEncoder, VideoOutput, VideoSettings, VideoSink};
