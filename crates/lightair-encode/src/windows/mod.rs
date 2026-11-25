//! The platform encoders on Windows: the GPUs' hardware video encoders (NVIDIA, AMD, Intel)
//! through Media Foundation, fed Direct3D 11 textures without a copy, and sound through Media
//! Foundation's AAC encoder (Opus in software).

mod audio;
mod d3d;
mod mf;
mod video;

pub use audio::{AacEncoder, AudioEncoder};
pub use mf::{EncoderInfo, Vendor};
pub use video::{InputSlot, Options, RateControl, VideoEncoder, hardware_encoders};

pub use crate::encoder::{
    AudioSettings, EncodedAudio, EncodedVideo, Hardware, InputFormat, VideoOutput, VideoSettings, VideoSink,
};
