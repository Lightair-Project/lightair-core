//! Transfers of takes, clips and projects between a paired phone and the desktop (lightair-live's
//! docs/05 §7), on the phones' TLS port only:
//!
//! - `GET /transfer/<token>/<name>`, with `Range: bytes=a-b` for a piece (206): a file the app
//!   offered with [`Transfers::offer_download`], streamed from disk. The connection stays open
//!   for the next request.
//! - `PUT /transfer/<token>/<name>` with `Content-Range: bytes a-b/total`: a piece of a file the
//!   app agreed to take with [`Transfers::offer_upload`]. Pieces come in order (one at the offset
//!   received so far, else 416); each is written to `<name>.partial`, and the last one checks the
//!   file's SHA-256 and renames it into place (201), or drops it (422: send it again).
//! - `HEAD /transfer/<token>/<name>`: how much of an upload has arrived (`Lightair-Received`), or a
//!   download's size, for resuming.
//!
//! The token is the authorisation: 128 random bits, valid for 24 hours, over the pinned TLS port
//! only, and only while the device it was issued to is still paired. A file's name may have
//! folders (`Take.lightair-take/screen.mp4`); `..` and absolute names aren't taken.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::{self, Request};

/// How long a token works.
pub const VALID_FOR: Duration = Duration::from_secs(24 * 3600);
/// Upload pieces larger than this are refused (the phones send 8 MB).
pub const MAX_PIECE: u64 = 16 << 20;

/// A file in a transfer: its name (folders kept), size and SHA-256 (lowercase hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

impl FileSpec {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "size": self.size, "sha256": self.sha256})
    }

    pub fn from_json(v: &Value) -> Option<FileSpec> {
        Some(FileSpec {
            name: v.get("name")?.as_str()?.to_owned(),
            size: v.get("size")?.as_u64()?,
            sha256: v.get("sha256")?.as_str()?.to_ascii_lowercase(),
        })
    }
}

/// What the phone is told: the token and the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub token: String,
    pub manifest: Vec<FileSpec>,
}

impl Transfer {
    pub fn to_json(&self) -> Value {
        json!({"token": self.token, "manifest": self.manifest.iter().map(FileSpec::to_json).collect::<Vec<_>>()})
    }
}

/// An upload whose every file arrived and checked out: the app takes it from here.
#[derive(Debug, Clone)]
pub struct Finished {
    pub transfer: Transfer,
    pub device: String,
    /// Where its files are (by name, folders kept).
    pub dir: PathBuf,
    /// What the app said about it when it offered the upload (its kind, …).
    pub meta: Value,
}

enum Kind {
    Download(HashMap<String, PathBuf>),
    Upload { dir: PathBuf, meta: Value, done: bool },
}

struct Entry {
    device: String,
    transfer: Transfer,
    kind: Kind,
    until: Instant,
}

/// The transfers on offer, shared by the app (which offers them) and the phones' port (which
/// serves them).
#[derive(Default)]
pub struct Transfers {
    entries: Mutex<HashMap<String, Entry>>,
    /// SHA-256s already worked out, by (path, size, modified): a take fetched again isn't hashed
    /// again.
    hashes: Mutex<HashMap<(PathBuf, u64, SystemTime), String>>,
    finished: Mutex<Vec<Finished>>,
}

fn token() -> String {
    use base64::Engine;
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// A name a transfer may use: relative, no `..`, no empty parts, `/` between folders.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.starts_with('/')
        && name.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
        && Path::new(name).components().all(|c| matches!(c, Component::Normal(_)))
}

