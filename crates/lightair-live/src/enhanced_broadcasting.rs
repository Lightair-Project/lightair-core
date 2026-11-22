//! Enhanced Broadcasting: a stream sent as several encodes in one Enhanced RTMP connection, as
//! the platform's configuration service plans them for this machine and these canvases. Twitch
//! uses it for Dual Format (a landscape and a vertical picture in one broadcast) and 2k.
//!
//! Written from the protocol, not from any implementation of it:
//! - What the service is for, what it may answer (1.5 to 10.5 Mbps, 2 to 5 renditions, any
//!   encoder settings, a new plan at every start) and Dual Format's rules: Twitch Help,
//!   "Enhanced Broadcasting with Multiple Encodes" and "Dual Format Streaming on Twitch".
//! - The request and answer field names, their meaning, and the key's `clientConfigId`: the
//!   service's interface as recorded in the desktop's docs/plans/twitch-dual-format.md §2.2-2.4
//!   (learned from how existing clients talk to the service, then confirmed against the service
//!   itself on 22 November 2026). Facts of the interface only: no other client's code, structure
//!   or comments are reused here.
//! - How the tracks travel: Veovera's Enhanced RTMP v2, "Multitrack Streaming via Enhanced RTMP".
//!
//! This module only asks for and reads the plan, and works out the ingest address and key. The
//! app runs the encoders and the connection (on the desktop, libobs's RTMP output).

use std::time::Duration;

use serde_json::{Value, json};

/// Twitch's configuration service.
pub const TWITCH: &str = "https://ingest.twitch.tv/api/v3/GetClientConfiguration";

/// How long to wait for the plan before going live without it.
pub const WAIT: Duration = Duration::from_secs(5);

/// The machine, as much as the service asks for: what it decides how many encodes this
/// machine can run from. Nothing else is sent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    pub physical_cores: u32,
    pub logical_cores: u32,
    /// The processor's name ("Apple M4 Pro"), when known.
    pub cpu_name: Option<String>,
    pub memory_total: u64,
    pub memory_free: u64,
    /// "macOS", "Windows".
    pub os_name: String,
    /// "15.1".
    pub os_version: String,
    pub arm: bool,
    pub gpus: Vec<Gpu>,
}

/// A graphics processor. The service refuses a machine without one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gpu {
    pub model: String,
    /// PCI vendor and device ids (Apple's chips: vendor 0x106b, device 0).
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_memory: u64,
    pub shared_memory: u64,
    pub driver: Option<String>,
}

/// A canvas to send: its size and frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasSize {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// What to ask the service for.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// The stream key (the service reads the channel from it).
    pub key: String,
    pub client: String,
    pub client_version: String,
    /// The codecs this machine can encode ("h264", "h265", "av1").
    pub codecs: Vec<String>,
    pub machine: Machine,
    /// The canvases, in track order: the main (landscape) one first, then the vertical one for
    /// Dual Format. A plan's encodes name them by this position.
    pub canvases: Vec<CanvasSize>,
    pub max_bitrate_kbps: Option<u32>,
    pub max_video_tracks: Option<u32>,
    /// Ask for Twitch's VOD audio track too.
    pub vod_track: bool,
    pub audio_rate: u32,
    pub audio_channels: u32,
}

impl Ask {
    /// The request body. Values that aren't known are sent as nulls.
    pub fn body(&self) -> Value {
        let m = &self.machine;
        let gpus: Vec<Value> = m
            .gpus
            .iter()
            .map(|g| {
                json!({
                    "model": g.model,
                    "vendor_id": g.vendor_id,
                    "device_id": g.device_id,
                    "dedicated_video_memory": g.dedicated_memory,
                    "shared_system_memory": g.shared_memory,
                    "driver_version": g.driver,
                })
            })
            .collect();
        let canvases: Vec<Value> = self
            .canvases
            .iter()
            .map(|c| {
                json!({
                    "width": c.width,
                    "height": c.height,
                    "canvas_width": c.width,
                    "canvas_height": c.height,
                    "framerate": { "numerator": c.fps_num, "denominator": c.fps_den.max(1) },
                })
            })
            .collect();
        json!({
            "service": "IVS",
            "schema_version": "2025-01-25",
            "authentication": self.key,
            "client": { "name": self.client, "version": self.client_version, "supported_codecs": self.codecs },
            "capabilities": {
                "cpu": { "physical_cores": m.physical_cores, "logical_cores": m.logical_cores, "speed": null, "name": m.cpu_name },
                "memory": { "total": m.memory_total, "free": m.memory_free },
                "gaming_features": null,
                "system": {
                    "version": m.os_version, "name": m.os_name, "build": 0, "release": m.os_version, "revision": "",
                    "bits": 64, "arm": m.arm, "armEmulation": false,
                },
                "gpu": if gpus.is_empty() { Value::Null } else { Value::Array(gpus) },
            },
            "preferences": {
                "maximum_aggregate_bitrate": self.max_bitrate_kbps,
                "maximum_video_tracks": self.max_video_tracks,
                "vod_track_audio": self.vod_track,
                "composition_gpu_index": null,
                "audio_samples_per_sec": self.audio_rate,
                "audio_channels": self.audio_channels,
                "audio_max_buffering_ms": 0,
                "audio_fixed_buffering": false,
                "canvases": canvases,
            },
        })
    }
}

