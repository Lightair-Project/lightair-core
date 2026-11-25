//! A phone's Studio controlled from a paired desktop or another phone in Remote:
//! lightair-core's secure server (TLS on 4456, pairing the person allows on the phone, a key per
//! device, mDNS with `role=studio-mobile`) with the session as its handler, for the methods that
//! make sense on a phone: scenes, the mix's tracks, the stream, the recording, clips, the
//! privacy cut, and the state they change.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use lightair_remote::{
    ApiError, Call, Config, DesktopInfo, DeviceRequest, Identity, Pairing, Reply, Scope,
    SecureConfig, Server,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::session::Session;

/// A device asking to pair, waiting for the person's answer on the phone.
#[derive(Debug, Clone, PartialEq)]
pub struct PairRequest {
    pub name: String,
    pub model: String,
    pub platform: String,
}

/// What the interface shows about remote control.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteStatus {
    pub on: bool,
    /// Paired devices' names.
    pub paired: Vec<String>,
    /// Connected now.
    pub connected: Vec<String>,
    pub pending: Option<PairRequest>,
}

/// The pairing waiting for the person's answer, and where the answer goes.
type Asking = Arc<Mutex<Option<(PairRequest, std::sync::mpsc::Sender<bool>)>>>;

pub struct Remote {
    server: Arc<Server>,
    _secure: lightair_remote::SecureListener,
    _advertiser: Option<lightair_remote::mdns::Advertiser>,
    pairing: Arc<Pairing>,
    info: DesktopInfo,
    port: u16,
    fingerprint: String,
    pending: Asking,
    stop: Arc<AtomicBool>,
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn identity(dir: &Path, id: &str) -> Result<Identity, String> {
    let (cert, key) = (dir.join("remote-cert.der"), dir.join("remote-key.der"));
    if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert), std::fs::read(&key)) {
        return Ok(Identity { cert_der, key_der });
    }
    // made once and kept: paired devices pin its fingerprint
    let made = Identity::generate(id)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(&cert, &made.cert_der).map_err(|e| e.to_string())?;
    std::fs::write(&key, &made.key_der).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600));
    }
    Ok(made)
}

