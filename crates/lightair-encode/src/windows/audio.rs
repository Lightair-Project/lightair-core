//! Sound on Windows: AAC-LC through Media Foundation's AAC encoder transform (part of Windows),
//! Opus through the crate's software encoder (Windows has none).
//!
//! The AAC transform takes 16-bit PCM at 44.1 or 48 kHz and makes raw AAC frames (no ADTS) of
//! 1024 frames each, at 96, 128, 160 or 192 kbit/s only: other bitrates are taken to the
//! nearest, and the format says which. Packet times are counted as on Apple platforms: the
//! first input frame's program time plus 1024 frames a packet.

use std::mem::ManuallyDrop;
use windows::Win32::Media::MediaFoundation::{
    IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType, MFMediaType_Audio,
    MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO,
};

use super::mf::{self, Runtime, platform};
use crate::Error;
use crate::encoder::{AudioEncode, AudioSettings, EncodedAudio};
use crate::format::{AudioCodec, AudioFormat};
use crate::software::OpusEncoder;

/// The bitrates the AAC transform takes, in bytes a second.
const AAC_RATES: [u32; 4] = [12_000, 16_000, 20_000, 24_000];

/// AAC-LC's usual encoder delay; the transform doesn't report its own.
const AAC_PRIMING: u32 = 2112;

/// A sound encoder on Windows: AAC through Media Foundation, or Opus in software.
pub enum AudioEncoder {
    Aac(AacEncoder),
    Opus(OpusEncoder),
}

impl AudioEncoder {
    pub fn new(settings: AudioSettings) -> Result<AudioEncoder, Error> {
        match settings.codec {
            AudioCodec::Aac => AacEncoder::new(settings).map(AudioEncoder::Aac),
            AudioCodec::Opus => OpusEncoder::new(settings).map(AudioEncoder::Opus),
        }
    }

    /// The stream's format: the codec configuration a file or stream needs, and the priming.
    pub fn format(&self) -> &AudioFormat {
        match self {
            AudioEncoder::Aac(e) => e.format(),
            AudioEncoder::Opus(e) => e.format(),
        }
    }

    /// Encodes `interleaved` frames starting at program time `pts_ns`; returns the packets
    /// they completed.
    pub fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error> {
        match self {
            AudioEncoder::Aac(e) => e.encode(interleaved, pts_ns),
            AudioEncoder::Opus(e) => e.encode(interleaved, pts_ns),
        }
    }

    /// The last partial packet (padded with silence) and whatever the encoder still holds;
    /// the encoder takes no more after.
    pub fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        match self {
            AudioEncoder::Aac(e) => e.flush(),
            AudioEncoder::Opus(e) => e.flush(),
        }
    }
}

impl AudioEncode for AudioEncoder {
    fn format(&self) -> &AudioFormat {
        AudioEncoder::format(self)
    }

    fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error> {
        AudioEncoder::encode(self, interleaved, pts_ns)
    }

    fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        AudioEncoder::flush(self)
    }
}

/// AAC-LC through Media Foundation's AAC encoder transform.
pub struct AacEncoder {
    transform: IMFTransform,
    input_id: u32,
    output_id: u32,
    output_size: u32,
    provides_samples: bool,
    format: AudioFormat,
    /// Interleaved frames waiting for a whole packet.
    pending: Vec<f32>,
    /// Samples converted for the transform.
    pcm: Vec<u8>,
    /// The program time of the first frame ever given, frames given since, and packets made.
    start: Option<i64>,
    given: i64,
    made: i64,
    ended: bool,
    _runtime: Runtime,
}