/// What the service said about the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Fine,
    /// Go live if the person wants to; the words say why not to.
    Caution(String),
    /// Don't go live with this plan.
    Refused(String),
}

/// One video encode of the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoTrack {
    /// The encoder, as libobs names it (`com.apple.videotoolbox.videoencoder.ave.avc`, `obs_x264`…).
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    /// Its frame rate, when the plan says.
    pub fps: Option<(u32, u32)>,
    /// Which of the asked canvases it's made from (their position in [`Ask::canvases`]).
    pub canvas: usize,
    /// How it's scaled from the canvas ("OBS_SCALE_BICUBIC"…), when the plan says.
    pub scale: Option<String>,
    /// The encoder's own settings (bitrate, keyint_sec, profile, rate_control, bframes…).
    pub settings: Value,
    /// The connection's bitrate table for adaptive bitrate, when the plan has one.
    pub bitrate_table: Option<Value>,
}

impl VideoTrack {
    /// Its bitrate in kbps (from its settings), 0 when not said.
    pub fn kbps(&self) -> u32 {
        self.settings["bitrate"].as_u64().unwrap_or(0) as u32
    }
}

/// One audio encode of the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrack {
    pub channels: u32,
    pub settings: Value,
}

/// Where to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingest {
    pub rtmps: bool,
    /// The address with `{stream_key}` where the key goes.
    pub template: String,
    /// A key the service wants used instead of the person's.
    pub key: Option<String>,
}

/// The service's plan for this stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Ties the stream to this plan (it goes with the key).
    pub config_id: String,
    pub verdict: Verdict,
    /// In track order: video track *i* is the *i*-th.
    pub video: Vec<VideoTrack>,
    pub audio: Vec<AudioTrack>,
    /// The VOD track's audio, after the live ones.
    pub vod_audio: Vec<AudioTrack>,
    pub ingest: Vec<Ingest>,
}

/// Reads the service's answer (a plan file reads the same way). Fields it doesn't know are
/// ignored; what it needs and lacks is an error.
pub fn read(text: &str) -> Result<Plan, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("the plan isn't JSON: {e}"))?;
    let words = |v: &Value| plain(v["html_en_us"].as_str().unwrap_or(""));
    let verdict = match v["status"]["result"].as_str() {
        None | Some("success") => Verdict::Fine,
        Some("warning") => Verdict::Caution(words(&v["status"])),
        Some("error") => {
            let w = words(&v["status"]);
            Verdict::Refused(if w.is_empty() { "the platform refused the stream's settings".into() } else { w })
        }
        Some(other) => Verdict::Caution(format!("the platform answered \"{other}\"")),
    };
    let number = |v: &Value| v.as_u64().map(|n| n as u32);
    let video = v["encoder_configurations"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|e| {
            Ok(VideoTrack {
                encoder: e["type"].as_str().ok_or("a video encode without an encoder")?.to_string(),
                width: number(&e["width"]).ok_or("a video encode without a width")?,
                height: number(&e["height"]).ok_or("a video encode without a height")?,
                fps: number(&e["framerate"]["numerator"]).zip(number(&e["framerate"]["denominator"])),
                canvas: e["canvas_index"].as_u64().unwrap_or(0) as usize,
                scale: e["gpu_scale_type"].as_str().map(String::from),
                settings: if e["settings"].is_object() { e["settings"].clone() } else { json!({}) },
                bitrate_table: e.get("bitrate_interpolation_points").filter(|p| !p.is_null()).cloned(),
            })
        })
        .collect::<Result<Vec<_>, &str>>()?;
    let audio_of = |list: &Value| -> Vec<AudioTrack> {
        list.as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|a| AudioTrack {
                channels: number(&a["channels"]).unwrap_or(2),
                settings: if a["settings"].is_object() { a["settings"].clone() } else { json!({}) },
            })
            .collect()
    };
    let ingest = v["ingest_endpoints"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|e| {
            let protocol = e["protocol"].as_str()?.to_ascii_uppercase();
            protocol.starts_with("RTMP").then(|| Ingest {
                rtmps: protocol == "RTMPS",
                template: e["url_template"].as_str().unwrap_or("").to_string(),
                key: e["authentication"].as_str().filter(|k| !k.is_empty()).map(String::from),
            })
        })
        .filter(|i| !i.template.is_empty())
        .collect();
    Ok(Plan {
        config_id: v["meta"]["config_id"].as_str().unwrap_or("").to_string(),
        verdict,
        video,
        audio: audio_of(&v["audio_configurations"]["live"]),
        vod_audio: audio_of(&v["audio_configurations"]["vod"]),
        ingest,
    })
}

