//! H.264 and HEVC NAL units: splitting either layout, converting between them, and the
//! decoder configuration records (`avcC`, `hvcC`) built from parameter sets.

use crate::Error;

/// How a packet's bytes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NalLayout {
    /// NAL units each after a 4-byte big-endian length (VideoToolbox, MP4, Matroska).
    LengthPrefixed,
    /// NAL units each after a `00 00 01` or `00 00 00 01` start code (MPEG-TS, MediaCodec).
    AnnexB,
    /// Not NAL units: ProRes pictures and sound.
    Raw,
}

/// The NAL units of `data` (without their lengths or start codes).
pub fn split(data: &[u8], layout: NalLayout) -> Vec<&[u8]> {
    match layout {
        NalLayout::LengthPrefixed => split_length_prefixed(data),
        NalLayout::AnnexB => split_annex_b(data),
        NalLayout::Raw => vec![data],
    }
}

fn split_length_prefixed(mut data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let n = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        data = &data[4..];
        if n > data.len() {
            break;
        }
        if n > 0 {
            out.push(&data[..n]);
        }
        data = &data[n..];
    }
    out
}

fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    // the offsets just past each start code
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        // a 4-byte start code's leading zero, and trailing zeros, belong to no NAL unit
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

/// Whether a NAL unit is a parameter set or an access unit delimiter: the units a file keeps
/// in its decoder configuration record rather than in its samples.
pub fn is_out_of_band(unit: &[u8], hevc: bool) -> bool {
    let Some(&b) = unit.first() else { return true };
    if hevc {
        // VPS, SPS, PPS, AUD
        matches!((b >> 1) & 0x3F, 32..=35)
    } else {
        // SPS, PPS, AUD
        matches!(b & 0x1F, 7..=9)
    }
}

/// A picture as MP4 and Matroska store it: 4-byte lengths, without parameter sets or access
/// unit delimiters.
pub fn to_sample(data: &[u8], layout: NalLayout, hevc: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    append_sample(&mut out, data, layout, hevc);
    out
}

/// [`to_sample`] appended to `out`; returns the bytes appended.
pub fn append_sample(out: &mut Vec<u8>, data: &[u8], layout: NalLayout, hevc: bool) -> usize {
    let before = out.len();
    if layout == NalLayout::Raw {
        out.extend_from_slice(data);
        return data.len();
    }
    for unit in split(data, layout) {
        if is_out_of_band(unit, hevc) {
            continue;
        }
        out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out.len() - before
}

/// A picture as Annex B, optionally with the parameter sets in front (a keyframe for a stream).
pub fn to_annex_b(data: &[u8], layout: NalLayout, parameter_sets: Option<&[Vec<u8>]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    for p in parameter_sets.unwrap_or(&[]) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(p);
    }
    for unit in split(data, layout) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    out
}

/// A NAL unit's payload without its emulation prevention bytes (`00 00 03` → `00 00`).
pub fn unescape(unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(unit.len());
    let mut zeros = 0;
    for &b in unit {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Reads bits most significant first, and Exp-Golomb codes.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, at: 0 }
    }

    fn bit(&mut self) -> Result<u32, Error> {
        let byte = *self.data.get(self.at / 8).ok_or(Error::Invalid("a parameter set ended early"))?;
        let b = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Ok(b as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u64, Error> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Ok(v)
    }

    fn skip(&mut self, n: usize) {
        self.at += n;
    }

    fn ue(&mut self) -> Result<u32, Error> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Invalid("a parameter set's Exp-Golomb code is too long"));
            }
        }
        Ok(((1u64 << zeros) - 1 + self.bits(zeros)?) as u32)
    }
}

/// What `avcC` needs from an H.264 SPS beyond its first bytes.
struct AvcSps {
    profile: u8,
    chroma_format: u32,
    bit_depth_luma_minus8: u32,
    bit_depth_chroma_minus8: u32,
}

fn parse_avc_sps(sps: &[u8]) -> Result<AvcSps, Error> {
    let rbsp = unescape(sps.get(1..).ok_or(Error::Invalid("an empty SPS"))?);
    let mut r = Bits::new(&rbsp);
    let profile = r.bits(8)? as u8;
    r.skip(16); // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    let (mut chroma_format, mut luma, mut chroma) = (1, 0, 0);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format = r.ue()?;
        if chroma_format == 3 {
            r.skip(1);
        }
        luma = r.ue()?;
        chroma = r.ue()?;
    }
    Ok(AvcSps { profile, chroma_format, bit_depth_luma_minus8: luma, bit_depth_chroma_minus8: chroma })
}

