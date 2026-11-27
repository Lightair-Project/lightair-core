//! What a file is, by its bytes, and which files each kind of content may hold.

use lightair_packages::Kind;

/// A family of file, as its first bytes say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// SVG, HTML, CSS, JavaScript, shaders, LUTs, Markdown: UTF-8 text.
    Text,
    Json,
    /// TrueType/OpenType (and collections).
    Font,
    Woff,
    Woff2,
    /// MP4, MOV, M4A (ISO base media).
    Iso,
    /// WebM, MKV.
    Matroska,
    Mp3,
    Wav,
    Ogg,
    Flac,
    Adts,
    /// glTF binary (`.glb`, `.vrm`).
    Glb,
    /// Live2D's `.moc3`.
    Moc3,
    /// Inochi2D's `.inp` / `.inx`.
    Inochi,
    /// A ZIP (a `.lairava` PNGtuber, or an archive inside the package).
    Zip,
    Wasm,
    MachO,
    Pe,
    Elf,
    /// A script with a `#!` line.
    Shebang,
    Unknown,
}

impl Family {
    /// Whether it's a program (or a library) the system can run.
    pub fn is_executable(self) -> bool {
        matches!(self, Family::MachO | Family::Pe | Family::Elf | Family::Shebang)
    }

    pub fn is_image(self) -> bool {
        matches!(self, Family::Png | Family::Jpeg | Family::Gif | Family::Webp)
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Family::Mp3 | Family::Wav | Family::Ogg | Family::Flac | Family::Adts | Family::Iso)
    }

    pub fn is_video(self) -> bool {
        matches!(self, Family::Iso | Family::Matroska)
    }

    pub fn name(self) -> &'static str {
        match self {
            Family::Png => "a PNG image",
            Family::Jpeg => "a JPEG image",
            Family::Gif => "a GIF image",
            Family::Webp => "a WebP image",
            Family::Text => "text",
            Family::Json => "JSON",
            Family::Font => "a font",
            Family::Woff | Family::Woff2 => "a web font",
            Family::Iso => "an MP4/MOV file",
            Family::Matroska => "a WebM/MKV file",
            Family::Mp3 => "an MP3 file",
            Family::Wav => "a WAV file",
            Family::Ogg => "an Ogg file",
            Family::Flac => "a FLAC file",
            Family::Adts => "an AAC file",
            Family::Glb => "a glTF model",
            Family::Moc3 => "a Live2D model",
            Family::Inochi => "an Inochi2D model",
            Family::Zip => "a ZIP archive",
            Family::Wasm => "WebAssembly",
            Family::MachO => "a macOS program or library",
            Family::Pe => "a Windows program or library",
            Family::Elf => "a Linux program or library",
            Family::Shebang => "a script",
            Family::Unknown => "a kind of file Lightair doesn't know",
        }
    }
}

/// What a file's first bytes say it is.
pub fn sniff(bytes: &[u8]) -> Family {
    let b = bytes;
    let starts = |m: &[u8]| b.starts_with(m);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Family::Png
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        Family::Jpeg
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Family::Gif
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Family::Webp
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE" {
        Family::Wav
    } else if starts(&[0x00, 0x01, 0x00, 0x00]) || starts(b"OTTO") || starts(b"true") || starts(b"ttcf") {
        Family::Font
    } else if starts(b"wOFF") {
        Family::Woff
    } else if starts(b"wOF2") {
        Family::Woff2
    } else if b.len() >= 8 && &b[4..8] == b"ftyp" {
        Family::Iso
    } else if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Family::Matroska
    } else if starts(b"ID3") || (b.len() >= 2 && b[0] == 0xFF && (b[1] & 0xE6) == 0xE2) {
        Family::Mp3
    } else if b.len() >= 2 && b[0] == 0xFF && (b[1] & 0xF6) == 0xF0 {
        Family::Adts
    } else if starts(b"OggS") {
        Family::Ogg
    } else if starts(b"fLaC") {
        Family::Flac
    } else if starts(b"glTF") {
        Family::Glb
    } else if starts(b"MOC3") {
        Family::Moc3
    } else if starts(b"TRNSRTS\0") {
        Family::Inochi
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        Family::Zip
    } else if starts(b"\0asm") {
        Family::Wasm
    } else if starts(&[0xFE, 0xED, 0xFA, 0xCE])
        || starts(&[0xFE, 0xED, 0xFA, 0xCF])
        || starts(&[0xCE, 0xFA, 0xED, 0xFE])
        || starts(&[0xCF, 0xFA, 0xED, 0xFE])
        || starts(&[0xCA, 0xFE, 0xBA, 0xBE])
    {
        Family::MachO
    } else if starts(b"MZ") {
        Family::Pe
    } else if starts(b"\x7fELF") {
        Family::Elf
    } else if starts(b"#!") {
        Family::Shebang
    } else if let Ok(text) = std::str::from_utf8(strip_bom(b)) {
        let t = text.trim_start();
        if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(text).is_ok() {
            Family::Json
        } else {
            Family::Text
        }
    } else {
        Family::Unknown
    }
}