impl Plan {
    /// What keeps this plan from running here, in words (empty: nothing). `canvases`: how many
    /// were asked for; `encoders`: the encoders this machine has.
    pub fn problems(&self, canvases: usize, encoders: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        if let Verdict::Refused(why) = &self.verdict {
            out.push(why.clone());
        }
        if self.video.is_empty() {
            out.push("the plan has no video".into());
        }
        if self.audio.is_empty() {
            out.push("the plan has no audio".into());
        }
        for (i, t) in self.video.iter().enumerate() {
            if t.canvas >= canvases {
                out.push(format!("video track {} is for a canvas that wasn't sent", i + 1));
            }
            if !encoders.contains(&t.encoder) {
                out.push(format!("video track {} wants an encoder this computer doesn't have ({})", i + 1, t.encoder));
            }
        }
        for c in 0..canvases {
            if !self.video.iter().any(|t| t.canvas == c) {
                out.push(format!("the plan sends nothing from canvas {}", c + 1));
            }
        }
        if self.ingest.is_empty() {
            out.push("the plan has nowhere to send the stream".into());
        }
        out.dedup();
        out
    }

    /// All its video, in kbps.
    pub fn video_kbps(&self) -> u32 {
        self.video.iter().map(VideoTrack::kbps).sum()
    }

    /// The server address and the stream key to connect with: RTMPS when asked for and offered,
    /// else RTMP; the service's key if it gave one, else `key`; with the plan's id added to the
    /// key's query (any query the key had stays).
    pub fn connect(&self, rtmps: bool, key: &str) -> Option<(String, String)> {
        let ingest = self.ingest.iter().find(|i| i.rtmps == rtmps).or(self.ingest.first())?;
        let server = match ingest.template.find("{stream_key}") {
            Some(at) => ingest.template[..at].trim_end_matches('/').to_string(),
            None => ingest.template.trim_end_matches('/').to_string(),
        };
        let key = ingest.key.as_deref().unwrap_or(key);
        Some((server, with_plan_id(key, &self.config_id)))
    }
}

/// `key` with `clientConfigId=<id>` in its query (none: unchanged).
pub fn with_plan_id(key: &str, id: &str) -> String {
    if id.is_empty() {
        return key.to_string();
    }
    let joiner = if key.contains('?') { '&' } else { '?' };
    format!("{key}{joiner}clientConfigId={}", crate::oauth::encode(id))
}

