//! Normalised images (design §4.3): what `lightair-pack build` writes and the check expects.
//!
//! - **PNG** (still, without an ICC profile): decoded and encoded again with fixed settings,
//!   keeping the colour type, bit depth, palette, transparency and colour space. Lossless.
//! - **PNG** (animated, or with an ICC profile), **JPEG**, **WebP** and **GIF**: rewritten at the
//!   chunk, segment or block level, keeping only what draws the picture and its colour, dropping
//!   metadata (EXIF, XMP, text, comments, thumbnails), unknown parts and anything after the end.
//!
//! A file is normalised when normalising it gives the same bytes: [`is_normalised`].

use std::io::Cursor;

use crate::filetype::Family;

/// The largest picture decoded, in pixels per side.
const MAX_SIDE: u32 = 16_384;

/// `bytes` normalised, or why the file can't be (it's damaged, or not what it says).
pub fn normalise(family: Family, bytes: &[u8]) -> Result<Vec<u8>, String> {
    match family {
        Family::Png => png(bytes),
        Family::Jpeg => jpeg(bytes),
        Family::Webp => webp(bytes),
        Family::Gif => gif(bytes),
        _ => Ok(bytes.to_vec()),
    }
}

/// Whether `bytes` are already normalised.
pub fn is_normalised(family: Family, bytes: &[u8]) -> Result<bool, String> {
    Ok(normalise(family, bytes)? == bytes)
}

/// Decodes the picture (its first frame) under size limits, to prove it's a whole, readable
/// image.
pub fn decode_check(family: Family, bytes: &[u8]) -> Result<(u32, u32), String> {
    let format = match family {
        Family::Png => image::ImageFormat::Png,
        Family::Jpeg => image::ImageFormat::Jpeg,
        Family::Gif => image::ImageFormat::Gif,
        Family::Webp => image::ImageFormat::WebP,
        _ => return Err("isn't an image".into()),
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    Ok((img.width(), img.height()))
}

// --- PNG ---------------------------------------------------------------------------------------

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Chunks that draw the picture or say its colour, animation included.
const PNG_KEEP: &[&[u8; 4]] = &[
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"acTL", b"fcTL", b"fdAT",
];

fn png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let chunks = png_chunks(bytes)?;
    let has = |t: &[u8; 4]| chunks.iter().any(|(ty, _)| ty == t);
    if has(b"acTL") || has(b"iCCP") {
        let mut out = PNG_SIG.to_vec();
        for (ty, whole) in &chunks {
            if PNG_KEEP.contains(&ty) {
                out.extend_from_slice(whole);
            }
        }
        return Ok(out);
    }
    png_reencode(bytes)
}

/// Every chunk: its type and its whole bytes (length, type, data, CRC), CRCs checked.
/// A chunk: its type and its whole bytes.
type Chunk<'a> = ([u8; 4], &'a [u8]);

fn png_chunks(bytes: &[u8]) -> Result<Vec<Chunk<'_>>, String> {
    if !bytes.starts_with(PNG_SIG) {
        return Err("isn't a PNG".into());
    }
    let mut pos = PNG_SIG.len();
    let mut out = Vec::new();
    loop {
        if pos + 12 > bytes.len() {
            return Err("ends before its last chunk".into());
        }
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let end = pos.checked_add(12 + len).filter(|e| *e <= bytes.len()).ok_or("has a chunk longer than the file")?;
        let ty: [u8; 4] = bytes[pos + 4..pos + 8].try_into().unwrap();
        let crc = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
        if crc32(&bytes[pos + 4..end - 4]) != crc {
            return Err(format!("has a damaged {} chunk", String::from_utf8_lossy(&ty)));
        }
        out.push((ty, &bytes[pos..end]));
        pos = end;
        if &ty == b"IEND" {
            return Ok(out);
        }
    }
}

fn png_reencode(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (w, h, color, depth, palette, trns, gamma, chroma, srgb) = {
        let info = reader.info();
        if info.width > MAX_SIDE || info.height > MAX_SIDE {
            return Err(format!("is larger than {MAX_SIDE} pixels a side"));
        }
        (
            info.width,
            info.height,
            info.color_type,
            info.bit_depth,
            info.palette.as_ref().map(|p| p.to_vec()),
            info.trns.as_ref().map(|t| t.to_vec()),
            info.source_gamma,
            info.source_chromaticities,
            info.srgb,
        )
    };
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(frame.buffer_size());

    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        enc.set_compression(png::Compression::Default);
        enc.set_filter(png::FilterType::Paeth);
        enc.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
        if let Some(p) = palette {
            enc.set_palette(p);
        }
        if let Some(t) = trns {
            enc.set_trns(t);
        }
        if let Some(s) = srgb {
            enc.set_source_srgb(s);
        } else {
            if let Some(g) = gamma {
                enc.set_source_gamma(g);
            }
            if let Some(c) = chroma {
                enc.set_source_chromaticities(c);
            }
        }
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&buf).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

// --- JPEG --------------------------------------------------------------------------------------

