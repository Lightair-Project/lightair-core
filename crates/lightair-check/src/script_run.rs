//! The script run (plugins.md §4.4): a script package's `main.js` in the same isolate the app
//! uses (`lightair-script`), against a fake Lightair that records every call. Every command,
//! hotkey and step it declares is invoked, its panels' elements used, its settings changed, and
//! chat and scene events fed in; any use outside the permissions it declared fails the check, as
//! does code that doesn't load, runs too long or uses too much memory.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightair_packages::Manifest;
use lightair_script::{Budget, FetchRequest, FetchResponse, Host, Package, Record, Script, State};
use serde_json::{Value, json};

use crate::report::Check;

/// A Lightair that answers every call with something plausible and keeps what was asked.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    fetched: Mutex<Vec<String>>,
    logs: Mutex<Vec<String>>,
}

impl Host for Fake {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(method.to_string());
        Ok(if method.ends_with(".list") || method.ends_with(".history") { json!([]) } else { json!({}) })
    }
    fn fetch(&self, req: &FetchRequest) -> Result<FetchResponse, String> {
        // nothing leaves the check machine; the address it asked for is what's checked
        self.fetched.lock().unwrap_or_else(|e| e.into_inner()).push(req.url.clone());
        Ok(FetchResponse { status: 200, headers: Default::default(), body: "{}".into() })
    }
    fn log(&self, level: &str, text: &str) {
        let mut l = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        if l.len() < 50 {
            l.push(format!("{level}: {text}"));
        }
    }
}

pub fn script(c: &mut Check, folder: &Path, rel: &str, manifest: &Manifest) {
    let main = folder.join("main.js");
    let code = match std::fs::read_to_string(&main) {
        Ok(code) => code,
        Err(_) => return c.fail("a script needs a main.js in its folder", Some(rel)),
    };
    let fake = Arc::new(Fake::default());
    let package = Package {
        id: manifest.id.to_string(),
        name: manifest.name.clone(),
        code,
        permissions: manifest.permissions.clone(),
        data_dir: None,
        settings: json!({}),
    };
    let script = Script::start(package, fake.clone(), Budget::default());
    if !settled(&script) {
        return c.fail("the script didn't finish loading in time", Some(rel));
    }
    match script.state() {
        State::Failed(e) => return c.fail(format!("the script didn't load: {e}"), Some(rel)),
        State::Suspended(why) => return c.fail(format!("the script was suspended while loading: {why}"), Some(rel)),
        _ => {}
    }

    // everything it says it adds, used once
    let d = script.declared();
    for cmd in d.commands.iter().chain(&d.hotkeys) {
        if let Err(e) = script.command(&cmd.id) {
            c.note(format!("its command \"{}\" failed: {e}", cmd.title), Some(rel));
        }
    }
    for step in &d.steps {
        let params: serde_json::Map<String, Value> =
            step.fields.iter().filter_map(|f| Some((f["key"].as_str()?.to_string(), sample(f)))).collect();
        if let Err(e) = script.step(&step.id, Value::Object(params)) {
            c.note(format!("its step \"{}\" failed: {e}", step.title), Some(rel));
        }
    }
    for panel in &d.panels {
        for el in panel.ui.as_array().into_iter().flatten() {
            let Some(id) = el["id"].as_str() else { continue };
            let value = match el["type"].as_str() {
                Some("toggle") => json!(true),
                Some("slider") => json!((el["min"].as_f64().unwrap_or(0.0) + el["max"].as_f64().unwrap_or(1.0)) / 2.0),
                Some("text") => json!("test"),
                Some("list") => el["items"].get(0).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            script.ui(&panel.id, id, value);
        }
    }
    if !d.settings.is_empty() {
        let values: serde_json::Map<String, Value> =
            d.settings.iter().filter_map(|f| Some((f["key"].as_str()?.to_string(), sample(f)))).collect();
        script.set_settings(Value::Object(values));
    }
    script.event(
        "chat",
        json!({"platform": "twitch", "kind": "message", "user": {"display_name": "Viewer", "login": "viewer"}, "text": "hello !count"}),
    );
    script.event("scene", json!({"name": "Scene 2"}));
    // its timers get a moment to fire
    std::thread::sleep(Duration::from_millis(1500));
    // a round trip, so the events above have been handled
    let _ = script.command("__lightair_check_settle");
    script.stop();

    let records: Vec<Record> = script.records();
    let mut refused = 0;
    for r in records.iter().filter(|r| !r.allowed) {
        refused += 1;
        if refused > 20 {
            break;
        }
        let msg = match r.permission.as_str() {
            "time" => format!("a callback ran over its time budget ({})", r.what),
            "refused" => format!("it called {}, which scripts can't use: {}", r.what, r.reason),
            "network" => format!("it tried to reach {}: {}", r.what, r.reason),
            p => format!("it used {} without declaring {p}", r.what),
        };
        c.fail(msg, Some(rel));
    }
    if let State::Suspended(why) = script.state() {
        c.fail(format!("the script was suspended: {why}"), Some(rel));
    }
    let calls = fake.calls.lock().unwrap_or_else(|e| e.into_inner()).len();
    let fetched = fake.fetched.lock().unwrap_or_else(|e| e.into_inner()).len();
    c.note(
        format!(
            "ran: {} command(s), {} step(s), {} panel(s), {} overlay(s); {calls} call(s) into Lightair, {fetched} request(s)",
            d.commands.len() + d.hotkeys.len(),
            d.steps.len(),
            d.panels.len(),
            d.overlays.len()
        ),
        Some(rel),
    );
    for o in &d.overlays {
        // an overlay's page is a widget folder (index.html, style.css, main.js, fields.json)
        if !folder.join(&o.page).join("index.html").is_file() {
            c.fail(format!("its overlay \"{}\" needs a folder with an index.html: {}", o.title, o.page), Some(rel));
        }
    }
}

/// Waits (a few seconds at most) for the script to finish loading.
fn settled(script: &Script) -> bool {
    for _ in 0..60 {
        if !matches!(script.state(), State::Starting) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A sample value for a declared setting or field.
fn sample(field: &Value) -> Value {
    if !field["default"].is_null() {
        return field["default"].clone();
    }
    match field["kind"].as_str() {
        Some("number") => json!(1),
        Some("toggle") => json!(true),
        Some("choice") => field["options"].get(0).cloned().unwrap_or(json!("")),
        _ => json!("test"),
    }
}