/// The service's message as plain words: tags dropped, entities read, spaces tidied.
fn plain(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Asks `url` (the platform's service) for a plan; waits up to [`WAIT`].
pub fn fetch(url: &str, ask: &Ask) -> Result<Plan, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(crate::net::tls_config())
        .timeout_global(Some(WAIT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut res = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", &format!("{}/{}", ask.client, ask.client_version))
        .send(ask.body().to_string())
        .map_err(|e| format!("the platform's stream settings didn't come ({e})"))?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("the platform's stream settings didn't come (HTTP {status})"));
    }
    read(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> Ask {
        Ask {
            key: "live_123_abc".into(),
            client: "Lightair".into(),
            client_version: "0.9.0".into(),
            codecs: vec!["h264".into()],
            machine: Machine {
                physical_cores: 12,
                logical_cores: 12,
                cpu_name: Some("Apple M4 Pro".into()),
                memory_total: 48 << 30,
                memory_free: 20 << 30,
                os_name: "macOS".into(),
                os_version: "27.2".into(),
                arm: true,
                gpus: vec![Gpu { model: "Apple M4 Pro".into(), vendor_id: 0x106b, ..Gpu::default() }],
            },
            canvases: vec![
                CanvasSize { width: 1920, height: 1080, fps_num: 60, fps_den: 1 },
                CanvasSize { width: 1080, height: 1920, fps_num: 60, fps_den: 1 },
            ],
            max_bitrate_kbps: None,
            max_video_tracks: None,
            vod_track: false,
            audio_rate: 48_000,
            audio_channels: 2,
        }
    }

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort();
        k
    }

    /// What leaves the machine for the platform: exactly the fields its service documents (the
    /// machine's cores, memory, system and GPU, the canvases, the caps), nothing else about it.
    #[test]
    fn the_request_carries_only_the_documented_fields() {
        let b = ask().body();
        assert_eq!(keys(&b), ["authentication", "capabilities", "client", "preferences", "schema_version", "service"]);
        assert_eq!(b["authentication"], "live_123_abc", "the key goes in the body, nowhere else");
        assert_eq!(keys(&b["client"]), ["name", "supported_codecs", "version"]);
        let c = &b["capabilities"];
        assert_eq!(keys(c), ["cpu", "gaming_features", "gpu", "memory", "system"]);
        assert_eq!(keys(&c["cpu"]), ["logical_cores", "name", "physical_cores", "speed"]);
        assert_eq!(keys(&c["memory"]), ["free", "total"]);
        assert_eq!(
            keys(&c["system"]),
            ["arm", "armEmulation", "bits", "build", "name", "release", "revision", "version"]
        );
        assert_eq!(
            keys(&c["gpu"][0]),
            ["dedicated_video_memory", "device_id", "driver_version", "model", "shared_system_memory", "vendor_id"]
        );
        assert_eq!(c["gpu"][0]["vendor_id"], 0x106b);
        let p = &b["preferences"];
        assert_eq!(
            keys(p),
            [
                "audio_channels",
                "audio_fixed_buffering",
                "audio_max_buffering_ms",
                "audio_samples_per_sec",
                "canvases",
                "composition_gpu_index",
                "maximum_aggregate_bitrate",
                "maximum_video_tracks",
                "vod_track_audio"
            ]
        );
        // the canvases in track order: landscape first, then the vertical one
        let canvases = p["canvases"].as_array().unwrap();
        assert_eq!((canvases[0]["width"].as_u64(), canvases[1]["width"].as_u64()), (Some(1920), Some(1080)));
        assert_eq!(keys(&canvases[0]), ["canvas_height", "canvas_width", "framerate", "height", "width"]);
    }

    /// What isn't known or wasn't chosen goes as null, as the service expects, not left out.
    #[test]
    fn what_isnt_known_goes_as_null() {
        let mut a = ask();
        a.machine.cpu_name = None;
        a.machine.gpus.clear();
        let b = a.body();
        for path in [
            "/preferences/maximum_aggregate_bitrate",
            "/preferences/maximum_video_tracks",
            "/capabilities/cpu/name",
            "/capabilities/gpu",
        ] {
            assert_eq!(b.pointer(path), Some(&Value::Null), "{path}");
        }
        assert_eq!(ask().body()["capabilities"]["gpu"][0]["driver_version"], Value::Null);
        a.max_bitrate_kbps = Some(8000);
        a.max_video_tracks = Some(4);
        let b = a.body();
        assert_eq!(
            (b["preferences"]["maximum_aggregate_bitrate"].as_u64(), b["preferences"]["maximum_video_tracks"].as_u64()),
            (Some(8000), Some(4))
        );
    }

    /// A Dual Format plan shaped as the service answers (the desktop's docs/plans has full ones).
    const PLAN: &str = r#"{
        "meta": { "config_id": "plan 7/a" },
        "status": { "result": "success" },
        "ingest_endpoints": [
            { "protocol": "RTMP", "url_template": "rtmp://ingest.example/app/{stream_key}" },
            { "protocol": "RTMPS", "url_template": "rtmps://ingest.example:443/app/{stream_key}", "authentication": "live_service_key" },
            { "protocol": "SRT", "url_template": "srt://ingest.example:9000" }
        ],
        "encoder_configurations": [
            { "type": "enc.avc", "width": 1920, "height": 1080, "framerate": { "numerator": 60, "denominator": 1 }, "settings": { "bitrate": 6000 }, "canvas_index": 0 },
            { "type": "enc.avc", "width": 640, "height": 360, "settings": { "bitrate": 500 }, "canvas_index": 0 },
            { "type": "enc.avc", "width": 1080, "height": 1920, "settings": { "bitrate": 5000 }, "canvas_index": 1, "unknown": true },
            { "type": "enc.avc", "width": 360, "height": 640, "settings": { "bitrate": 500 }, "canvas_index": 1 }
        ],
        "audio_configurations": { "live": [ { "channels": 2, "settings": { "bitrate": 160 } } ], "vod": [] }
    }"#;

    /// The plan reads as tracks in order, each from its canvas, ready to run here.
    #[test]
    fn a_dual_format_plan_reads_as_tracks_in_order() {
        let p = read(PLAN).unwrap();
        assert_eq!(p.verdict, Verdict::Fine);
        let tracks: Vec<_> = p.video.iter().map(|t| (t.width, t.height, t.canvas)).collect();
        assert_eq!(tracks, [(1920, 1080, 0), (640, 360, 0), (1080, 1920, 1), (360, 640, 1)]);
        assert_eq!(p.video_kbps(), 12_000);
        assert_eq!(p.audio.len(), 1);
        assert_eq!(p.ingest.len(), 2, "only RTMP and RTMPS endpoints");
        assert!(p.problems(2, &["enc.avc".into()]).is_empty(), "{:?}", p.problems(2, &["enc.avc".into()]));
        assert!(read("{\"encoder_configurations\": [{\"type\": \"enc.avc\", \"height\": 1080}]}").is_err(), "no width");
        assert!(read("not json").is_err());
    }

    /// A plan that can't run here says why, before anything goes live.
    #[test]
    fn what_keeps_a_plan_from_running_is_said() {
        let p = read(PLAN).unwrap();
        // only one canvas sent: the vertical tracks name a canvas that isn't there
        assert!(p.problems(1, &["enc.avc".into()]).iter().any(|w| w.contains("canvas that wasn't sent")));
        // an encoder this computer doesn't have
        assert!(p.problems(2, &["other".into()]).iter().any(|w| w.contains("encoder")));
        // a canvas sent that the plan draws nothing from
        assert!(p.problems(3, &["enc.avc".into()]).iter().any(|w| w.contains("nothing from canvas 3")));
        // nowhere to send it
        let mut no_where = p.clone();
        no_where.ingest.clear();
        assert!(!no_where.problems(2, &["enc.avc".into()]).is_empty());
        assert!(no_where.connect(true, "k").is_none());
    }

    /// The service's verdict: an error keeps the stream from starting with its words (plain
    /// text), a warning is said and left to the person.
    #[test]
    fn the_services_verdict_comes_with_its_words() {
        let with = |status: &str| read(&format!(r#"{{"status": {status}}}"#)).unwrap().verdict;
        assert_eq!(
            with(r#"{"result": "error", "html_en_us": "<b>Too few</b> encodes &amp; tracks"}"#),
            Verdict::Refused("Too few encodes & tracks".into())
        );
        assert!(
            matches!(with(r#"{"result": "error"}"#), Verdict::Refused(w) if !w.is_empty()),
            "words even when none came"
        );
        assert_eq!(with(r#"{"result": "warning", "html_en_us": "Heads up"}"#), Verdict::Caution("Heads up".into()));
        assert!(matches!(with(r#"{"result": "maybe"}"#), Verdict::Caution(_)));
        let refused =
            read(r#"{"status": {"result": "error", "html_en_us": "No."}, "encoder_configurations": []}"#).unwrap();
        assert!(refused.problems(2, &[]).contains(&"No.".to_string()), "the refusal is the first reason");
    }

    /// The stream key carries the plan's id (the stream name is where the platform reads it),
    /// any query it had kept.
    #[test]
    fn the_key_carries_the_plan_id_and_keeps_its_query() {
        assert_eq!(with_plan_id("live_abc", "id1"), "live_abc?clientConfigId=id1");
        assert_eq!(with_plan_id("live_abc?bandwidthtest", "id1"), "live_abc?bandwidthtest&clientConfigId=id1");
        assert_eq!(with_plan_id("live_abc", "a b/c"), "live_abc?clientConfigId=a%20b%2Fc");
        assert_eq!(with_plan_id("live_abc", ""), "live_abc", "no plan id: the key as it was");
    }

    /// Connecting: the endpoint's own key when it gives one, RTMPS when asked and offered, the
    /// address without its `{stream_key}`.
    #[test]
    fn the_connection_uses_the_endpoints_own_key_and_address() {
        let p = read(PLAN).unwrap();
        let (server, key) = p.connect(true, "live_mine").unwrap();
        assert_eq!(server, "rtmps://ingest.example:443/app");
        assert_eq!(key, "live_service_key?clientConfigId=plan%207%2Fa", "the endpoint's key preferred");
        let (server, key) = p.connect(false, "live_mine").unwrap();
        assert_eq!(server, "rtmp://ingest.example/app");
        assert_eq!(key, "live_mine?clientConfigId=plan%207%2Fa", "no key from it: the person's");
    }
}