/// The `avcC` record (ISO/IEC 14496-15 §5.3.3) for H.264 parameter sets, with 4-byte lengths.
pub fn avc_config(parameter_sets: &[Vec<u8>]) -> Result<Vec<u8>, Error> {
    let sps: Vec<&Vec<u8>> = parameter_sets.iter().filter(|n| n.first().map(|b| b & 0x1F) == Some(7)).collect();
    let pps: Vec<&Vec<u8>> = parameter_sets.iter().filter(|n| n.first().map(|b| b & 0x1F) == Some(8)).collect();
    let first = sps.first().ok_or(Error::Invalid("no H.264 SPS"))?;
    if pps.is_empty() || first.len() < 4 {
        return Err(Error::Invalid("no H.264 PPS, or a short SPS"));
    }
    let parsed = parse_avc_sps(first)?;
    let mut out = vec![1, first[1], first[2], first[3], 0xFF, 0xE0 | sps.len() as u8];
    for s in &sps {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s);
    }
    out.push(pps.len() as u8);
    for p in &pps {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    if !matches!(parsed.profile, 66 | 77 | 88) {
        out.push(0xFC | (parsed.chroma_format as u8 & 3));
        out.push(0xF8 | (parsed.bit_depth_luma_minus8 as u8 & 7));
        out.push(0xF8 | (parsed.bit_depth_chroma_minus8 as u8 & 7));
        out.push(0); // no SPS extensions
    }
    Ok(out)
}

/// What `hvcC` needs from an HEVC SPS.
struct HevcSps {
    /// profile space, tier, profile; compatibility flags; constraint flags; level: the
    /// general profile_tier_level's first 12 bytes as they are.
    general: [u8; 12],
    max_sub_layers: u8,
    temporal_id_nested: bool,
    chroma_format: u32,
    bit_depth_luma_minus8: u32,
    bit_depth_chroma_minus8: u32,
}

fn parse_hevc_sps(sps: &[u8]) -> Result<HevcSps, Error> {
    let rbsp = unescape(sps.get(2..).ok_or(Error::Invalid("an empty SPS"))?);
    let mut r = Bits::new(&rbsp);
    r.skip(4); // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as u8;
    let temporal_id_nested = r.bit()? == 1;
    let mut general = [0u8; 12];
    for g in &mut general {
        *g = r.bits(8)? as u8;
    }
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.bit()? == 1;
        level_present[i] = r.bit()? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2);
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip(88);
        }
        if level_present[i] {
            r.skip(8);
        }
    }
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format = r.ue()?;
    if chroma_format == 3 {
        r.skip(1);
    }
    r.ue()?; // width
    r.ue()?; // height
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let luma = r.ue()?;
    let chroma = r.ue()?;
    Ok(HevcSps {
        general,
        max_sub_layers: max_sub_layers_minus1 + 1,
        temporal_id_nested,
        chroma_format,
        bit_depth_luma_minus8: luma,
        bit_depth_chroma_minus8: chroma,
    })
}

/// The `hvcC` record (ISO/IEC 14496-15 §8.3.3) for HEVC parameter sets, with 4-byte lengths.
pub fn hevc_config(parameter_sets: &[Vec<u8>]) -> Result<Vec<u8>, Error> {
    let kind = |n: &Vec<u8>| n.first().map(|b| (b >> 1) & 0x3F);
    let sps = parameter_sets.iter().find(|n| kind(n) == Some(33)).ok_or(Error::Invalid("no HEVC SPS"))?;
    let s = parse_hevc_sps(sps)?;
    let mut out = Vec::with_capacity(64);
    out.push(1);
    out.extend_from_slice(&s.general);
    out.extend_from_slice(&[0xF0, 0x00]); // min_spatial_segmentation_idc 0
    out.push(0xFC); // parallelismType unknown
    out.push(0xFC | (s.chroma_format as u8 & 3));
    out.push(0xF8 | (s.bit_depth_luma_minus8 as u8 & 7));
    out.push(0xF8 | (s.bit_depth_chroma_minus8 as u8 & 7));
    out.extend_from_slice(&[0, 0]); // avgFrameRate unknown
    out.push(((s.max_sub_layers & 7) << 3) | ((s.temporal_id_nested as u8) << 2) | 3);
    let arrays: Vec<(u8, Vec<&Vec<u8>>)> = [32u8, 33, 34, 39]
        .iter()
        .map(|&t| (t, parameter_sets.iter().filter(|n| kind(n) == Some(t)).collect()))
        .filter(|(_, v): &(u8, Vec<&Vec<u8>>)| !v.is_empty())
        .collect();
    out.push(arrays.len() as u8);
    for (t, units) in arrays {
        out.push(0x80 | t);
        out.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for u in units {
            out.extend_from_slice(&(u.len() as u16).to_be_bytes());
            out.extend_from_slice(u);
        }
    }
    Ok(out)
}

/// The parameter sets a keyframe carries in band (start codes or lengths), for an encoder
/// that sends them with the picture rather than separately.
pub fn parameter_sets_in(data: &[u8], layout: NalLayout, hevc: bool) -> Vec<Vec<u8>> {
    split(data, layout)
        .into_iter()
        .filter(|u| {
            let Some(&b) = u.first() else { return false };
            if hevc { matches!((b >> 1) & 0x3F, 32..=34) } else { matches!(b & 0x1F, 7 | 8) }
        })
        .map(<[u8]>::to_vec)
        .collect()
}