fn jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return Err("isn't a JPEG".into());
    }
    let mut out = vec![0xFF, 0xD8];
    let mut pos = 2;
    loop {
        // the next marker (fill bytes skipped)
        while pos < bytes.len() && bytes[pos] == 0xFF && bytes.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        if pos + 1 >= bytes.len() || bytes[pos] != 0xFF {
            return Err("ends before its end marker".into());
        }
        let marker = bytes[pos + 1];
        pos += 2;
        match marker {
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Ok(out);
            }
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&[0xFF, marker]);
                continue;
            }
            _ => {}
        }
        if pos + 2 > bytes.len() {
            return Err("ends inside a segment".into());
        }
        let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        let end = pos.checked_add(len).filter(|e| *e <= bytes.len() && len >= 2).ok_or("has a damaged segment")?;
        let seg = &bytes[pos..end];
        let data = &seg[2..];
        let keep = match marker {
            0xE0 => data.starts_with(b"JFIF\0"),
            0xE2 => data.starts_with(b"ICC_PROFILE\0"),
            0xEE => data.starts_with(b"Adobe"),
            0xE1 | 0xE3..=0xED | 0xEF | 0xFE => false,
            _ => true,
        };
        if keep {
            out.extend_from_slice(&[0xFF, marker]);
            out.extend_from_slice(seg);
        }
        pos = end;
        if marker == 0xDA {
            // entropy-coded data up to the next marker that isn't stuffing or a restart
            let start = pos;
            while pos + 1 < bytes.len() {
                if bytes[pos] == 0xFF {
                    let next = bytes[pos + 1];
                    if next != 0x00 && !(0xD0..=0xD7).contains(&next) && next != 0xFF {
                        break;
                    }
                }
                pos += 1;
            }
            out.extend_from_slice(&bytes[start..pos]);
        }
    }
}

// --- WebP --------------------------------------------------------------------------------------

fn webp(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err("isn't a WebP".into());
    }
    let riff_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let end = (8 + riff_len).min(bytes.len());
    if 8 + riff_len > bytes.len() {
        return Err("is shorter than its header says".into());
    }
    let mut body = Vec::new();
    let mut pos = 12;
    while pos + 8 <= end {
        let ty: [u8; 4] = bytes[pos..pos + 4].try_into().unwrap();
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let data_end = pos.checked_add(8 + len).filter(|e| *e <= end).ok_or("has a chunk longer than the file")?;
        let padded = (data_end + (len & 1)).min(end);
        let keep = matches!(&ty, b"VP8 " | b"VP8L" | b"VP8X" | b"ANIM" | b"ANMF" | b"ALPH" | b"ICCP");
        if keep {
            let mut chunk = bytes[pos..data_end].to_vec();
            if &ty == b"VP8X" && chunk.len() > 8 {
                chunk[8] &= !(0x08 | 0x04); // no EXIF, no XMP
            }
            if len & 1 == 1 {
                chunk.push(0);
            }
            body.extend_from_slice(&chunk);
        }
        pos = padded;
    }
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Ok(out)
}

// --- GIF ---------------------------------------------------------------------------------------

fn gif(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 13 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Err("isn't a GIF".into());
    }
    let mut pos = 13;
    let flags = bytes[10];
    if flags & 0x80 != 0 {
        pos += 3 * (1 << ((flags & 0x07) + 1));
    }
    if pos > bytes.len() {
        return Err("ends inside its colour table".into());
    }
    let mut out = bytes[..pos].to_vec();
    loop {
        let Some(&block) = bytes.get(pos) else { return Err("ends before its trailer".into()) };
        match block {
            0x3B => {
                out.push(0x3B);
                return Ok(out);
            }
            0x21 => {
                let label = *bytes.get(pos + 1).ok_or("ends inside an extension")?;
                let start = pos;
                pos = sub_blocks_end(bytes, pos + 2)?;
                let keep = match label {
                    0xF9 => true,
                    0xFF => {
                        let id = bytes.get(start + 3..start + 14).unwrap_or(&[]);
                        id == b"NETSCAPE2.0" || id == b"ANIMEXTS1.0"
                    }
                    _ => false,
                };
                if keep {
                    out.extend_from_slice(&bytes[start..pos]);
                }
            }
            0x2C => {
                let start = pos;
                let desc_end = pos + 10;
                let f = *bytes.get(pos + 9).ok_or("ends inside a frame")?;
                let mut p = desc_end;
                if f & 0x80 != 0 {
                    p += 3 * (1 << ((f & 0x07) + 1));
                }
                p += 1; // LZW minimum code size
                pos = sub_blocks_end(bytes, p)?;
                out.extend_from_slice(&bytes[start..pos]);
            }
            _ => return Err("has a block Lightair doesn't know".into()),
        }
    }
}

/// The position after a chain of sub-blocks starting at `pos`.
fn sub_blocks_end(bytes: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        let len = *bytes.get(pos).ok_or("ends inside a block")? as usize;
        pos += 1;
        if len == 0 {
            return Ok(pos);
        }
        pos += len;
        if pos > bytes.len() {
            return Err("ends inside a block".into());
        }
    }
}
