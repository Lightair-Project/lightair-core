//! Devices and apps on this computer or network, for automation steps (docs/plans/
//! live-integrations.md, 2.8): VTube Studio (its local API: the first time, VTube Studio asks
//! whether Lightair may), Elgato Key Lights (their local HTTP API). No accounts, no approval.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tungstenite::Message;

use crate::net;

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where integrations keep what they're given (VTube Studio's token).
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

fn token_file() -> Option<PathBuf> {
    DIR.get().map(|d| d.join("vtube-studio.json"))
}

/// One VTube Studio connection at a time (its API answers in order).
static VTS: Mutex<()> = Mutex::new(());

fn vts_request(socket: &mut net::Socket, kind: &str, data: Value) -> Result<Value, String> {
    let id = format!("lightair-{}", crate::event::now_ms());
    let req = json!({"apiName": "VTubeStudioPublicAPI", "apiVersion": "1.0", "requestID": id, "messageType": kind, "data": data});
    socket.send(Message::text(req.to_string())).map_err(|e| e.to_string())?;
    // the token request waits for the person to answer in VTube Studio
    let wait = if kind == "AuthenticationTokenRequest" { 60 } else { 8 };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(wait) {
        let Some(Message::Text(t)) = net::read(socket)? else { continue };
        let v: Value = serde_json::from_str(t.as_str()).map_err(|e| e.to_string())?;
        if v["requestID"] != id.as_str() {
            continue;
        }
        if v["messageType"] == "APIError" {
            return Err(format!("VTube Studio: {}", v["data"]["message"].as_str().unwrap_or("refused")));
        }
        return Ok(v["data"].clone());
    }
    Err("VTube Studio didn't answer".into())
}

/// A connection to VTube Studio, signed in as Lightair (asking for a token the first time).
fn vts_connect() -> Result<net::Socket, String> {
    let port = std::env::var("LIGHTAIR_VTS_PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(8001);
    let mut socket = net::connect(&format!("ws://127.0.0.1:{port}"), Duration::from_millis(300))
        .map_err(|_| "VTube Studio isn't running (or its API is off: Settings › Start API)".to_string())?;
    let plugin = json!({"pluginName": "Lightair", "pluginDeveloper": "Lightair"});
    let saved = token_file().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v["token"].as_str().map(String::from));
    let token = match saved {
        Some(t) => t,
        None => {
            let v = vts_request(&mut socket, "AuthenticationTokenRequest", plugin.clone())?;
            let t = v["authenticationToken"].as_str().ok_or("VTube Studio gave no token (Allow Lightair in VTube Studio)")?.to_string();
            if let Some(f) = token_file() {
                let _ = std::fs::create_dir_all(f.parent().unwrap_or(&f));
                let _ = std::fs::write(f, json!({"token": t}).to_string());
            }
            t
        }
    };
    let mut auth = plugin;
    auth["authenticationToken"] = json!(token);
    let v = vts_request(&mut socket, "AuthenticationRequest", auth)?;
    if v["authenticated"].as_bool() != Some(true) {
        if let Some(f) = token_file() {
            let _ = std::fs::remove_file(f);
        }
        return Err("VTube Studio didn't accept Lightair: run it again and choose Allow".into());
    }
    Ok(socket)
}