// The transform is used by one thread at a time (the encoder's owner); Media Foundation's
// objects work from any thread.
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    pub fn new(settings: AudioSettings) -> Result<AacEncoder, Error> {
        if !matches!(settings.sample_rate, 44_100 | 48_000) {
            return Err(Error::Unsupported("Windows' AAC encoder takes 44.1 or 48 kHz only".into()));
        }
        let runtime = Runtime::start()?;
        let channels = settings.channels.clamp(1, 2);
        let rate = settings.sample_rate;
        let wanted = settings.bitrate_kbps.saturating_mul(1000) / 8;
        let bytes_per_second = *AAC_RATES.iter().min_by_key(|&&r| r.abs_diff(wanted)).unwrap_or(&AAC_RATES[2]);
        let input_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
        let output_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
        let found = mf::enumerate(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(input_info),
            Some(output_info),
        )?;
        let Some((activate, _)) = found.into_iter().next() else {
            return Err(Error::Unsupported(
                "this PC has no AAC encoder (a Windows N edition without the Media Feature Pack?)".into(),
            ));
        };
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(platform("Windows' AAC encoder wouldn't start"))?;
        let (input_id, output_id) = mf::stream_ids(&transform);
        let what = "Windows' AAC encoder didn't take the format";
        unsafe {
            let output = MFCreateMediaType().map_err(platform(what))?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(platform(what))?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(platform(what))?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(platform(what))?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(platform(what))?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels).map_err(platform(what))?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second).map_err(platform(what))?;
            // raw frames, AAC-LC (profile and level 0x29: AAC level 2)
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(platform(what))?;
            let _ = output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29);
            transform.SetOutputType(output_id, &output, 0).map_err(platform(what))?;

            let input = MFCreateMediaType().map_err(platform(what))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(platform(what))?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(platform(what))?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(platform(what))?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(platform(what))?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels).map_err(platform(what))?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2).map_err(platform(what))?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2).map_err(platform(what))?;
            transform.SetInputType(input_id, &input, 0).map_err(platform(what))?;

            let start = "Windows' AAC encoder wouldn't start streaming";
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(platform(start))?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(platform(start))?;
        }
        let (output_size, provides_samples) = match unsafe { transform.GetOutputStreamInfo(output_id) } {
            Ok(info) => (info.cbSize, info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0),
            Err(_) => (0, false),
        };
        let kbps = bytes_per_second * 8 / 1000;
        Ok(AacEncoder {
            transform,
            input_id,
            output_id,
            output_size: output_size.max(768 * channels),
            provides_samples,
            format: AudioFormat::new(AudioCodec::Aac, rate, channels, kbps, AAC_PRIMING),
            pending: Vec::new(),
            pcm: Vec::new(),
            start: None,
            given: 0,
            made: 0,
            ended: false,
            _runtime: runtime,
        })
    }

    /// The stream's format: AudioSpecificConfig, the bitrate the transform took, the priming.
    pub fn format(&self) -> &AudioFormat {
        &self.format
    }

    /// Encodes `interleaved` frames starting at program time `pts_ns`; returns the packets
    /// they completed. The sound is taken as continuous: times after the first only matter for
    /// the first.
    pub fn encode(&mut self, interleaved: &[f32], pts_ns: i64) -> Result<Vec<EncodedAudio>, Error> {
        if self.ended {
            return Err(Error::Mux("the sound encoder has been flushed".into()));
        }
        self.start.get_or_insert(pts_ns);
        self.pending.extend_from_slice(interleaved);
        let per = self.per_packet();
        let whole = self.pending.len() / per * per;
        let mut out = Vec::new();
        if whole > 0 {
            let frames: Vec<f32> = self.pending.drain(..whole).collect();
            self.give(&frames, &mut out)?;
        }
        Ok(out)
    }

    /// The last partial packet (padded with silence) and whatever the transform still holds;
    /// the encoder takes no more after.
    pub fn flush(&mut self) -> Result<Vec<EncodedAudio>, Error> {
        if self.ended {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let mut frames = std::mem::take(&mut self.pending);
            frames.resize(self.per_packet(), 0.0);
            self.give(&frames, &mut out)?;
        }
        self.ended = true;
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        self.take(&mut out)?;
        Ok(out)
    }

    fn per_packet(&self) -> usize {
        self.format.frames_per_packet() as usize * self.format.channels as usize
    }

    /// Gives whole packets' frames to the transform and takes what it makes.
    fn give(&mut self, frames: &[f32], out: &mut Vec<EncodedAudio>) -> Result<(), Error> {
        self.pcm.clear();
        self.pcm.reserve(frames.len() * 2);
        for &s in frames {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            self.pcm.extend_from_slice(&v.to_le_bytes());
        }
        let channels = self.format.channels as i64;
        let rate = self.format.sample_rate as i64;
        let count = frames.len() as i64 / channels;
        let time = self.given * 10_000_000 / rate;
        let duration = count * 10_000_000 / rate;
        let sample = mf::memory_sample(&self.pcm, time, duration)?;
        self.given += count;
        loop {
            match unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) } {
                Ok(()) => break,
                // full: take its output, then give again
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    if self.take(out)? == 0 {
                        return Err(Error::Platform { what: "Windows' AAC encoder took nothing", status: e.code().0 });
                    }
                }
                Err(e) => return Err(platform("Windows' AAC encoder refused sound")(e)),
            }
        }
        self.take(out)?;
        Ok(())
    }

    /// Every packet the transform has ready; returns how many.
    fn take(&mut self, out: &mut Vec<EncodedAudio>) -> Result<usize, Error> {
        let before = out.len();
        loop {
            let given = if self.provides_samples { None } else { Some(mf::output_sample(self.output_size)?) };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(given),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => continue,
                Err(e) => return Err(platform("Windows' AAC encoder failed")(e)),
            }
            let Some(sample) = sample else { break };
            let bytes = mf::sample_bytes(&sample)?;
            if bytes.is_empty() {
                continue;
            }
            let fpp = self.format.frames_per_packet();
            let start = self.start.unwrap_or(0);
            let pts_ns = start + self.made * fpp as i64 * 1_000_000_000 / self.format.sample_rate.max(1) as i64;
            self.made += 1;
            out.push(EncodedAudio { data: bytes, pts_ns, frames: fpp });
        }
        Ok(out.len() - before)
    }
}
