//! A phone's encoded picture and sound to a desktop. Part of the **mobile
//! kit**. The platform encodes (VideoToolbox, MediaCodec); this muxes the access units into an
//! MPEG transport stream ([`ts`]) and sends it to the desktop's SRT listener ([`srt`]), the phone
//! calling, with the source in the stream id and the passphrase from the control channel.

pub mod backoff;
pub mod bpm;
pub mod feed;
pub mod governor;
pub mod net;
pub mod rtmp;
pub mod srt;
pub mod srtla;
pub mod ts;

pub use feed::{Feed, FeedStats, TsSink};
pub use rtmp::{RtmpAudioTrack, RtmpSender, RtmpTarget, RtmpTrack, RtmpTracks, RtmpVideo};
pub use srt::{SrtSender, SrtTarget};
pub use srtla::{LinkStats, Relay as SrtlaRelay};
pub use ts::{AudioConfig, NalFormat, VideoCodec};
