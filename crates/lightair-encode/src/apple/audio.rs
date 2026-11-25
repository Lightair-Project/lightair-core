//! AAC-LC and Opus through AudioToolbox's `AudioConverter`.
//!
//! Interleaved 32-bit float PCM goes in as it comes (any number of frames); whole packets
//! (1024 frames for AAC, 960 for Opus) come out. A packet's time is the program time of the
//! input frame it starts at, counted from the first frame ever given, as the engine's sound
//! packets carry it; the encoder's priming (the format's `priming_frames`) is the file's to
//! trim.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterGetProperty, AudioConverterNew,
    AudioConverterPrimeInfo, AudioConverterRef, AudioConverterSetProperty, kAudioConverterEncodeBitRate,
    kAudioConverterPrimeInfo, kAudioConverterPropertyMaximumOutputPacketSize,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, kAudioFormatMPEG4AAC, kAudioFormatOpus,
};

use crate::Error;
use crate::format::{AudioCodec, AudioFormat, Packet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    pub codec: AudioCodec,
    /// 48 000 for both codecs in Lightair (Opus only takes 48 kHz here).
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u32,
    /// AAC: 96–320; Opus: 32–256.
    pub bitrate_kbps: u32,
}

impl AudioSettings {
    pub fn aac(kbps: u32) -> AudioSettings {
        AudioSettings { codec: AudioCodec::Aac, sample_rate: 48_000, channels: 2, bitrate_kbps: kbps }
    }

    pub fn opus(kbps: u32) -> AudioSettings {
        AudioSettings { codec: AudioCodec::Opus, sample_rate: 48_000, channels: 2, bitrate_kbps: kbps }
    }
}

/// One encoded packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudio {
    /// Raw AAC (no ADTS header) or an Opus packet.
    pub data: Vec<u8>,
    /// The program time of the packet's first input frame.
    pub pts_ns: i64,
    pub frames: u32,
}

impl EncodedAudio {
    pub fn packet(&self) -> Packet<'_> {
        Packet::audio(&self.data, self.pts_ns)
    }
}

/// What the input callback hands the converter: one packet's frames, once.
struct Input {
    data: *const f32,
    frames: u32,
    channels: u32,
    given: bool,
    /// No more input ever: the converter drains what it holds.
    end: bool,
}

/// The input callback's "nothing more for now" (any non-zero status the converter passes back).
const NO_DATA_NOW: i32 = i32::from_be_bytes(*b"wait");

unsafe extern "C-unwind" fn supply(
    _converter: AudioConverterRef,
    packets: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let input = unsafe { &mut *(user as *mut Input) };
    if input.given || input.frames == 0 {
        unsafe { *packets.as_ptr() = 0 };
        return if input.end { 0 } else { NO_DATA_NOW };
    }
    unsafe {
        let list = data.as_ptr();
        (*list).mNumberBuffers = 1;
        (*list).mBuffers[0] = AudioBuffer {
            mNumberChannels: input.channels,
            mDataByteSize: input.frames * input.channels * 4,
            mData: input.data as *mut c_void,
        };
        *packets.as_ptr() = input.frames;
    }
    input.given = true;
    0
}

pub struct AudioEncoder {
    converter: AudioConverterRef,
    format: AudioFormat,
    /// Interleaved frames waiting for a whole packet.
    pending: Vec<f32>,
    /// The program time of the first frame ever given, and packets made since.
    start: Option<i64>,
    made: i64,
    out: Vec<u8>,
    max_packet: u32,
    ended: bool,
}

