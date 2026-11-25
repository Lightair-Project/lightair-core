//! Opus in software: the reference encoder as pure Rust (no C compiler, no system library), for
//! platforms whose own media frameworks have no Opus encoder (Windows).
//!
//! Interleaved 32-bit float PCM at 48 kHz goes in as it comes; whole 20 ms packets (960 frames)
//! come out. A packet's time is the program time of the input frame it starts at, counted from
//! the first frame ever given; the encoder's look-ahead is the format's `priming_frames`
//! (`OpusHead`'s pre-skip) for files to trim.

use unsafe_libopus::varargs::{VarArg, VarArgs};
use unsafe_libopus::{
    OPUS_APPLICATION_AUDIO, OPUS_GET_LOOKAHEAD_REQUEST, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
    OPUS_SET_SIGNAL_REQUEST, OPUS_SET_VBR_REQUEST, OPUS_SIGNAL_MUSIC, OpusEncoder as RawEncoder, opus_encode_float,
    opus_encoder_create, opus_encoder_ctl_impl, opus_encoder_destroy,
};

use crate::Error;
use crate::encoder::{AudioEncode, AudioSettings, EncodedAudio};
use crate::format::{AudioCodec, AudioFormat};

/// The largest packet asked for: what the Opus specification recommends for one frame.
const MAX_PACKET: usize = 4000;

pub struct OpusEncoder {
    raw: *mut RawEncoder,
    format: AudioFormat,
    /// Interleaved frames waiting for a whole packet.
    pending: Vec<f32>,
    /// The program time of the first frame ever given, and packets made since.
    start: Option<i64>,
    made: i64,
    out: Vec<u8>,
    ended: bool,
}

// The encoder's state is plain memory used by one thread at a time (the encoder's owner).
unsafe impl Send for OpusEncoder {}

fn ctl(raw: *mut RawEncoder, request: i32, value: i32) -> i32 {
    unsafe { opus_encoder_ctl_impl(raw, request, VarArgs::new(vec![VarArg::I32(value)])) }
}

impl OpusEncoder {
    /// An encoder for `settings` (Opus, 48 kHz, mono or stereo).
    pub fn new(settings: AudioSettings) -> Result<OpusEncoder, Error> {
        if settings.codec != AudioCodec::Opus {
            return Err(Error::Unsupported(format!("the Opus encoder can't make {}", settings.codec.name())));
        }
        if settings.sample_rate != 48_000 {
            return Err(Error::Unsupported("Opus is encoded at 48 kHz only".into()));
        }
        let channels = settings.channels.clamp(1, 2);
        let mut status = 0i32;
        let raw = unsafe { opus_encoder_create(48_000, channels as i32, OPUS_APPLICATION_AUDIO, &mut status) };
        if raw.is_null() || status != OPUS_OK {
            return Err(Error::Platform { what: "the Opus encoder wouldn't start", status });
        }
        let kbps = settings.bitrate_kbps.clamp(6, 510);
        let status = ctl(raw, OPUS_SET_BITRATE_REQUEST, kbps as i32 * 1000);
        if status != OPUS_OK {
            unsafe { opus_encoder_destroy(raw) };
            return Err(Error::Platform { what: "the Opus encoder refused the bitrate", status });
        }
        // constrained VBR at the full complexity, tuned for music and mixed program sound
        ctl(raw, OPUS_SET_VBR_REQUEST, 1);
        ctl(raw, OPUS_SET_COMPLEXITY_REQUEST, 10);
        ctl(raw, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC);
        let mut lookahead = 0i32;
        let status = unsafe {
            opus_encoder_ctl_impl(raw, OPUS_GET_LOOKAHEAD_REQUEST, VarArgs::new(vec![VarArg::I32Out(&mut lookahead)]))
        };
        let priming = if status == OPUS_OK && lookahead > 0 { lookahead as u32 } else { 312 };
        Ok(OpusEncoder {
            raw,
            format: AudioFormat::new(AudioCodec::Opus, 48_000, channels, kbps, priming),
            pending: Vec::new(),
            start: None,
            made: 0,
            out: vec![0; MAX_PACKET],
            ended: false,
        })
    }

    /// The stream's format: `OpusHead` and the pre-skip.
    pub fn format(&self) -> &AudioFormat {
        &self.format
    }

    /// The bitrate from the next packet on (Opus changes it between any two packets).
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), Error> {
        let kbps = kbps.clamp(6, 510);
        let status = ctl(self.raw, OPUS_SET_BITRATE_REQUEST, kbps as i32 * 1000);
        if status != OPUS_OK {
            return Err(Error::Platform { what: "the Opus encoder refused the bitrate", status });
        }
        self.format.bitrate_kbps = kbps;
        Ok(())
    }

    /// Encodes `interleaved` frames starting at program time `pts_ns`; returns the packets
    /// they completed. The sound is taken as continuous: times after the first only matter
    /// for the first.
    pub fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error> {
        if self.ended {
            return Err(Error::Mux("the sound encoder has been flushed".into()));
        }
        self.start.get_or_insert(pts_ns);
        self.pending.extend_from_slice(interleaved);
        let per = self.per_packet();
        let mut out = Vec::new();
        let mut used = 0;
        while self.pending.len() - used >= per {
            let packet = self.encode_one(used)?;
            used += per;
            out.push(packet);
        }
        self.pending.drain(..used);
        Ok(out)
    }

    /// The last partial packet (padded with silence), then silence for the encoder's
    /// look-ahead, so the last real sound comes out; the encoder takes no more after.
    pub fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        if self.ended {
            return Ok(Vec::new());
        }
        let per = self.per_packet();
        let frames_per_packet = self.format.frames_per_packet() as usize;
        let mut silent_packets = (self.format.priming_frames as usize).div_ceil(frames_per_packet);
        if !self.pending.is_empty() {
            let len = self.pending.len().div_ceil(per) * per;
            self.pending.resize(len, 0.0);
        } else if self.start.is_none() {
            silent_packets = 0;
        }
        self.pending.resize(self.pending.len() + silent_packets * per, 0.0);
        let mut out = Vec::new();
        let mut used = 0;
        while self.pending.len() - used >= per {
            out.push(self.encode_one(used)?);
            used += per;
        }
        self.pending.clear();
        self.ended = true;
        Ok(out)
    }

    fn per_packet(&self) -> usize {
        self.format.frames_per_packet() as usize * self.format.channels as usize
    }

    /// One packet from the pending frames starting at `at`.
    fn encode_one(&mut self, at: usize) -> Result<EncodedAudio, Error> {
        let frames = self.format.frames_per_packet();
        let n = unsafe {
            opus_encode_float(
                self.raw,
                self.pending[at..].as_ptr(),
                frames as i32,
                self.out.as_mut_ptr(),
                self.out.len() as i32,
            )
        };
        if n < 0 {
            return Err(Error::Platform { what: "the Opus encoder failed", status: n });
        }
        let start = self.start.unwrap_or(0);
        let pts_ns = start + self.made * frames as i64 * 1_000_000_000 / 48_000;
        self.made += 1;
        Ok(EncodedAudio { data: self.out[..n as usize].to_vec(), pts_ns, frames })
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.raw) };
    }
}

impl AudioEncode for OpusEncoder {
    fn format(&self) -> &AudioFormat {
        OpusEncoder::format(self)
    }

    fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error> {
        OpusEncoder::encode(self, interleaved, pts_ns)
    }

    fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        OpusEncoder::flush(self)
    }
}