fn phone_id(dir: &Path) -> String {
    let p = dir.join("remote-id");
    if let Ok(id) = std::fs::read_to_string(&p)
        && !id.trim().is_empty()
    {
        return id.trim().to_string();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::write(&p, &id);
    id
}

/// What a method does on the session (`None`: the session's gone).
fn answer(session: &Session, call: &Call) -> Reply {
    let p = &call.params;
    let st = session.state();
    let ws = &st.workspace;
    let id_of = |v: &Value| -> Option<lightair_model::Id> {
        v.as_str()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(lightair_model::Id)
    };
    match call.method.as_str() {
        "scene.list" => Ok(json!({
            "scenes": ws.scenes.iter().map(|s| json!({
                "id": s.id.to_string(), "name": s.name, "program": s.id == st.program,
            })).collect::<Vec<_>>(),
        })),
        "scene.switch" => {
            let want = &p["scene"];
            let scene = ws
                .scenes
                .iter()
                .find(|s| id_of(want) == Some(s.id) || want.as_str() == Some(s.name.as_str()))
                .ok_or_else(|| ApiError::not_found("no such scene"))?;
            session.switch_scene(scene.id);
            Ok(json!({}))
        }
        "audio.list" => Ok(json!({
            "tracks": ws.tracks.iter().map(|t| json!({
                "id": t.id.to_string(), "name": t.name, "muted": t.muted, "volume_db": t.volume_db,
            })).collect::<Vec<_>>(),
        })),
        "audio.set" => {
            let want = &p["track"];
            let track = ws
                .tracks
                .iter()
                .find(|t| id_of(want) == Some(t.id) || want.as_str() == Some(t.name.as_str()))
                .ok_or_else(|| ApiError::not_found("no such track"))?;
            if let Some(m) = p["muted"].as_bool() {
                session.set_track_muted(track.id, m);
            }
            if let Some(db) = p["volume_db"].as_f64() {
                session.set_track_gain(track.id, db as f32);
            }
            Ok(json!({}))
        }
        "stream.start" => {
            let dests = session.remote_destinations();
            if dests.is_empty() {
                return Err(ApiError::state("no destinations are set up on the phone"));
            }
            session.go_live(dests, false);
            Ok(json!({}))
        }
        "stream.stop" => {
            session.end_live();
            Ok(json!({}))
        }
        "record.start" => {
            session.start_recording(false);
            Ok(json!({}))
        }
        "record.stop" => {
            session.stop_recording();
            Ok(json!({}))
        }
        "clip.save" => {
            let seconds = p["seconds"].as_u64().unwrap_or(30) as u32;
            session
                .save_clip(seconds)
                .map(|path| json!({ "path": path.to_string_lossy() }))
                .map_err(ApiError::state)
        }
        "panic.on" | "panic.off" => {
            session.set_privacy_cut(call.method == "panic.on");
            Ok(json!({}))
        }
        "state" => Ok(state(session)),
        // the desktop's API, as a phone's Remote mode reads it
        "state.get" => Ok(desktop_state(session)),
        "stats.get" => {
            let fps = ws
                .canvas(st.canvas)
                .map_or(30, |c| c.fps_num / c.fps_den.max(1));
            Ok(json!({ "fps": fps }))
        }
        "outputs.list" => Ok(json!({ "destinations": destinations(session) })),
        "panic.get" => Ok(json!({ "muted_all": all_muted(session), "privacy": st.privacy })),
        "panic.privacy" => {
            session.set_privacy_cut(p["on"].as_bool().unwrap_or(true));
            Ok(json!({}))
        }
        "panic.mute_all" => {
            let on = p["on"].as_bool().unwrap_or(true);
            for t in &ws.tracks {
                session.set_track_muted(t.id, on);
            }
            Ok(json!({}))
        }
        "panic.stop_all" => {
            session.end_live();
            session.stop_recording();
            Ok(json!({}))
        }
        "preview.frame" => {
            use base64::Engine as _;
            let max_width = p["max_width"].as_u64().unwrap_or(720).clamp(16, 1920) as u32;
            let canvas = id_of(&p["canvas"]);
            let (jpeg, width, height) = session
                .program_jpeg(max_width, canvas)
                .ok_or_else(|| ApiError::state("no picture yet"))?;
            Ok(json!({
                "mime": "image/jpeg",
                "width": width,
                "height": height,
                "data": base64::engine::general_purpose::STANDARD.encode(jpeg),
            }))
        }
        _ => Err(ApiError::new(
            lightair_remote::ErrorKind::Unknown,
            "not on a phone",
        )),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn all_muted(session: &Session) -> bool {
    let st = session.state();
    !st.workspace.tracks.is_empty() && st.workspace.tracks.iter().all(|t| t.muted)
}

/// The destinations as the desktop's `outputs.list` gives them.
fn destinations(session: &Session) -> Vec<Value> {
    session
        .hot()
        .outputs
        .read()
        .iter()
        .map(|o| {
            let h = o.health();
            let state = match h.state {
                crate::outputs::OutputState::Connecting => "connecting",
                crate::outputs::OutputState::Live => "live",
                crate::outputs::OutputState::Congested => "congested",
                crate::outputs::OutputState::Reconnecting { .. } => "reconnecting",
                crate::outputs::OutputState::Failed { .. } => "failed",
            };
            json!({
                "id": h.id, "name": h.name, "enabled": true, "state": state,
                "bitrate_kbps": h.sent_kbps, "dropped_pct": 0.0, "message": h.message,
            })
        })
        .collect()
}

/// The workspace and outputs in the shape of the desktop's `state.get`, so a phone in Remote
/// shows a phone's Studio as it shows a desktop.
fn desktop_state(session: &Session) -> Value {
    let st = session.state();
    let ws = &st.workspace;
    let now = now_ms();
    let live_s = match st.live {
        crate::host::LiveState::Live { since_ms } => {
            Some(now.saturating_sub(since_ms) as f64 / 1000.0)
        }
        _ => None,
    };
    let rec_s = match &st.recording {
        crate::host::RecordingState::Recording { since_ms, .. } => {
            Some(now.saturating_sub(*since_ms) as f64 / 1000.0)
        }
        _ => None,
    };
    json!({
        "canvases": ws.canvases.iter().map(|c| json!({
            "id": c.id.to_string(), "name": c.name, "width": c.width, "height": c.height,
        })).collect::<Vec<_>>(),
        "program": { "id": st.program.to_string() },
        "scenes": ws.scenes.iter().map(|s| json!({
            "id": s.id.to_string(),
            "name": s.name,
            "objects": s.objects.iter().map(|o| {
                let kind = o
                    .source
                    .and_then(|id| ws.sources.get(&id))
                    .map_or("", |s| s.kind.as_str());
                json!({
                    "id": o.id.to_string(), "name": o.name, "kind": kind,
                    "visible": o.layouts.get(&st.canvas).is_none_or(|l| l.visible),
                    "locked": o.locked,
                    "source": o.source.map(|s| s.to_string()),
                    "parent": o.parent.map(|p| p.to_string()),
                })
            }).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "sources": ws.sources.values().map(|s| json!({
            "id": s.id.to_string(), "kind": s.kind,
        })).collect::<Vec<_>>(),
        "tracks": ws.tracks.iter().map(|t| json!({
            "id": t.id.to_string(), "name": t.name, "source": t.source.to_string(),
            "volume_db": t.volume_db, "muted": t.muted,
        })).collect::<Vec<_>>(),
        "outputs": {
            "recording": { "active": rec_s.is_some(), "seconds": rec_s.unwrap_or(0.0), "paused": false },
            "streaming": {
                "active": live_s.is_some(),
                "destinations": destinations(session).into_iter().map(|mut d| {
                    d["active"] = json!(live_s.is_some());
                    d["seconds"] = json!(live_s.unwrap_or(0.0));
                    d
                }).collect::<Vec<_>>(),
            },
            "virtual_camera": { "active": false },
        },
    })
}

/// The state a controller shows (sent as `state.changed` when it changes).
fn state(session: &Session) -> Value {
    let st = session.state();
    json!({
        "scene": st.program.to_string(),
        "live": !matches!(st.live, crate::host::LiveState::Off),
        "recording": !matches!(st.recording, crate::host::RecordingState::Idle),
        "privacy": st.privacy,
    })
}

impl Remote {
    /// Starts the server for `session`: its keys and paired devices kept in `dir`, `name` the
    /// phone's (what controllers show).
    pub fn start(
        session: &Arc<Session>,
        dir: &Path,
        name: &str,
        port: u16,
    ) -> Result<Remote, String> {
        let id = phone_id(dir);
        let identity = identity(dir, &id)?;
        let fingerprint = identity.fingerprint_b64();
        let short = identity.short_fingerprint();
        let info = DesktopInfo {
            id,
            name: name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let weak: Weak<Session> = Arc::downgrade(session);
        let handler = Arc::new(move |call: Call| -> Reply {
            let s = weak
                .upgrade()
                .ok_or_else(|| ApiError::state("Studio is closed"))?;
            answer(&s, &call)
        });
        // the plain server stays on this phone only; controllers come in over TLS
        let server = Server::start(
            Config {
                port: 0,
                lan: false,
                password: Some(lightair_remote::new_password()),
            },
            handler,
        )
        .map_err(|e| e.to_string())?;
        let server = Arc::new(server);
        let devices_file: PathBuf = dir.join("remote-devices.json");
        let devices = std::fs::read_to_string(&devices_file)
            .map(|t| lightair_remote::pairing::devices_from_json(&t))
            .unwrap_or_default();
        let pending: Asking = Default::default();
        let asking = pending.clone();
        let pairing = Pairing::new(
            devices,
            move |r: &DeviceRequest| {
                // asks the person on the phone (remote_answer), a minute at most
                let (tx, rx) = std::sync::mpsc::channel();
                *asking.lock() = Some((
                    PairRequest {
                        name: r.name.clone(),
                        model: r.model.clone(),
                        platform: r.platform.clone(),
                    },
                    tx,
                ));
                let yes = rx.recv_timeout(Duration::from_secs(60)).unwrap_or(false);
                *asking.lock() = None;
                yes
            },
            move |list| {
                let _ = std::fs::write(
                    &devices_file,
                    lightair_remote::pairing::devices_to_json(list),
                );
            },
        );
        let secure = server
            .start_secure(
                SecureConfig {
                    port,
                    identity,
                    desktop: info.clone(),
                    features: vec![
                        "scenes".into(),
                        "audio".into(),
                        "stream".into(),
                        "panic".into(),
                        "clips".into(),
                        // what a phone's Remote mode looks for (as from a desktop)
                        "preview.jpeg".into(),
                        "audio.levels".into(),
                        "outputs".into(),
                    ],
                },
                pairing.clone(),
            )
            .map_err(|e| e.to_string())?;
        let port = secure.addr().port();
        let advertiser = lightair_remote::mdns::Advertiser::start_as(
            info.clone(),
            port,
            &short,
            "studio-mobile",
        )
        .map_err(|e| tracing::warn!("not advertised on the network: {e}"))
        .ok();
        // the state, to every controller, when it changes
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (stop, weak, server) = (stop.clone(), Arc::downgrade(session), server.clone());
            let _ = std::thread::Builder::new()
                .name("lightair-remote-state".into())
                .spawn(move || {
                    let (mut last, mut tick) = (Value::Null, 0u32);
                    while !stop.load(Ordering::Acquire) {
                        let Some(s) = weak.upgrade() else { break };
                        // meters ten times a second, as the desktop's `audio.levels`
                        let levels: Vec<Value> = s
                            .hot()
                            .levels
                            .read()
                            .iter()
                            .map(|l| {
                                json!({
                                    "track": l.track.to_string(),
                                    "peak": [l.peak_db, l.peak_db],
                                    "rms": [l.rms_db, l.rms_db],
                                })
                            })
                            .collect();
                        let now = state(&s);
                        drop(s);
                        if !levels.is_empty() {
                            server.emit("audio.levels", json!({ "tracks": levels }));
                        }
                        tick += 1;
                        if now != last && tick % 3 == 0 {
                            server.emit("state.changed", now.clone());
                            // the desktop's events a phone's Remote mode listens for
                            if now["scene"] != last["scene"] {
                                server.emit("scene.program", json!({ "id": now["scene"] }));
                            }
                            if now["live"] != last["live"] {
                                server.emit("stream.state", json!({ "active": now["live"] }));
                            }
                            if now["recording"] != last["recording"] {
                                server.emit("record.state", json!({ "active": now["recording"] }));
                            }
                            if now["privacy"] != last["privacy"] {
                                server.emit(
                                    "panic.state",
                                    json!({ "privacy": now["privacy"], "muted_all": false }),
                                );
                            }
                            last = now;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
        }
        Ok(Remote {
            server,
            _secure: secure,
            _advertiser: advertiser,
            pairing,
            info,
            port,
            fingerprint,
            pending,
            stop,
        })
    }

    /// A one-time pairing link for a controller (the QR code; 2 minutes, one use).
    pub fn offer(&self, addresses: &[String]) -> String {
        let offer = self.pairing.offer(Scope::Control);
        self.pairing
            .link(&offer, &self.info, addresses, self.port, &self.fingerprint)
    }

    pub fn status(&self) -> RemoteStatus {
        RemoteStatus {
            on: true,
            paired: self.pairing.devices().into_iter().map(|d| d.name).collect(),
            connected: self
                .server
                .clients()
                .into_iter()
                .filter(|c| c.protocol != "JSON-RPC" || !c.peer.starts_with("127."))
                .map(|c| c.peer)
                .collect(),
            pending: self.pending.lock().as_ref().map(|(r, _)| r.clone()),
        }
    }

    /// The person's answer to the device asking to pair.
    pub fn answer(&self, accept: bool) {
        if let Some((_, tx)) = self.pending.lock().take() {
            let _ = tx.send(accept);
        }
    }

    /// Forgets a paired device (its key stops working at once).
    pub fn forget(&self, name: &str) {
        for d in self
            .pairing
            .devices()
            .into_iter()
            .filter(|d| d.name == name)
        {
            self.pairing.remove(&d.id);
        }
    }
}