/// A file's SHA-256, lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl Transfers {
    fn hash(&self, path: &Path) -> io::Result<(u64, String)> {
        let meta = fs::metadata(path)?;
        let key = (path.to_path_buf(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        if let Some(h) = self.hashes.lock().get(&key) {
            return Ok((meta.len(), h.clone()));
        }
        let h = sha256_file(path)?;
        self.hashes.lock().insert(key, h.clone());
        Ok((meta.len(), h))
    }

    fn purge(&self) {
        let now = Instant::now();
        self.entries.lock().retain(|_, e| e.until > now);
    }

    /// Offers files to a device: (name, where it is). Hashes them (once per version of a file),
    /// so it can take a while for big ones: call it off the UI thread.
    pub fn offer_download(&self, device: &str, files: Vec<(String, PathBuf)>) -> io::Result<Transfer> {
        let mut manifest = vec![];
        let mut map = HashMap::new();
        for (name, path) in files {
            if !safe_name(&name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a name a transfer can use: {name}"),
                ));
            }
            let (size, sha256) = self.hash(&path)?;
            manifest.push(FileSpec { name: name.clone(), size, sha256 });
            map.insert(name, path);
        }
        self.purge();
        let transfer = Transfer { token: token(), manifest };
        self.entries.lock().insert(
            transfer.token.clone(),
            Entry {
                device: device.to_owned(),
                transfer: transfer.clone(),
                kind: Kind::Download(map),
                until: Instant::now() + VALID_FOR,
            },
        );
        Ok(transfer)
    }

    /// Agrees to take files from a device into `dir` (each at `dir/<name>`). `meta` comes back
    /// with [`Finished`].
    pub fn offer_upload(&self, device: &str, files: Vec<FileSpec>, dir: PathBuf, meta: Value) -> io::Result<Transfer> {
        if files.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no files"));
        }
        for f in &files {
            if !safe_name(&f.name) || f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a file a transfer can take: {}", f.name),
                ));
            }
        }
        self.purge();
        let transfer = Transfer { token: token(), manifest: files };
        self.entries.lock().insert(
            transfer.token.clone(),
            Entry {
                device: device.to_owned(),
                transfer: transfer.clone(),
                kind: Kind::Upload { dir, meta, done: false },
                until: Instant::now() + VALID_FOR,
            },
        );
        Ok(transfer)
    }

    /// How a transfer stands: `{done, files: [{name, received, size}]}` (`None`: no such token).
    pub fn status(&self, token: &str) -> Option<Value> {
        let entries = self.entries.lock();
        let e = entries.get(token)?;
        Some(match &e.kind {
            Kind::Download(_) => json!({"done": false, "kind": "download",
                "files": e.transfer.manifest.iter().map(|f| json!({"name": f.name, "size": f.size})).collect::<Vec<_>>()}),
            Kind::Upload { dir, done, .. } => json!({"done": done, "kind": "upload",
                "files": e.transfer.manifest.iter().map(|f| json!({"name": f.name, "size": f.size, "received": received(dir, f)})).collect::<Vec<_>>()}),
        })
    }

    /// The device a token was issued to.
    pub fn device_of(&self, token: &str) -> Option<String> {
        self.entries.lock().get(token).map(|e| e.device.clone())
    }

    /// Uploads that have finished since the last call.
    pub fn take_finished(&self) -> Vec<Finished> {
        std::mem::take(&mut *self.finished.lock())
    }
}

/// How much of an upload's file has arrived (the whole file once it's in place).
fn received(dir: &Path, f: &FileSpec) -> u64 {
    if dir.join(&f.name).is_file() {
        return f.size;
    }
    fs::metadata(partial(dir, &f.name)).map_or(0, |m| m.len())
}

fn partial(dir: &Path, name: &str) -> PathBuf {
    let mut p = dir.join(name).into_os_string();
    p.push(".partial");
    PathBuf::from(p)
}

/// `/transfer/<token>/<name>`, the name percent-decoded.
pub(crate) fn parse_path(path: &str) -> Option<(String, String)> {
    let rest = path.split('?').next()?.strip_prefix("/transfer/")?;
    let (token, name) = rest.split_once('/')?;
    let name = percent_decode(name)?;
    (!token.is_empty() && safe_name(&name)).then(|| (token.to_owned(), name))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a size: the first and last byte.
fn range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (first, last) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (size.saturating_sub(n), size - 1)
        }
        (a, "") => (a.parse().ok()?, size - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(size - 1)),
    };
    (first <= last && first < size).then_some((first, last))
}

