//! What the engine asks of the platform, and what it tells the interface. The facade maps
//! these to its UniFFI types one to one.

use lightair_model::Id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facing {
    Front,
    Back,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CameraRequest {
    pub facing: Facing,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// An external camera's id (iPadOS `AVCaptureDevice.uniqueID`, Android Camera2's
    /// id), opened rather than the built-in one `facing` names.
    pub device: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaRequest {
    pub path: String,
    pub looped: bool,
    pub with_sound: bool,
    pub audio_feed: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoEncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval_s: u32,
    pub realtime: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioEncoderConfig {
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate_kbps: u32,
    pub codec: AudioCodec,
}

/// AAC (RTMP, SRT, recordings) or Opus (WHIP: 20 ms packets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCodec {
    #[default]
    Aac,
    Opus,
}

/// Where the render thread draws an encoder's frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderTarget {
    /// iOS: a `CVPixelBufferPoolRef` (+1 reference, given up) of BGRA IOSurface buffers.
    PixelBufferPool(u64),
    /// Android: an `ANativeWindow*` (+1 reference, given up): MediaCodec's input surface.
    NativeWindow(u64),
    /// Tests: frames are kept in the encoder's queue as textures.
    Texture,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordingRequest {
    pub path: String,
    pub video_encoder: u32,
    pub audio_encoder: Option<u32>,
    pub fragment_interval_s: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    BuiltIn,
    Wired,
    Bluetooth,
    Usb,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInput {
    pub id: String,
    pub name: String,
    pub kind: InputKind,
    pub latency_ms: u32,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum HostError {
    #[error("{0}")]
    Permission(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
}

/// The platform side (Swift, Kotlin). Called on the control thread only.
pub trait Host: Send + Sync {
    fn start_camera(&self, feed: u32, request: CameraRequest) -> Result<(), HostError>;
    fn stop_camera(&self, feed: u32);
    fn start_microphone(&self, feed: u32, input: Option<String>) -> Result<(), HostError>;
    fn stop_microphone(&self, feed: u32);
    fn audio_inputs(&self) -> Vec<AudioInput>;
    fn open_media(&self, feed: u32, request: MediaRequest) -> Result<(), HostError>;
    fn close_media(&self, feed: u32);
    fn open_video_encoder(
        &self,
        encoder: u32,
        config: VideoEncoderConfig,
    ) -> Result<EncoderTarget, HostError>;
    fn set_video_bitrate(&self, encoder: u32, kbps: u32);
    fn close_video_encoder(&self, encoder: u32);
    fn open_audio_encoder(&self, encoder: u32, config: AudioEncoderConfig)
    -> Result<(), HostError>;
    fn close_audio_encoder(&self, encoder: u32);
    fn start_recording(&self, recording: u32, request: RecordingRequest) -> Result<(), HostError>;
    fn stop_recording(&self, recording: u32);
    fn keep_screen_on(&self, on: bool);
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecordingState {
    Idle,
    Starting,
    Recording { since_ms: u64, file: String },
    Finishing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecordingResult {
    Finished { duration_ms: u64, bytes: u64 },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeedState {
    Running,
    Interrupted { reason: String },
    Failed { reason: String },
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub track: Id,
    pub peak_db: f32,
    pub rms_db: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveState {
    Off,
    Starting,
    Live { since_ms: u64 },
    Ending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkKind {
    Wifi,
    Cellular,
    Wired,
    None,
}

/// What the host knows about the phone (every few seconds, and when it changes).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceState {
    pub thermal: crate::governor::Thermal,
    /// 0–1.
    pub battery: f32,
    pub charging: bool,
    pub low_power: bool,
    pub network: NetworkKind,
    pub upload_kbps: Option<u32>,
    /// The networks there are now, for destinations sent on one.
    pub networks: Vec<NetworkPath>,
}

/// A network a destination can be sent on: iOS by its interface (`en0`, `pdp_ip0`, from
/// `NWPath.availableInterfaces`), Android by its `Network.getNetworkHandle()`.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkPath {
    pub kind: NetworkKind,
    pub interface: String,
    pub handle: u64,
}

impl NetworkPath {
    /// What binds a sender's socket to it.
    pub fn bind(&self) -> Option<lightair_feed::net::Bind> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        return lightair_feed::net::Bind::interface(&self.interface).ok();
        #[cfg(target_os = "android")]
        return Some(lightair_feed::net::Bind::network(self.handle));
        #[allow(unreachable_code)]
        None
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        DeviceState {
            thermal: crate::governor::Thermal::Nominal,
            battery: 1.0,
            charging: true,
            low_power: false,
            network: NetworkKind::Wifi,
            upload_kbps: None,
            networks: Vec::new(),
        }
    }
}

/// Once a second while live or recording.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthReport {
    pub outputs: Vec<crate::outputs::OutputHealth>,
    pub device: DeviceState,
    pub governor_step: u8,
    pub governor_message: Option<String>,
    /// Ordered start: destinations still to start, in order, and the one that didn't
    /// (waiting for `skip_destination` or `end_live`).
    pub queued: Vec<String>,
    pub blocked: Option<String>,
    /// Stopped while live: `start_destination` starts one again.
    pub stopped: Vec<String>,
    /// Frames dropped since going live, by cause: the network (every destination's
    /// sender), the encoders (frames they didn't take in time), the render (frames drawn late).
    pub dropped_network: u64,
    pub dropped_encoder: u64,
    pub dropped_render: u64,
}

/// What the interface hears, on the control thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Changed {
        version: u64,
    },
    LayerAdded {
        layer: Id,
    },
    Meters {
        levels: Vec<Level>,
        program_lufs: f32,
    },
    Recording {
        state: RecordingState,
    },
    Issue {
        id: String,
        text: String,
        fix: Option<String>,
    },
    IssueCleared {
        id: String,
    },
    Stats {
        fps: f32,
        render_ms: f32,
        dropped: u32,
    },
    Live {
        state: LiveState,
    },
    /// An alert begins: play its sound and speak its words into `sound_feed`.
    Alert {
        alert: crate::alerts::Alert,
        sound_feed: u32,
    },
    /// New chat, oldest first: messages, and the events alerts come from.
    Chat {
        messages: Vec<lightair_live::ChatEvent>,
    },
    Health {
        report: HealthReport,
    },
    Failed {
        reason: String,
    },
}

pub trait Events: Send + Sync {
    fn event(&self, event: Event);
}
