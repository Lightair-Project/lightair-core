//! Twitch's nearest ingest: Twitch's list of ingests, each reachable one
//! tried with a TCP connect at once, the fastest kept for half an hour.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Ingest {
    pub name: String,
    /// The server, without the key (`rtmps://….contribute.live-video.net/app/`).
    pub url: String,
}

const LIST: &str = "https://ingest.twitch.tv/ingests";
const KEEP: Duration = Duration::from_secs(30 * 60);

static NEAREST: Mutex<Option<(Instant, Ingest)>> = Mutex::new(None);

/// The ingests Twitch lists as available, RTMPS where it gives one.
pub fn parse(v: &Value) -> Vec<Ingest> {
    v["ingests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["availability"].as_f64().unwrap_or(1.0) > 0.0)
        .filter_map(|i| {
            let template = i["url_template_secure"]
                .as_str()
                .filter(|t| !t.is_empty())
                .or(i["url_template"].as_str())?;
            Some(Ingest {
                name: i["name"].as_str().unwrap_or("Twitch").to_string(),
                url: template.replace("{stream_key}", ""),
            })
        })
        .collect()
}

/// The server's host and port (RTMPS 443, RTMP 1935 unless given).
fn address(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let host_port = rest.split('/').next()?;
    let port = if scheme == "rtmps" { 443 } else { 1935 };
    Some(match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (host_port.to_string(), port),
    })
}

/// How long a TCP connection to the ingest takes (`None`: none within 1.5 s).
fn connect_time(url: &str) -> Option<Duration> {
    let (host, port) = address(url)?;
    let addr = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    let start = Instant::now();
    TcpStream::connect_timeout(&addr, Duration::from_millis(1500)).ok()?;
    Some(start.elapsed())
}

/// The one of `ingests` that `probe` finds fastest, all tried at once.
pub fn fastest(
    ingests: Vec<Ingest>,
    probe: impl Fn(&str) -> Option<Duration> + Sync,
) -> Option<Ingest> {
    let probe = &probe;
    let times: Vec<Option<Duration>> = std::thread::scope(|s| {
        let tries: Vec<_> = ingests
            .iter()
            .map(|i| {
                let url = i.url.as_str();
                s.spawn(move || probe(url))
            })
            .collect();
        tries.into_iter().map(|t| t.join().ok().flatten()).collect()
    });
    ingests
        .into_iter()
        .zip(times)
        .filter_map(|(i, t)| Some((t?, i)))
        .min_by_key(|(t, _)| *t)
        .map(|(_, i)| i)
}

/// Twitch's nearest ingest, found once per half hour (Twitch's list and a connect to each).
pub fn twitch_nearest() -> Result<Ingest, String> {
    if let Some((at, i)) = NEAREST.lock().clone()
        && at.elapsed() < KEEP
    {
        return Ok(i);
    }
    let agent = lightair_live::net::agent();
    let v = lightair_live::net::get_json(&agent, LIST, &[])?;
    let found = fastest(parse(&v), connect_time).ok_or("No Twitch ingest answered.")?;
    *NEAREST.lock() = Some((Instant::now(), found.clone()));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_read_and_the_fastest_kept() {
        let v = serde_json::json!({ "ingests": [
            { "name": "US East: Ashburn, VA", "availability": 1.0,
              "url_template": "rtmp://iad05.contribute.live-video.net/app/{stream_key}",
              "url_template_secure": "rtmps://iad05.contribute.live-video.net/app/{stream_key}" },
            { "name": "Europe: Germany, Frankfurt", "availability": 1.0,
              "url_template": "rtmp://fra05.contribute.live-video.net/app/{stream_key}" },
            { "name": "Closed", "availability": 0.0,
              "url_template": "rtmp://x.contribute.live-video.net/app/{stream_key}" }
        ]});
        let list = parse(&v);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].url, "rtmps://iad05.contribute.live-video.net/app/");
        assert_eq!(
            address(&list[0].url),
            Some(("iad05.contribute.live-video.net".into(), 443))
        );
        assert_eq!(address("rtmp://h:1936/app/"), Some(("h".into(), 1936)));
        let near = fastest(list, |url| {
            Some(Duration::from_millis(if url.contains("fra05") {
                20
            } else {
                90
            }))
        });
        assert_eq!(near.unwrap().name, "Europe: Germany, Frankfurt");
    }
}
