//! What widget and script code reaches: hosts in its URLs and resource attributes, and code built
//! at runtime. A careful tokenizer (comments, strings, template literals, regular expressions)
//! rather than a full JavaScript parser: it finds every literal address and every call that
//! builds or loads code, which is what the permission check needs.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

/// What a piece of code reaches.
#[derive(Debug, Default, Clone)]
pub struct Reach {
    /// Hosts in literal addresses (`https://`, `wss://`, `//host`).
    pub hosts: BTreeSet<String>,
    /// Code built or loaded at runtime (`eval`, `new Function`, a string timer, `import()` of a
    /// computed address, `document.write`, `importScripts`), with a short description each.
    pub dynamic: Vec<String>,
    /// Network calls whose address is computed (`fetch(url)`), so only the run can see where.
    pub computed: Vec<String>,
}

/// Scans JavaScript.
pub fn scan_js(src: &str, reach: &mut Reach) {
    let tokens = tokenize(src);
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Tok::Str(s) => hosts_in(s, &mut reach.hosts),
            Tok::Ident(name) => {
                let next_is_call = matches!(tokens.get(i + 1), Some(Tok::Punct('(')));
                let arg = tokens.get(i + 2);
                let prev_new = matches!(tokens.get(i.wrapping_sub(1)), Some(Tok::Ident(w)) if w == "new");
                let prev_dot = matches!(tokens.get(i.wrapping_sub(1)), Some(Tok::Punct('.')));
                if !next_is_call {
                    continue;
                }
                match name.as_str() {
                    "eval" if !prev_dot || is_window(&tokens, i) => reach.dynamic.push("eval()".into()),
                    "Function" if prev_new || !prev_dot => reach.dynamic.push("new Function()".into()),
                    "setTimeout" | "setInterval" if matches!(arg, Some(Tok::Str(_))) => {
                        reach.dynamic.push(format!("{name}() with a string"))
                    }
                    "import" if !matches!(arg, Some(Tok::Str(_))) => {
                        reach.dynamic.push("import() of a computed address".into())
                    }
                    "write" | "writeln" if prev_dot => reach.dynamic.push("document.write()".into()),
                    "importScripts" => reach.dynamic.push("importScripts()".into()),
                    "fetch" | "WebSocket" | "EventSource" | "sendBeacon" if !matches!(arg, Some(Tok::Str(_))) => {
                        reach.computed.push(format!("{name}() with a computed address"))
                    }
                    "open" if prev_dot && !matches!(tokens.get(i + 4), Some(Tok::Str(_))) => {
                        // xhr.open(method, url): only flag when the second argument isn't literal
                        if matches!(arg, Some(Tok::Str(m)) if ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"].contains(&m.to_ascii_uppercase().as_str()))
                        {
                            reach.computed.push("XMLHttpRequest.open() with a computed address".into());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn is_window(tokens: &[Tok], i: usize) -> bool {
    matches!(tokens.get(i.wrapping_sub(2)), Some(Tok::Ident(w)) if w == "window" || w == "globalThis" || w == "self")
}

/// Scans HTML: resource attributes, inline scripts, inline event handlers and styles.
pub fn scan_html(src: &str, reach: &mut Reach) {
    static ATTR: OnceLock<Regex> = OnceLock::new();
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    static HANDLER: OnceLock<Regex> = OnceLock::new();
    static REFRESH: OnceLock<Regex> = OnceLock::new();
    let attr = ATTR.get_or_init(|| {
        Regex::new(r#"(?is)<(?:script|img|iframe|link|source|video|audio|embed|object|track|input|form|frame|image|use)\b[^>]*?\b(?:src|href|data|action|poster|srcset|xlink:href)\s*=\s*["']?([^"'\s>]+)"#)
            .unwrap()
    });
    let script = SCRIPT.get_or_init(|| Regex::new(r"(?is)<script\b[^>]*>(.*?)</script>").unwrap());
    let handler =
        HANDLER.get_or_init(|| Regex::new(r#"(?is)\son[a-z]+\s*=\s*"([^"]*)"|\son[a-z]+\s*=\s*'([^']*)'"#).unwrap());
    let refresh = REFRESH
        .get_or_init(|| Regex::new(r#"(?is)<meta[^>]+http-equiv\s*=\s*["']?refresh[^>]*url=([^"'>\s]+)"#).unwrap());
    for c in attr.captures_iter(src) {
        hosts_in(&c[1], &mut reach.hosts);
    }
    for c in refresh.captures_iter(src) {
        hosts_in(&c[1], &mut reach.hosts);
    }
    for c in script.captures_iter(src) {
        scan_js(&c[1], reach);
    }
    for c in handler.captures_iter(src) {
        if let Some(m) = c.get(1).or_else(|| c.get(2)) {
            scan_js(m.as_str(), reach);
        }
    }
    scan_css(src, reach);
}

/// Scans CSS: `url(…)` and `@import`.
pub fn scan_css(src: &str, reach: &mut Reach) {
    static URL: OnceLock<Regex> = OnceLock::new();
    let url =
        URL.get_or_init(|| Regex::new(r#"(?i)url\(\s*["']?([^"')\s]+)|@import\s+(?:url\()?\s*["']([^"']+)"#).unwrap());
    for c in url.captures_iter(src) {
        if let Some(m) = c.get(1).or_else(|| c.get(2)) {
            hosts_in(m.as_str(), &mut reach.hosts);
        }
    }
}

/// Adds the hosts of every address in `text` (`https://host/…`, `wss://host`, `//host/…`).
pub fn hosts_in(text: &str, hosts: &mut BTreeSet<String>) {
    static ADDR: OnceLock<Regex> = OnceLock::new();
    let addr = ADDR.get_or_init(|| {
        Regex::new(r"(?i)(?:\b(?:https?|wss?)://|(?:^|[\s(=,'])//)([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+)")
            .unwrap()
    });
    for c in addr.captures_iter(text) {
        let host = c[1].to_ascii_lowercase();
        if !is_ignorable(&host) {
            hosts.insert(host);
        }
    }
}

/// Addresses that aren't network loads: XML namespaces and the like.
fn is_ignorable(host: &str) -> bool {
    matches!(host, "www.w3.org" | "w3.org" | "schemas.xmlsoap.org" | "ns.adobe.com" | "purl.org")
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
    Other,
}

/// Splits JavaScript into identifiers, string values (template literals' fixed parts included),
/// punctuation and the rest, skipping comments and regular expressions.
fn tokenize(src: &str) -> Vec<Tok> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if ch == '"' || ch == '\'' {
            let (s, next) = string(&c, i, ch);
            out.push(Tok::Str(s));
            i = next;
        } else if ch == '`' {
            // template literal: its fixed parts as one string, `${…}` scanned as code
            let mut s = String::new();
            i += 1;
            while i < c.len() && c[i] != '`' {
                if c[i] == '\\' {
                    i += 2;
                    continue;
                }
                if c[i] == '$' && c.get(i + 1) == Some(&'{') {
                    let mut depth = 1;
                    let start = i + 2;
                    i += 2;
                    while i < c.len() && depth > 0 {
                        match c[i] {
                            '{' => depth += 1,
                            '}' => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                    let inner: String = c[start..i.saturating_sub(1)].iter().collect();
                    out.extend(tokenize(&inner));
                    s.push(' ');
                    continue;
                }
                s.push(c[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if ch == '/' && regex_allowed(out.last()) {
            i += 1;
            let mut in_class = false;
            while i < c.len() && c[i] != '\n' {
                match c[i] {
                    '\\' => i += 1,
                    '[' => in_class = true,
                    ']' => in_class = false,
                    '/' if !in_class => break,
                    _ => {}
                }
                i += 1;
            }
            i += 1;
            while i < c.len() && c[i].is_alphanumeric() {
                i += 1;
            }
            out.push(Tok::Other);
        } else if ch.is_alphabetic() || ch == '_' || ch == '$' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect()));
        } else if ch.is_ascii_digit() {
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Other);
        } else {
            out.push(Tok::Punct(ch));
            i += 1;
        }
    }
    out
}

fn string(c: &[char], start: usize, quote: char) -> (String, usize) {
    let mut s = String::new();
    let mut i = start + 1;
    while i < c.len() && c[i] != quote && c[i] != '\n' {
        if c[i] == '\\' && i + 1 < c.len() {
            s.push(c[i + 1]);
            i += 2;
            continue;
        }
        s.push(c[i]);
        i += 1;
    }
    (s, i + 1)
}

/// Whether a `/` here starts a regular expression rather than a division.
fn regex_allowed(prev: Option<&Tok>) -> bool {
    match prev {
        None => true,
        Some(Tok::Punct(p)) => !matches!(p, ')' | ']' | '}'),
        Some(Tok::Ident(w)) => matches!(
            w.as_str(),
            "return"
                | "typeof"
                | "instanceof"
                | "in"
                | "of"
                | "new"
                | "delete"
                | "void"
                | "throw"
                | "case"
                | "do"
                | "else"
        ),
        Some(Tok::Str(_)) | Some(Tok::Other) => false,
    }
}
