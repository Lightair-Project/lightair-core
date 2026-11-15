//! Just enough HTTP/1.1: a request's head and body, and plain answers.

use std::io::{self, Read, Write};
use std::net::TcpStream;

/// Bodies larger than this aren't read (16 MB).
const MAX_BODY: usize = 16 << 20;
const MAX_HEAD: usize = 64 << 10;

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    /// HTTP/1.0 (no keep-alive unless asked).
    old: bool,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn keep_alive(&self) -> bool {
        match self.header("connection") {
            Some(c) if c.eq_ignore_ascii_case("close") => false,
            Some(c) if c.eq_ignore_ascii_case("keep-alive") => true,
            _ => !self.old,
        }
    }
}

/// The next request on the connection (`None`: the client closed it).
pub(crate) fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(n) => n,
            Err(e) if buf.is_empty() && matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(head)) => {
                let headers: Vec<(String, String)> = req
                    .headers
                    .iter()
                    .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string()))
                    .collect();
                let mut r = Request {
                    method: req.method.unwrap_or("GET").to_string(),
                    path: req.path.unwrap_or("/").to_string(),
                    old: req.version == Some(0),
                    headers,
                    body: vec![],
                };
                let len: usize = r.header("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
                if len > MAX_BODY {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "request too large"));
                }
                let mut body = buf[head..].to_vec();
                while body.len() < len {
                    let n = stream.read(&mut chunk)?;
                    if n == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                body.truncate(len);
                r.body = body;
                return Ok(Some(r));
            }
            Ok(httparse::Status::Partial) if buf.len() < MAX_HEAD => {}
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request")),
        }
    }
}

/// Writes an answer. The connection stays open after a success, closes after anything else.
pub(crate) fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n", body.len());
    if !body.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if status >= 300 {
        head.push_str("Connection: close\r\n");
    }
    for (n, v) in extra {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The request was addressed to this machine by a local name (or by none).
pub(crate) fn local_host(host: Option<&str>) -> bool {
    let Some(host) = host else { return true };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name.ends_with(".localhost")
}
