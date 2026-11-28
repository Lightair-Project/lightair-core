//! Which permission each of Lightair's methods needs from a script (docs/13 tier 3,
//! catalog-formats §6), and which hosts a script may fetch.

/// What calling a method takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// This permission.
    Permission(&'static str),
    /// Never available to scripts, whatever they hold (with why).
    Refused(&'static str),
}

/// What `lightair.call(method)` needs.
pub fn for_method(method: &str) -> Needs {
    use Needs::*;
    let (group, verb) = method.rsplit_once('.').unwrap_or((method, ""));
    match method {
        "chat.send" => return Permission("chat.write"),
        "chat.history" => return Permission("chat.read"),
        "audio.levels" => return Permission("audio.levels"),
        "app.exit" => return Refused("scripts can't quit Lightair"),
        "script.run" => return Refused("scripts can't run other scripts"),
        "events.subscribe" => return Refused("use lightair.on() for events"),
        "preview.frame" | "scene.thumbnail" | "scene.thumbnails" => return Permission("screen.capture"),
        _ => {}
    }
    if ["auth", "pair", "session", "transfer", "takes", "plugins", "obswebsocket"].contains(&group) {
        return Refused("that part of Lightair isn't open to scripts");
    }
    // starting and stopping what goes out
    if ["output", "outputs", "record", "stream", "clip", "vcam", "replay"].contains(&group)
        && !matches!(verb, "list" | "get" | "state" | "status" | "health" | "saved" | "categories")
        && method != "stream.info.get"
    {
        return Permission("outputs.control");
    }
    // reading
    if matches!(
        verb,
        "list"
            | "get"
            | "state"
            | "status"
            | "history"
            | "health"
            | "saved"
            | "categories"
            | "program"
            | "targets"
            | "version"
    ) || method == "stream.info.get"
    {
        return Permission("project.read");
    }
    Permission("project.write")
}

/// What `lightair.on(event)` needs.
pub fn for_event(event: &str) -> Needs {
    match event {
        "chat" => Needs::Permission("chat.read"),
        "levels" => Needs::Permission("audio.levels"),
        "scene" | "outputs" | "object" | "source" => Needs::Permission("project.read"),
        _ => Needs::Refused("no such event"),
    }
}

/// Whether `permissions` let a script fetch `url` (http or https only; through Lightair's own
/// fetch). Hosts as written in the URL; the app also refuses names that turn out to be on the
/// home network without `network.lan`.
pub fn may_fetch(permissions: &[String], url: &str) -> Result<(), String> {
    let Some((scheme, rest)) = url.split_once("://") else { return Err("not an address".into()) };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err("only http and https addresses".into());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => {
            (h.trim_matches(['[', ']']).to_ascii_lowercase(), p.parse::<u16>().ok())
        }
        _ => (authority.trim_matches(['[', ']']).to_ascii_lowercase(), None),
    };
    if host.is_empty() {
        return Err("not an address".into());
    }
    let local = host == "localhost" || host == "127.0.0.1" || host == "::1";
    for p in permissions {
        let Some(rule) = p.strip_prefix("network:") else { continue };
        if let Some(lp) = rule.strip_prefix("localhost:") {
            if local && port.map(|x| x.to_string()) == Some(lp.to_string()) {
                return Ok(());
            }
            continue;
        }
        if local {
            continue;
        }
        let matches = rule == "*"
            || match rule.strip_prefix("*.") {
                Some(base) => host == base || host.ends_with(&format!(".{base}")),
                None => host == rule,
            };
        if matches {
            return Ok(());
        }
    }
    if permissions.iter().any(|p| p == "network.lan") && is_private_literal(&host) {
        return Ok(());
    }
    Err(format!("it didn't declare network:{host}"))
}

fn is_private_literal(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v)) => v.is_private() || v.is_link_local(),
        Ok(std::net::IpAddr::V6(v)) => (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => host.ends_with(".local"),
    }
}