/// `bytes a-b/total`.
fn content_range(header: &str) -> Option<(u64, u64, u64)> {
    let spec = header.trim().strip_prefix("bytes ")?;
    let (r, total) = spec.split_once('/')?;
    let (a, b) = r.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn head(stream: &mut impl Write, status: u16, reason: &str, headers: &[(&str, String)]) -> io::Result<()> {
    let mut h = format!("HTTP/1.1 {status} {reason}\r\n");
    for (n, v) in headers {
        h.push_str(&format!("{n}: {v}\r\n"));
    }
    h.push_str("\r\n");
    stream.write_all(h.as_bytes())
}

fn answer(
    stream: &mut impl Write,
    status: u16,
    reason: &str,
    body: &Value,
    extra: &[(&str, String)],
) -> io::Result<()> {
    let body = body.to_string();
    let mut headers =
        vec![("Content-Type", "application/json".to_string()), ("Content-Length", body.len().to_string())];
    headers.extend(extra.iter().map(|(n, v)| (*n, v.clone())));
    head(stream, status, reason, &headers)?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Serves transfer requests on a connection, the first already read, until the phone closes it
/// or asks for something else. `alive(device)`: the device is still paired.
pub(crate) fn serve<S: Read + Write>(
    stream: &mut S,
    mut req: Request,
    transfers: &Transfers,
    alive: &dyn Fn(&str) -> bool,
) -> io::Result<()> {
    loop {
        let keep = req.keep_alive();
        let ok = one(stream, &req, transfers, alive)?;
        if !ok || !keep {
            return Ok(());
        }
        match http::read_request(stream)? {
            Some(next) if next.path.starts_with("/transfer/") => req = next,
            Some(_) => return http::respond(stream, 404, "text/plain", b"Not found", &[]),
            None => return Ok(()),
        }
    }
}

/// One request: whether the connection may carry another.
fn one<S: Read + Write>(
    stream: &mut S,
    req: &Request,
    transfers: &Transfers,
    alive: &dyn Fn(&str) -> bool,
) -> io::Result<bool> {
    let refuse = |stream: &mut S, status: u16, reason: &str, why: &str| -> io::Result<bool> {
        answer(stream, status, reason, &json!({"error": why}), &[("Connection", "close".into())])?;
        Ok(false)
    };
    let Some((token, name)) = parse_path(&req.path) else {
        return refuse(stream, 404, "Not Found", "No such file");
    };
    transfers.purge();
    // what the request is about, read under the lock, used after it
    let found = {
        let entries = transfers.entries.lock();
        entries.get(&token).map(|e| {
            let spec = e.transfer.manifest.iter().find(|f| f.name == name).cloned();
            let target = match &e.kind {
                Kind::Download(map) => map.get(&name).cloned().map(Target::Download),
                Kind::Upload { dir, .. } => Some(Target::Upload(dir.clone())),
            };
            (e.device.clone(), spec, target)
        })
    };
    let Some((device, Some(spec), Some(target))) = found else {
        return refuse(stream, 404, "Not Found", "This transfer has ended or never was. Ask again.");
    };
    if !alive(&device) {
        return refuse(stream, 403, "Forbidden", "This device isn't paired with this computer any more.");
    }
    match (req.method.as_str(), target) {
        ("HEAD", Target::Download(_)) => {
            head(stream, 200, "OK", &[("Content-Length", spec.size.to_string()), ("Accept-Ranges", "bytes".into())])?;
            stream.flush()?;
            Ok(true)
        }
        ("GET", Target::Download(path)) => {
            let mut f = File::open(&path)?;
            let size = f.metadata()?.len();
            let (status, reason, first, last) = match req.header("range") {
                None => (200, "OK", 0, size.saturating_sub(1)),
                Some(r) => match range(r, size) {
                    Some((a, b)) => (206, "Partial Content", a, b),
                    None => {
                        answer(
                            stream,
                            416,
                            "Range Not Satisfiable",
                            &json!({"error": "no such range"}),
                            &[("Content-Range", format!("bytes */{size}"))],
                        )?;
                        return Ok(true);
                    }
                },
            };
            let len = if size == 0 { 0 } else { last - first + 1 };
            let mut headers = vec![
                ("Content-Length", len.to_string()),
                ("Content-Type", "application/octet-stream".to_string()),
                ("Accept-Ranges", "bytes".to_string()),
            ];
            if status == 206 {
                headers.push(("Content-Range", format!("bytes {first}-{last}/{size}")));
            }
            head(stream, status, reason, &headers)?;
            f.seek(SeekFrom::Start(first))?;
            let copied = io::copy(&mut (&mut f).take(len), stream)?;
            stream.flush()?;
            Ok(copied == len)
        }
        ("HEAD", Target::Upload(dir)) => {
            head(
                stream,
                200,
                "OK",
                &[("Content-Length", "0".into()), ("Lightair-Received", received(&dir, &spec).to_string())],
            )?;
            stream.flush()?;
            Ok(true)
        }
        ("PUT", Target::Upload(dir)) => put(stream, req, transfers, &token, &spec, &dir),
        _ => refuse(stream, 405, "Method Not Allowed", "GET or HEAD a download; PUT or HEAD an upload"),
    }
}

enum Target {
    Download(PathBuf),
    Upload(PathBuf),
}

fn put<S: Read + Write>(
    stream: &mut S,
    req: &Request,
    transfers: &Transfers,
    token: &str,
    spec: &FileSpec,
    dir: &Path,
) -> io::Result<bool> {
    let have = received(dir, spec);
    let told = |stream: &mut S, status: u16, reason: &str, why: &str, have: u64| -> io::Result<bool> {
        answer(
            stream,
            status,
            reason,
            &json!({"error": why, "received": have}),
            &[("Lightair-Received", have.to_string())],
        )?;
        Ok(true)
    };
    if have >= spec.size && dir.join(&spec.name).is_file() {
        return told(stream, 200, "OK", "already here", spec.size);
    }
    let body = &req.body;
    // an empty file: an empty body, without Content-Range or with `bytes */0`
    if spec.size == 0 {
        let range = req.header("content-range").map(str::trim);
        if !body.is_empty() || range.is_some_and(|r| r != "bytes */0") {
            return told(stream, 400, "Bad Request", "an empty file is sent as an empty body", 0);
        }
        let part = partial(dir, &spec.name);
        if let Some(parent) = part.parent() {
            fs::create_dir_all(parent)?;
        }
        File::create(&part)?;
        return finish(stream, transfers, token, spec, dir, &part);
    }
    let (first, last, total) = match req.header("content-range") {
        Some(r) => match content_range(r) {
            Some(x) => x,
            None => return told(stream, 400, "Bad Request", "Content-Range: bytes a-b/total", have),
        },
        // the whole file in one go
        None if body.len() as u64 == spec.size => (0, spec.size.saturating_sub(1), spec.size),
        None => return told(stream, 400, "Bad Request", "Content-Range: bytes a-b/total", have),
    };
    if total != spec.size || last < first || last - first + 1 != body.len() as u64 || body.len() as u64 > MAX_PIECE {
        return told(stream, 400, "Bad Request", "the piece doesn't match its Content-Range or the file's size", have);
    }
    if first != have {
        return told(
            stream,
            416,
            "Range Not Satisfiable",
            "send the piece that starts where Lightair-Received says",
            have,
        );
    }
    let part = partial(dir, &spec.name);
    if let Some(parent) = part.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&part)?;
    f.write_all(body)?;
    f.flush()?;
    drop(f);
    let have = have + body.len() as u64;
    if have < spec.size {
        return told(stream, 202, "Accepted", "more to come", have);
    }
    finish(stream, transfers, token, spec, dir, &part)
}

/// The whole file is here: checked, then put in place (the upload finished once all are).
fn finish<S: Read + Write>(
    stream: &mut S,
    transfers: &Transfers,
    token: &str,
    spec: &FileSpec,
    dir: &Path,
    part: &Path,
) -> io::Result<bool> {
    if sha256_file(part)? != spec.sha256 {
        let _ = fs::remove_file(part);
        answer(
            stream,
            422,
            "Unprocessable Entity",
            &json!({"error": "the file didn't match its SHA-256: send it again", "received": 0}),
            &[("Lightair-Received", "0".into())],
        )?;
        return Ok(true);
    }
    fs::rename(part, dir.join(&spec.name))?;
    // the last file of the upload: the app's to take
    let mut entries = transfers.entries.lock();
    if let Some(e) = entries.get_mut(token)
        && let Kind::Upload { dir, meta, done } = &mut e.kind
        && !*done
        && e.transfer.manifest.iter().all(|f| dir.join(&f.name).is_file())
    {
        *done = true;
        transfers.finished.lock().push(Finished {
            transfer: e.transfer.clone(),
            device: e.device.clone(),
            dir: dir.clone(),
            meta: meta.clone(),
        });
    }
    drop(entries);
    answer(
        stream,
        201,
        "Created",
        &json!({"received": spec.size, "done": true}),
        &[("Lightair-Received", spec.size.to_string())],
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_inside() {
        assert!(safe_name("screen.mp4"));
        assert!(safe_name("Studio take 10.32.lightair-take/screen.mp4"));
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a\\b", "./a", "a/"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(range("bytes=900-5000", 1000), Some((900, 999)));
        assert_eq!(range("bytes=1000-", 1000), None);
        assert_eq!(range("bytes=0-1,5-6", 1000), None);
        assert_eq!(content_range("bytes 0-8388607/20000000"), Some((0, 8388607, 20000000)));
    }

    #[test]
    fn paths() {
        assert_eq!(
            parse_path("/transfer/abc/Take%201.lightair-take/screen.mp4"),
            Some(("abc".into(), "Take 1.lightair-take/screen.mp4".into()))
        );
        assert_eq!(parse_path("/transfer/abc/..%2Fx"), None);
        assert_eq!(parse_path("/transfer/abc"), None);
    }
}
