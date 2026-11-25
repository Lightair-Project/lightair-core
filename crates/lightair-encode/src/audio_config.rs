//! Sound codecs' configuration as files and streams carry it: AAC's AudioSpecificConfig and its
//! MPEG-4 elementary stream descriptor, and Opus's identification header.

const AAC_RATES: [u32; 13] =
    [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];

/// The AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1) for AAC-LC at `sample_rate` with
/// `channels` (1–7; 8 channels is configuration 7).
pub fn aac_specific_config(sample_rate: u32, channels: u32) -> Vec<u8> {
    let object = 2u32; // AAC-LC
    let channel_config = match channels {
        8 => 7,
        n => n.min(7),
    };
    match AAC_RATES.iter().position(|&r| r == sample_rate) {
        Some(index) => {
            let v = (object << 11) | ((index as u32) << 7) | (channel_config << 3);
            vec![(v >> 8) as u8, v as u8]
        }
        None => {
            // an explicit rate: 5 + 4 + 24 + 4 + 3 bits
            let v: u64 = ((object as u64) << 35)
                | (0xF << 31)
                | ((sample_rate as u64 & 0xFF_FFFF) << 7)
                | ((channel_config as u64) << 3);
            (0..5).rev().map(|i| (v >> (i * 8)) as u8).collect()
        }
    }
}

/// The ADTS header (7 bytes, no CRC) for one AAC-LC packet of `payload` bytes, for outputs that
/// send raw AAC frames.
pub fn adts_header(sample_rate: u32, channels: u32, payload: usize) -> [u8; 7] {
    let index = AAC_RATES.iter().position(|&r| r == sample_rate).unwrap_or(3) as u8;
    let ch = channels.min(7) as u8;
    let len = payload + 7;
    [
        0xFF,
        0xF1,
        (1 << 6) | (index << 2) | (ch >> 2),
        ((ch & 3) << 6) | ((len >> 11) as u8 & 3),
        (len >> 3) as u8,
        (((len & 7) as u8) << 5) | 0x1F,
        0xFC,
    ]
}

/// Opus's identification header (`OpusHead`, RFC 7845 §5.1): what Matroska's CodecPrivate
/// holds. Mapping family 0: mono or stereo.
pub fn opus_head(channels: u32, pre_skip: u16, input_rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    out.extend_from_slice(b"OpusHead");
    out.push(1);
    out.push(channels.clamp(1, 2) as u8);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&input_rate.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.push(0);
    out
}

/// MP4's `dOps` box contents (Opus in ISOBMFF §4.3.2): the same fields as `OpusHead`,
/// big-endian, without the magic.
pub fn opus_dops(head: &[u8]) -> Vec<u8> {
    let field = |at: usize, n: usize| head.get(at..at + n).unwrap_or(&[0; 4][..n]).to_vec();
    let mut out = vec![0, field(9, 1)[0]];
    let mut pre_skip = field(10, 2);
    pre_skip.reverse();
    out.extend_from_slice(&pre_skip);
    let mut rate = field(12, 4);
    rate.reverse();
    out.extend_from_slice(&rate);
    let mut gain = field(16, 2);
    gain.reverse();
    out.extend_from_slice(&gain);
    out.push(0);
    out
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    // the 4-byte size form every reader accepts
    let n = body.len() as u32;
    out.extend_from_slice(&[
        0x80 | ((n >> 21) & 0x7F) as u8,
        0x80 | ((n >> 14) & 0x7F) as u8,
        0x80 | ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]);
    out.extend_from_slice(body);
    out
}

/// The `esds` box's contents after its version and flags: an ES descriptor for an AAC stream
/// with `specific_config` (ISO/IEC 14496-1 §7.2.6.5).
pub fn es_descriptor(track_id: u16, specific_config: &[u8], bitrate: u32) -> Vec<u8> {
    let mut config = vec![0x40, 0x15, 0, 0x18, 0x00];
    config.extend_from_slice(&bitrate.to_be_bytes()); // max
    config.extend_from_slice(&bitrate.to_be_bytes()); // average
    config.extend_from_slice(&descriptor(0x05, specific_config));
    let mut es = Vec::new();
    es.extend_from_slice(&track_id.to_be_bytes());
    es.push(0);
    es.extend_from_slice(&descriptor(0x04, &config));
    es.extend_from_slice(&descriptor(0x06, &[0x02]));
    descriptor(0x03, &es)
}
