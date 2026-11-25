//! The platform encoders on Windows: the GPUs' hardware video encoders (NVIDIA, AMD, Intel)
//! through Media Foundation, fed Direct3D 11 textures (drawn into their input slots, or copied
//! in on the GPU), and sound through Media Foundation's AAC encoder (Opus in software).

mod audio;
mod blit;
mod d3d;
mod mf;
mod video;

pub use audio::{AacEncoder, AudioEncoder};
pub use blit::{Blit, video_device};
pub use mf::{EncoderInfo, Vendor};
pub use video::{InputSlot, Options, RateControl, VideoEncoder, hardware_encoders};

pub use crate::encoder::{
    AudioSettings, EncodedAudio, EncodedVideo, Hardware, InputFormat, VideoOutput, VideoSettings, VideoSink,
};