/// A method for an automation step or remote client, if it's one of these:
/// `vts.hotkeys`, `vts.hotkey` {hotkey: name or id}, `vts.expression` {file, active},
/// `light.get` / `light.set` {address, on, brightness, temperature}.
pub fn call(method: &str, p: &Value) -> Option<Result<Value, String>> {
    Some(match method {
        "vts.hotkeys" | "vts.hotkey" | "vts.expression" => {
            let _one = VTS.lock();
            (|| {
                let mut s = vts_connect()?;
                let list = vts_request(&mut s, "HotkeysInCurrentModelRequest", json!({}))?;
                let out = match method {
                    "vts.hotkeys" => Ok(json!({"hotkeys": list["availableHotkeys"].as_array().map(|a| a.iter().map(|h| json!({"name": h["name"], "id": h["hotkeyID"], "type": h["type"]})).collect::<Vec<_>>()).unwrap_or_default()})),
                    "vts.hotkey" => {
                        let want = p["hotkey"].as_str().unwrap_or("");
                        let id = list["availableHotkeys"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|h| h["hotkeyID"] == want || h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(want)))
                            .and_then(|h| h["hotkeyID"].as_str().map(String::from))
                            .ok_or_else(|| format!("the model has no hotkey “{want}”"))?;
                        vts_request(&mut s, "HotkeyTriggerRequest", json!({"hotkeyID": id}))
                    }
                    _ => vts_request(&mut s, "ExpressionActivationRequest", json!({"expressionFile": p["file"], "active": p["active"].as_bool().unwrap_or(true)})),
                };
                let _ = s.close(None);
                out
            })()
        }
        "light.get" | "light.set" => (|| {
            let address = p["address"].as_str().filter(|a| !a.is_empty()).ok_or("the light's address is needed (its IP, from Elgato Control Center)")?;
            let host = if address.contains(':') { address.to_string() } else { format!("{address}:9123") };
            let url = format!("http://{host}/elgato/lights");
            let agent = net::agent();
            if method == "light.get" {
                return net::get_json(&agent, &url, &[]);
            }
            let mut light = json!({});
            if let Some(on) = p["on"].as_bool() {
                light["on"] = json!(on as u8);
            }
            if let Some(b) = p["brightness"].as_f64() {
                light["brightness"] = json!(b.clamp(0.0, 100.0) as u64);
            }
            // Kelvin (2900–7000) → Elgato's own scale (143–344)
            if let Some(k) = p["temperature"].as_f64() {
                light["temperature"] = json!((1_000_000.0 / k.clamp(2900.0, 7000.0)).round() as u64);
            }
            let body = json!({"numberOfLights": 1, "lights": [light]});
            let mut res = agent.put(&url).header("Content-Type", "application/json").send(body.to_string()).map_err(|e| e.to_string())?;
            let text = res.body_mut().read_to_string().unwrap_or_default();
            Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
        })(),
        _ => return None,
    })
}

/// Heart rate from Pulsoid (the person's own token from pulsoid.net › Keys): `on_bpm` with each
/// reading, until `stop`. Reconnects with a pause after a failure.
pub fn pulsoid(token: String, on_bpm: impl Fn(u32) + Send + 'static, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    let _ = std::thread::Builder::new().name("lightair-heart-rate".into()).spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let result = (|| -> Result<(), String> {
                let mut socket = net::connect(&format!("wss://dev.pulsoid.net/api/v1/data/real_time?access_token={token}"), Duration::from_millis(500))?;
                while !stop.load(Ordering::Acquire) {
                    let Some(Message::Text(t)) = net::read(&mut socket)? else { continue };
                    if let Ok(v) = serde_json::from_str::<Value>(t.as_str())
                        && let Some(bpm) = v["data"]["heart_rate"].as_u64()
                    {
                        on_bpm(bpm as u32);
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                tracing::info!(%e, "heart rate");
                crate::hub::sleep_unless(&stop, Duration::from_secs(15));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_methods_are_left_alone() {
        assert!(super::call("scene.switch", &serde_json::json!({})).is_none());
        assert!(super::call("light.set", &serde_json::json!({})).unwrap().is_err());
    }
}

#[cfg(test)]
mod vts_check {
    /// With LIGHTAIR_VTS_PORT set to a VTube Studio (or a stand-in): the whole sign-in and a hotkey.
    #[test]
    fn hotkey_against_a_running_vtube_studio() {
        if std::env::var("LIGHTAIR_VTS_PORT").is_err() {
            return;
        }
        super::set_dir(std::env::temp_dir().join(format!("lightair-vts-{}", std::process::id())));
        let list = super::call("vts.hotkeys", &serde_json::json!({})).unwrap().unwrap();
        assert_eq!(list["hotkeys"][0]["name"], "Wave");
        super::call("vts.hotkey", &serde_json::json!({"hotkey": "wave"})).unwrap().unwrap();
    }
}