// The converter is used by one thread at a time (the encoder's owner).
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    pub fn new(settings: AudioSettings) -> Result<AudioEncoder, Error> {
        let channels = settings.channels.clamp(1, 2);
        if settings.codec == AudioCodec::Opus && settings.sample_rate != 48_000 {
            return Err(Error::Unsupported("Opus is encoded at 48 kHz only".into()));
        }
        let input = AudioStreamBasicDescription {
            mSampleRate: settings.sample_rate as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 4 * channels,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * channels,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let output = AudioStreamBasicDescription {
            mSampleRate: settings.sample_rate as f64,
            mFormatID: match settings.codec {
                AudioCodec::Aac => kAudioFormatMPEG4AAC,
                AudioCodec::Opus => kAudioFormatOpus,
            },
            // AAC-LC's object type
            mFormatFlags: if settings.codec == AudioCodec::Aac { 2 } else { 0 },
            mBytesPerPacket: 0,
            mFramesPerPacket: settings.codec.frames_per_packet(),
            mBytesPerFrame: 0,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut converter: AudioConverterRef = ptr::null_mut();
        let status =
            unsafe { AudioConverterNew(NonNull::from(&input), NonNull::from(&output), NonNull::from(&mut converter)) };
        if status != 0 || converter.is_null() {
            return Err(Error::Unsupported(format!(
                "this Mac can't encode {} at {} Hz (status {status})",
                settings.codec.name(),
                settings.sample_rate
            )));
        }
        let mut encoder = AudioEncoder {
            converter,
            format: AudioFormat::new(settings.codec, settings.sample_rate, channels, settings.bitrate_kbps, 0),
            pending: Vec::new(),
            start: None,
            made: 0,
            out: Vec::new(),
            max_packet: 0,
            ended: false,
        };
        let bps: u32 = settings.bitrate_kbps * 1000;
        let status = unsafe {
            AudioConverterSetProperty(converter, kAudioConverterEncodeBitRate, 4, NonNull::from(&bps).cast())
        };
        if status != 0 {
            tracing::warn!(status, kbps = settings.bitrate_kbps, "the sound encoder kept its own bitrate");
        }
        let mut max_packet: u32 = 0;
        let mut size = 4u32;
        unsafe {
            AudioConverterGetProperty(
                converter,
                kAudioConverterPropertyMaximumOutputPacketSize,
                NonNull::from(&mut size),
                NonNull::from(&mut max_packet).cast(),
            )
        };
        encoder.max_packet = max_packet.max(1536 * channels);
        encoder.out = vec![0; encoder.max_packet as usize * 4];
        let mut prime = AudioConverterPrimeInfo { leadingFrames: 0, trailingFrames: 0 };
        let mut size = std::mem::size_of::<AudioConverterPrimeInfo>() as u32;
        let status = unsafe {
            AudioConverterGetProperty(
                converter,
                kAudioConverterPrimeInfo,
                NonNull::from(&mut size),
                NonNull::from(&mut prime).cast(),
            )
        };
        let priming = match (status, prime.leadingFrames, settings.codec) {
            (0, n, _) if n > 0 => n,
            // the usual encoder delays when the converter doesn't say
            (_, _, AudioCodec::Aac) => 2112,
            (_, _, AudioCodec::Opus) => 312,
        };
        encoder.format =
            AudioFormat::new(settings.codec, settings.sample_rate, channels, settings.bitrate_kbps, priming);
        Ok(encoder)
    }

    /// The stream's format: the codec configuration a file or stream needs, and the priming.
    pub fn format(&self) -> &AudioFormat {
        &self.format
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
        let channels = self.format.channels as usize;
        let per = self.format.frames_per_packet() as usize * channels;
        let mut out = Vec::new();
        let mut used = 0;
        while self.pending.len() - used >= per {
            let frames = self.pending[used..used + per].to_vec();
            used += per;
            self.convert(Some(&frames), &mut out)?;
        }
        self.pending.drain(..used);
        Ok(out)
    }

    /// The last partial packet (padded with silence) and whatever the encoder still holds;
    /// the encoder takes no more after.
    pub fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        if self.ended {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let per = self.format.frames_per_packet() as usize * self.format.channels as usize;
            let mut frames = std::mem::take(&mut self.pending);
            frames.resize(per, 0.0);
            self.convert(Some(&frames), &mut out)?;
        }
        self.ended = true;
        // drain: no more input, until the encoder has nothing left
        for _ in 0..8 {
            let before = out.len();
            self.convert(None, &mut out)?;
            if out.len() == before {
                break;
            }
        }
        Ok(out)
    }

    /// One packet's frames in (or none: the end), whatever packets the encoder makes out.
    fn convert(&mut self, frames: Option<&[f32]>, out: &mut Vec<EncodedAudio>) -> Result<(), Error> {
        let channels = self.format.channels;
        let mut input = Input {
            data: frames.map_or(ptr::null(), <[f32]>::as_ptr),
            frames: frames.map_or(0, |f| f.len() as u32 / channels),
            channels,
            given: false,
            end: frames.is_none(),
        };
        const MAX: usize = 4;
        let mut descriptions =
            [AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: 0 }; MAX];
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: self.out.len() as u32,
                mData: self.out.as_mut_ptr().cast(),
            }],
        };
        let mut packets = MAX as u32;
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                Some(supply),
                (&mut input as *mut Input).cast(),
                NonNull::from(&mut packets),
                NonNull::from(&mut list),
                descriptions.as_mut_ptr(),
            )
        };
        if status != 0 && status != NO_DATA_NOW {
            return Err(Error::Platform { what: "the sound encoder failed", status });
        }
        let start = self.start.unwrap_or(0);
        let rate = self.format.sample_rate.max(1) as i64;
        let fpp = self.format.frames_per_packet();
        for d in &descriptions[..(packets as usize).min(MAX)] {
            let at = d.mStartOffset.max(0) as usize;
            let Some(bytes) = self.out.get(at..at + d.mDataByteSize as usize) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            // packet k starts at input frame k × frames-per-packet (its sound is delayed by
            // the priming, which files trim)
            let pts_ns = start + self.made * fpp as i64 * 1_000_000_000 / rate;
            self.made += 1;
            out.push(EncodedAudio { data: bytes.to_vec(), pts_ns, frames: fpp });
        }
        Ok(())
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}