fn strip_bom(b: &[u8]) -> &[u8] {
    b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(b)
}

/// What a file's extension says it should be, if Lightair knows the extension.
pub fn expected(path: &str) -> Option<&'static [Family]> {
    let ext = extension(path);
    Some(match ext.as_str() {
        "png" => &[Family::Png],
        "jpg" | "jpeg" => &[Family::Jpeg],
        "gif" => &[Family::Gif],
        "webp" => &[Family::Webp],
        "json" | "gltf" => &[Family::Json],
        "svg" | "html" | "htm" | "css" | "js" | "mjs" | "txt" | "md" | "glsl" | "wgsl" | "effect" | "shader"
        | "hlsl" | "cube" | "3dl" | "csv" | "xml" | "frag" | "vert" => &[Family::Text, Family::Json],
        "ttf" | "otf" | "ttc" => &[Family::Font],
        "woff" => &[Family::Woff],
        "woff2" => &[Family::Woff2],
        "mp4" | "mov" | "m4v" | "m4a" => &[Family::Iso],
        "webm" | "mkv" => &[Family::Matroska],
        "mp3" => &[Family::Mp3],
        "wav" => &[Family::Wav],
        "ogg" | "oga" | "opus" => &[Family::Ogg],
        "flac" => &[Family::Flac],
        "aac" => &[Family::Adts],
        "glb" | "vrm" => &[Family::Glb],
        "moc3" => &[Family::Moc3],
        "inp" | "inx" => &[Family::Inochi],
        "lairava" => &[Family::Zip],
        "wasm" => &[Family::Wasm],
        "dylib" | "so" | "exe" | "dll" | "node" => &[Family::MachO, Family::Pe, Family::Elf],
        _ => return None,
    })
}

/// The lower-case extension of a path ("" when none).
pub fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Whether `kind`'s content may hold a file of `family`.
pub fn allowed(kind: &Kind, family: Family, path: &str) -> bool {
    use Family as F;
    let image = family.is_image();
    let av = family.is_audio() || family.is_video();
    let text = matches!(family, F::Text | F::Json);
    let fonts = matches!(family, F::Font | F::Woff | F::Woff2);
    let avatar = matches!(family, F::Glb | F::Moc3 | F::Inochi) || (family == F::Zip && extension(path) == "lairava");
    match kind {
        Kind::Widget => image || av || text || fonts,
        Kind::Template => image || av || text || fonts || avatar,
        Kind::Look | Kind::EffectPreset | Kind::Transition | Kind::Theme => image || av || text || fonts,
        Kind::Recipe => text,
        Kind::Sound => family.is_audio() || text,
        Kind::Font => fonts || text,
        Kind::Lut => text || image,
        Kind::Avatar => avatar || image || text || family.is_audio(),
        Kind::Media => image || av || text || fonts || avatar,
        Kind::ShaderEffect | Kind::ShaderTransition | Kind::ShaderSource => text || image,
        Kind::Extension => text || image || fonts || av,
        Kind::WasmEffect | Kind::WasmSource => family == F::Wasm || text || image,
        Kind::Helper => true,
        Kind::Other(_) => false,
    }
}

/// Files allowed at the top of a package, outside every content path: the manifest, a README,
/// a licence, a changelog, an icon and previews.
pub fn allowed_at_root(family: Family) -> bool {
    matches!(family, Family::Text | Family::Json) || family.is_image()
}
