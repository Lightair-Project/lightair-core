//! `.lairava` files (docs/pngtuber-format.md): a ZIP holding `avatar.json` and its layers' PNGs.
//! Read whole into memory (nothing is unpacked to disk, so no path in the file can reach outside
//! it); pictures decoded to straight-alpha RGBA.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, Write};

use crate::model::{Avatar, FORMAT, VERSION};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the file couldn't be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("it isn't a .lairava (not a ZIP: {0})")]
    Zip(#[from] zip::result::ZipError),
    #[error("its avatar.json couldn't be read: {0}")]
    Json(#[from] serde_json::Error),
    #[error("it isn't a Lightair PNGtuber (format \"{0}\")")]
    Format(String),
    #[error("it's from a newer Lightair (version {0}): update Lightair to open it")]
    Version(u32),
    #[error("a layer's picture is missing: {0}")]
    Missing(String),
    #[error("a layer's picture couldn't be read ({0}): {1}")]
    Picture(String, String),
}

/// The most pixels a picture may have on either side.
pub const MAX_SIDE: u32 = 8192;
/// The most a file may hold, unpacked (a guard against a ZIP that unpacks to far more).
const MAX_TOTAL: u64 = 1 << 30;

/// A picture: straight-alpha RGBA, row by row.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A PNGtuber read: its description and each layer's picture (in `avatar.layers`' order).
#[derive(Clone, Debug)]
pub struct Loaded {
    pub avatar: Avatar,
    pub pictures: Vec<Picture>,
}

/// Decodes a PNG to straight-alpha RGBA.
pub fn decode_png(bytes: &[u8]) -> Result<Picture, String> {
    let mut d = png::Decoder::new(Cursor::new(bytes));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA);
    let mut r = d.read_info().map_err(|e| e.to_string())?;
    let (w, h) = (r.info().width, r.info().height);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("{w}×{h} is too big (at most {MAX_SIDE} on a side)"));
    }
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("an indexed picture that didn't expand".into()),
    };
    Ok(Picture { width: w, height: h, rgba })
}

/// Encodes straight-alpha RGBA as a PNG.
pub fn encode_png(p: &Picture) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, p.width, p.height);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&p.rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Reads a `.lairava`.
pub fn read(mut file: impl Read + Seek) -> Result<Loaded, Error> {
    let mut zip = zip::ZipArchive::new(&mut file)?;
    let mut entries: HashMap<String, Vec<u8>> = HashMap::new();
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        total += f.size();
        if total > MAX_TOTAL {
            return Err(Error::Picture("the file".into(), "it unpacks to more than 1 GB".into()));
        }
        let mut bytes = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut bytes)?;
        let name = f.name()?.to_string();
        entries.insert(name, bytes);
    }
    let json = entries.get("avatar.json").ok_or_else(|| Error::Missing("avatar.json".into()))?;
    let avatar: Avatar = serde_json::from_slice(json)?;
    if avatar.format != FORMAT {
        return Err(Error::Format(avatar.format));
    }
    if avatar.version > VERSION {
        return Err(Error::Version(avatar.version));
    }
    let pictures = avatar
        .layers
        .iter()
        .map(|l| {
            let bytes = entries.get(&l.image).ok_or_else(|| Error::Missing(l.image.clone()))?;
            decode_png(bytes).map_err(|e| Error::Picture(l.image.clone(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Loaded { avatar, pictures })
}

/// Reads a `.lairava` from disk.
pub fn read_path(path: &std::path::Path) -> Result<Loaded, Error> {
    read(std::fs::File::open(path)?)
}

/// Writes a `.lairava`: `avatar.json` and each layer's picture (PNG bytes, by the layer's
/// `image` path), stored (PNGs are compressed already).
pub fn write(out: impl Write + Seek, avatar: &Avatar, pngs: &HashMap<String, Vec<u8>>) -> Result<(), Error> {
    let mut zip = zip::ZipWriter::new(out);
    let json_options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let png_options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut a = avatar.clone();
    a.format = FORMAT.into();
    a.version = VERSION;
    zip.start_file("avatar.json", json_options)?;
    zip.write_all(&serde_json::to_vec_pretty(&a)?)?;
    let mut written = std::collections::HashSet::new();
    for l in &a.layers {
        if !written.insert(l.image.clone()) {
            continue;
        }
        let bytes = pngs.get(&l.image).ok_or_else(|| Error::Missing(l.image.clone()))?;
        zip.start_file(&l.image, png_options)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(())
}

/// Writes a `.lairava` to disk (through a temporary file beside it, so a failed write leaves the
/// old one).
pub fn write_path(path: &std::path::Path, avatar: &Avatar, pngs: &HashMap<String, Vec<u8>>) -> Result<(), Error> {
    let tmp = path.with_extension("lairava.tmp");
    write(std::fs::File::create(&tmp)?, avatar, pngs)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
