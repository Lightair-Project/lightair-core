//! Script plugins (tier 3 `extension` packages, docs/13 stage D): one JavaScript isolate per
//! package (QuickJS, through rquickjs) on its own thread.
//!
//! - **Every call into Lightair is checked** against the package's granted permissions
//!   ([`permissions`]): Lightair's methods (the remote API's names), events, fetch (declared
//!   hosts only, through the host's own fetch), storage (per package, with a quota), files the
//!   user picked, notices. Each use, allowed or refused, is reported to the [`Host`].
//! - **Budgets:** a memory cap for the isolate; each callback (a command, a step, an event, a
//!   timer) gets a time budget for its JavaScript (time spent waiting on Lightair doesn't
//!   count). A callback over budget is stopped; a script that runs over again soon after, or runs
//!   out of memory, is suspended and the host is told why.
//! - **What a script adds** ([`Declared`]): commands, hotkeys, automation steps, settings, panels
//!   described as data (drawn natively by the app), and overlay pages with a message channel.
//!
//! The same isolate runs in `lightair-check`, against a host that records every call.

pub mod permissions;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use permissions::{Needs, for_event, for_method, may_fetch};

const PRELUDE: &str = include_str!("prelude.js");

/// A command or hotkey a script adds.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Decl {
    pub id: String,
    pub title: String,
}

/// An automation step a script adds; `fields` as the helper protocol's settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StepDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub fields: Vec<Value>,
}

/// A panel: an id, a title, and its contents as a list of elements (script-api.md, "Panels").
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PanelDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub ui: Value,
}

/// An overlay object kind: a page in the package the browser object draws.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct OverlayDecl {
    pub id: String,
    pub title: String,
    /// Its page: a widget folder (index.html, style.css, main.js, fields.json), relative to the
    /// script's folder.
    pub page: String,
    pub width: u32,
    pub height: u32,
}

/// Everything a script has said it adds.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Declared {
    pub commands: Vec<Decl>,
    pub hotkeys: Vec<Decl>,
    pub steps: Vec<StepDecl>,
    /// Settings in the helper protocol's shape (`key`, `label`, `kind`, `default`, `options`).
    pub settings: Vec<Value>,
    pub panels: Vec<PanelDecl>,
    pub overlays: Vec<OverlayDecl>,
}

/// A request a script asked Lightair to fetch for it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// The answer.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// How a script is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Starting,
    Running,
    /// Stopped by Lightair for running too long or out of memory; why, in a sentence.
    Suspended(String),
    /// Its code didn't load; why.
    Failed(String),
    Stopped,
}

/// One use a script made (or tried), for `lightair-check` and the logs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Record {
    /// The permission it took (`project.write`, `network`, …), or what was refused.
    pub permission: String,
    pub what: String,
    pub allowed: bool,
    pub reason: String,
}

/// What a script reaches through. Calls arrive already checked against its permissions.
pub trait Host: Send + Sync + 'static {
    /// One of Lightair's methods.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
    /// A request to a host it declared.
    fn fetch(&self, req: &FetchRequest) -> Result<FetchResponse, String>;
    /// A use, allowed or refused (the activity panel and its notices).
    fn record(&self, _r: &Record) {}
    /// What it adds changed.
    fn declared(&self, _d: &Declared) {}
    /// A panel's contents changed.
    fn panel_update(&self, _panel: &str, _ui: &Value) {}
    /// A message for one of its overlays.
    fn overlay_send(&self, _overlay: &str, _message: &Value) {}
    /// A notice from it (it holds `notifications`).
    fn notify(&self, _text: &str) {}
    /// The user picks a folder or file for it (it holds `fs.read:picked`); `None` when cancelled.
    fn pick(&self) -> Option<PathBuf> {
        None
    }
    fn log(&self, _level: &str, _text: &str) {}
    /// Its state changed (started, suspended, failed).
    fn state(&self, _s: &State) {}
}

/// A script package to run.
#[derive(Clone, Debug)]
pub struct Package {
    pub id: String,
    pub name: String,
    /// Its `main.js`.
    pub code: String,
    /// Granted permissions (declared minus taken back).
    pub permissions: Vec<String>,
    /// Its own folder for storage; `None` keeps storage in memory (the checker).
    pub data_dir: Option<PathBuf>,
    /// Its settings' current values.
    pub settings: Value,
}

/// Limits for one isolate.
#[derive(Clone, Debug)]
pub struct Budget {
    /// JavaScript time per callback.
    pub per_call: Duration,
    /// JavaScript time to load its code.
    pub start: Duration,
    /// The isolate's memory.
    pub memory: usize,
    /// Its storage (serialised).
    pub storage: usize,
    /// Over-budget callbacks within a minute before it's suspended.
    pub overruns: usize,
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            per_call: Duration::from_millis(250),
            start: Duration::from_secs(2),
            memory: 64 << 20,
            storage: 1 << 20,
            overruns: 2,
        }
    }
}

enum Job {
    Dispatch { msg: Value, reply: Option<Sender<Result<Value, String>>> },
    Stop,
}

struct Shared {
    state: Mutex<State>,
    declared: Mutex<Declared>,
    records: Mutex<Vec<Record>>,
    settings: Mutex<Value>,
}

/// A running script. Dropping it stops it.
pub struct Script {
    tx: Sender<Job>,
    shared: Arc<Shared>,
    next: AtomicU64,
}

impl Script {
    /// Starts `package`'s isolate on its own thread.
    pub fn start(package: Package, host: Arc<dyn Host>, budget: Budget) -> Script {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::Starting),
            declared: Mutex::new(Declared::default()),
            records: Mutex::new(Vec::new()),
            settings: Mutex::new(package.settings.clone()),
        });
        let s2 = shared.clone();
        let name = package.name.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("script {name}"))
            .stack_size(8 << 20)
            .spawn(move || run(package, host, budget, rx, s2));
        if let Err(e) = spawned {
            *shared.state.lock().unwrap_or_else(|e| e.into_inner()) = State::Failed(e.to_string());
        }
        Script { tx, shared, next: AtomicU64::new(1) }
    }

    pub fn state(&self) -> State {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn declared(&self) -> Declared {
        self.shared.declared.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Every use it made or tried so far.
    pub fn records(&self) -> Vec<Record> {
        self.shared.records.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn ask(&self, mut msg: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        msg["id"] = json!(id);
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.tx.send(Job::Dispatch { msg, reply: Some(tx) }).map_err(|_| "the script has stopped".to_string())?;
        rx.recv_timeout(timeout).unwrap_or_else(|_| Err("the script didn't answer in time".into()))
    }

    fn tell(&self, msg: Value) {
        let _ = self.tx.send(Job::Dispatch { msg, reply: None });
    }

    /// Runs one of its commands (or hotkeys).
    pub fn command(&self, command: &str) -> Result<Value, String> {
        self.ask(json!({"type": "command", "command": command}), Duration::from_secs(20))
    }

    /// Runs one of its automation steps.
    pub fn step(&self, step: &str, params: Value) -> Result<Value, String> {
        self.ask(json!({"type": "step", "step": step, "params": params}), Duration::from_secs(30))
    }

    /// An event it subscribed to (`chat`, `scene`, …).
    pub fn event(&self, event: &str, data: Value) {
        self.tell(json!({"type": "event", "event": event, "data": data}));
    }

    /// Someone used an element of one of its panels.
    pub fn ui(&self, panel: &str, element: &str, value: Value) {
        self.tell(json!({"type": "ui", "panel": panel, "element": element, "value": value}));
    }

    /// A message from one of its overlay pages.
    pub fn overlay_message(&self, overlay: &str, message: Value) {
        self.tell(json!({"type": "overlay", "overlay": overlay, "message": message}));
    }

    /// Its settings changed.
    pub fn set_settings(&self, values: Value) {
        *self.shared.settings.lock().unwrap_or_else(|e| e.into_inner()) = values.clone();
        self.tell(json!({"type": "settings", "values": values}));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Job::Stop);
    }
}

impl Drop for Script {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The isolate's own state, on its thread.
struct Inner {
    package: Package,
    host: Arc<dyn Host>,
    budget: Budget,
    shared: Arc<Shared>,
    /// Microseconds since `base` when the current callback's budget ends (`i64::MAX`: none).
    deadline: Arc<AtomicI64>,
    base: Instant,
    replies: RefCell<HashMap<u64, Result<Value, String>>>,
    timers: RefCell<BTreeMap<(Instant, u64), (u64, bool)>>,
    storage: RefCell<Option<serde_json::Map<String, Value>>>,
    picked: RefCell<Vec<PathBuf>>,
}

impl Inner {
    fn has(&self, permission: &str) -> bool {
        self.package.permissions.iter().any(|p| p == permission)
    }

    fn record(&self, permission: &str, what: &str, allowed: bool, reason: &str) {
        let r = Record { permission: permission.into(), what: what.into(), allowed, reason: reason.into() };
        self.host.record(&r);
        let mut list = self.shared.records.lock().unwrap_or_else(|e| e.into_inner());
        if list.len() < 10_000 {
            list.push(r);
        }
    }

    fn refuse(&self, permission: &str, what: &str) -> Result<Value, String> {
        self.record(permission, what, false, "not granted");
        Err(format!("This plugin doesn't have the permission {permission}."))
    }

    fn now(&self) -> i64 {
        self.base.elapsed().as_micros() as i64
    }

    fn set_budget(&self, d: Option<Duration>) {
        self.deadline.store(d.map_or(i64::MAX, |d| self.now() + d.as_micros() as i64), Ordering::Relaxed);
    }

    /// One native operation from the prelude.
    fn op(&self, name: &str, arg: Value) -> Result<Value, String> {
        match name {
            "call" => {
                let method = arg["method"].as_str().unwrap_or_default().to_string();
                match for_method(&method) {
                    Needs::Refused(why) => {
                        self.record("refused", &method, false, why);
                        Err(format!("{method}: {why}."))
                    }
                    Needs::Permission(p) if !self.has(p) => self.refuse(p, &method),
                    Needs::Permission(p) => {
                        self.record(p, &method, true, "");
                        self.host.call(&method, arg.get("params").cloned().unwrap_or_else(|| json!({})))
                    }
                }
            }
            "subscribe" => {
                let event = arg["event"].as_str().unwrap_or_default().to_string();
                match for_event(&event) {
                    Needs::Refused(why) => Err(format!("{event}: {why}.")),
                    Needs::Permission(p) if !self.has(p) => self.refuse(p, &format!("{event} events")),
                    Needs::Permission(p) => {
                        self.record(p, &format!("{event} events"), true, "");
                        Ok(Value::Null)
                    }
                }
            }
            "fetch" => {
                let req: FetchRequest = serde_json::from_value(arg).map_err(|e| e.to_string())?;
                if let Err(why) = may_fetch(&self.package.permissions, &req.url) {
                    self.record("network", &req.url, false, &why);
                    return Err(format!("This plugin can't reach {}: {why}.", req.url));
                }
                self.record("network", &req.url, true, "");
                let r = self.host.fetch(&req)?;
                Ok(serde_json::to_value(r).unwrap_or(Value::Null))
            }
            "storage.get" | "storage.set" => self.storage(name, &arg),
            "files.pick" => {
                if !self.has("fs.read:picked") {
                    return self.refuse("fs.read:picked", "a file or folder");
                }
                match self.host.pick() {
                    Some(path) => {
                        let real = std::fs::canonicalize(&path).unwrap_or(path);
                        self.record("fs.read:picked", &real.to_string_lossy(), true, "");
                        self.picked.borrow_mut().push(real.clone());
                        Ok(json!(real.to_string_lossy()))
                    }
                    None => Ok(Value::Null),
                }
            }
            "files.read" => {
                let path = PathBuf::from(arg["path"].as_str().unwrap_or_default());
                let real = std::fs::canonicalize(&path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
                if !self.picked.borrow().iter().any(|p| real.starts_with(p)) {
                    self.record("fs.read:picked", &real.to_string_lossy(), false, "not picked by the user");
                    return Err("Only files the user picked can be read.".into());
                }
                let meta = std::fs::metadata(&real).map_err(|e| e.to_string())?;
                if meta.len() > 5 << 20 {
                    return Err("That file is over 5 MB.".into());
                }
                let bytes = std::fs::read(&real).map_err(|e| e.to_string())?;
                Ok(json!(String::from_utf8_lossy(&bytes)))
            }
            "notify" => {
                if !self.has("notifications") {
                    return self.refuse("notifications", "a notice");
                }
                let text: String = arg["text"].as_str().unwrap_or_default().chars().take(200).collect();
                self.record("notifications", &text, true, "");
                self.host.notify(&text);
                Ok(Value::Null)
            }
            "declare" => self.declare(&arg),
            "panel.update" => {
                let id = arg["id"].as_str().unwrap_or_default().to_string();
                let ui = arg.get("ui").cloned().unwrap_or(Value::Null);
                if let Some(p) = self
                    .shared
                    .declared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .panels
                    .iter_mut()
                    .find(|p| p.id == id)
                {
                    p.ui = ui.clone();
                }
                self.host.panel_update(&id, &ui);
                Ok(Value::Null)
            }
            "overlay.send" => {
                self.host
                    .overlay_send(arg["id"].as_str().unwrap_or_default(), arg.get("message").unwrap_or(&Value::Null));
                Ok(Value::Null)
            }
            "settings.get" => Ok(self.shared.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            "log" => {
                let text: String = arg["text"].as_str().unwrap_or_default().chars().take(2000).collect();
                self.host.log(arg["level"].as_str().unwrap_or("info"), &text);
                Ok(Value::Null)
            }
            "timer" => {
                let id = arg["id"].as_u64().unwrap_or(0);
                let ms = arg["ms"].as_u64().unwrap_or(0).min(86_400_000);
                let repeat = arg["repeat"].as_bool().unwrap_or(false);
                let mut timers = self.timers.borrow_mut();
                if timers.len() >= 1000 {
                    return Err("too many timers".into());
                }
                timers.insert((Instant::now() + Duration::from_millis(ms), id), (ms, repeat));
                Ok(Value::Null)
            }
            "timer.clear" => {
                let id = arg["id"].as_u64().unwrap_or(0);
                self.timers.borrow_mut().retain(|(_, t), _| *t != id);
                Ok(Value::Null)
            }
            "reply" => {
                let id = arg["id"].as_u64().unwrap_or(0);
                let r = match arg.get("err") {
                    Some(e) => Err(e.as_str().unwrap_or("it failed").to_string()),
                    None => Ok(arg.get("ok").cloned().unwrap_or(Value::Null)),
                };
                self.replies.borrow_mut().insert(id, r);
                Ok(Value::Null)
            }
            other => Err(format!("unknown operation {other}")),
        }
    }

    fn declare(&self, arg: &Value) -> Result<Value, String> {
        let kind = arg["kind"].as_str().unwrap_or_default();
        let decl = arg.get("decl").cloned().unwrap_or(Value::Null);
        if kind == "hotkey" && !self.has("hotkeys") {
            return self.refuse("hotkeys", "a shortcut");
        }
        let bad = |e: serde_json::Error| e.to_string();
        let snapshot = {
            let mut d = self.shared.declared.lock().unwrap_or_else(|e| e.into_inner());
            let id = decl["id"].as_str().unwrap_or_default().to_string();
            match kind {
                "command" => upsert(&mut d.commands, serde_json::from_value(decl).map_err(bad)?, &id, |c| &c.id),
                "hotkey" => upsert(&mut d.hotkeys, serde_json::from_value(decl).map_err(bad)?, &id, |c| &c.id),
                "step" => upsert(&mut d.steps, serde_json::from_value(decl).map_err(bad)?, &id, |c| &c.id),
                "panel" => upsert(&mut d.panels, serde_json::from_value(decl).map_err(bad)?, &id, |c| &c.id),
                "overlay" => {
                    let o: OverlayDecl = serde_json::from_value(decl).map_err(bad)?;
                    if o.page.contains("..") || o.page.starts_with('/') || o.page.contains("://") {
                        return Err("An overlay's page must be a file inside the package.".into());
                    }
                    upsert(&mut d.overlays, o, &id, |c| &c.id)
                }
                "settings" => d.settings = decl.as_array().cloned().unwrap_or_default(),
                _ => return Err(format!("nothing to declare as {kind}")),
            }
            d.clone()
        };
        self.host.declared(&snapshot);
        Ok(Value::Null)
    }

    fn storage_file(&self) -> Option<PathBuf> {
        self.package.data_dir.as_ref().map(|d| d.join("storage.json"))
    }

    fn storage(&self, name: &str, arg: &Value) -> Result<Value, String> {
        let mut slot = self.storage.borrow_mut();
        let map = slot.get_or_insert_with(|| {
            self.storage_file()
                .and_then(|f| std::fs::read(f).ok())
                .and_then(|b| serde_json::from_slice::<serde_json::Map<String, Value>>(&b).ok())
                .unwrap_or_default()
        });
        let key = arg["key"].as_str().unwrap_or_default().to_string();
        if name == "storage.get" {
            return Ok(map.get(&key).cloned().unwrap_or(Value::Null));
        }
        let value = arg.get("value").cloned().unwrap_or(Value::Null);
        let before = map.get(&key).cloned();
        if value.is_null() {
            map.remove(&key);
        } else {
            map.insert(key.clone(), value);
        }
        let bytes = serde_json::to_vec(&*map).unwrap_or_default();
        if bytes.len() > self.budget.storage {
            match before {
                Some(b) => map.insert(key, b),
                None => map.remove(&key),
            };
            return Err(format!("This plugin's storage is full ({} KB at most).", self.budget.storage >> 10));
        }
        if let Some(file) = self.storage_file() {
            write_atomic(&file, &bytes).map_err(|e| format!("couldn't save: {e}"))?;
        }
        Ok(Value::Null)
    }
}

fn upsert<T>(list: &mut Vec<T>, item: T, id: &str, key: impl Fn(&T) -> &String) {
    match list.iter_mut().find(|x| key(x) == id) {
        Some(x) => *x = item,
        None => list.push(item),
    }
}

fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, file)
}

fn set_state(inner: &Inner, s: State) {
    *inner.shared.state.lock().unwrap_or_else(|e| e.into_inner()) = s.clone();
    inner.host.state(&s);
}

fn run(package: Package, host: Arc<dyn Host>, budget: Budget, rx: Receiver<Job>, shared: Arc<Shared>) {
    let deadline = Arc::new(AtomicI64::new(i64::MAX));
    let base = Instant::now();
    let inner = Rc::new(Inner {
        package,
        host,
        budget,
        shared,
        deadline: deadline.clone(),
        base,
        replies: RefCell::new(HashMap::new()),
        timers: RefCell::new(BTreeMap::new()),
        storage: RefCell::new(None),
        picked: RefCell::new(Vec::new()),
    });

    let rt = match rquickjs::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return set_state(&inner, State::Failed(format!("the script engine didn't start: {e}"))),
    };
    rt.set_memory_limit(inner.budget.memory);
    rt.set_max_stack_size(1 << 20);
    {
        let d = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            let end = d.load(Ordering::Relaxed);
            end != i64::MAX && base.elapsed().as_micros() as i64 > end
        })));
    }
    let ctx = match rquickjs::Context::full(&rt) {
        Ok(c) => c,
        Err(e) => return set_state(&inner, State::Failed(format!("the script engine didn't start: {e}"))),
    };

    // the prelude, then the package's code
    let loaded: Result<(), String> = ctx.with(|ctx| {
        let me = inner.clone();
        let native = rquickjs::Function::new(ctx.clone(), move |name: String, arg: String| -> String {
            let t0 = Instant::now();
            let arg: Value = serde_json::from_str(&arg).unwrap_or(Value::Null);
            let out = me.op(&name, arg);
            // time waiting on Lightair isn't the script's
            let waited = t0.elapsed().as_micros() as i64;
            let d = me.deadline.load(Ordering::Relaxed);
            if d != i64::MAX {
                me.deadline.store(d.saturating_add(waited), Ordering::Relaxed);
            }
            match out {
                Ok(v) => json!({"ok": v}).to_string(),
                Err(e) => json!({"err": e}).to_string(),
            }
        })
        .map_err(|e| e.to_string())?;
        ctx.globals().set("__host", native).map_err(|e| e.to_string())?;
        ctx.eval::<(), _>(PRELUDE).map_err(|e| exception(&ctx, e))?;
        inner.set_budget(Some(inner.budget.start));
        let r = ctx.eval::<(), _>(inner.package.code.as_bytes().to_vec()).map_err(|e| exception(&ctx, e));
        drain(&ctx);
        r
    });
    inner.set_budget(None);
    if let Err(e) = loaded {
        let s = if is_overrun(&e) {
            State::Suspended("it took too long to start".into())
        } else if is_oom(&e) {
            State::Suspended("it used too much memory".into())
        } else {
            State::Failed(e)
        };
        set_state(&inner, s);
        // keep answering until stopped, so callers don't wait
        while let Ok(job) = rx.recv() {
            match job {
                Job::Stop => break,
                Job::Dispatch { reply: Some(tx), .. } => {
                    let _ = tx.send(Err("the script isn't running".into()));
                }
                Job::Dispatch { .. } => {}
            }
        }
        return;
    }
    set_state(&inner, State::Running);

    let mut waiting: HashMap<u64, Sender<Result<Value, String>>> = HashMap::new();
    let mut overruns: Vec<Instant> = Vec::new();
    loop {
        // the next timer due, or the next message
        let next_due = inner.timers.borrow().keys().next().map(|(t, _)| *t);
        let job = match next_due {
            Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(j) => Some(j),
                Err(_) => break,
            },
        };
        let suspended = matches!(&*inner.shared.state.lock().unwrap_or_else(|e| e.into_inner()), State::Suspended(_));
        let msg = match job {
            Some(Job::Stop) => break,
            Some(Job::Dispatch { msg, reply }) => {
                if suspended {
                    if let Some(tx) = reply {
                        let _ = tx.send(Err("the script is suspended".into()));
                    }
                    continue;
                }
                if let (Some(tx), Some(id)) = (reply, msg["id"].as_u64()) {
                    waiting.insert(id, tx);
                }
                msg
            }
            None => {
                // a timer is due
                let due = inner.timers.borrow_mut().pop_first();
                let Some(((_, id), (ms, repeat))) = due else { continue };
                if repeat {
                    inner
                        .timers
                        .borrow_mut()
                        .insert((Instant::now() + Duration::from_millis(ms.max(10)), id), (ms, repeat));
                }
                if suspended {
                    continue;
                }
                json!({"type": "timer", "timer": id})
            }
        };
        let id = msg["id"].as_u64();
        inner.set_budget(Some(inner.budget.per_call));
        let result: Result<(), String> = ctx.with(|ctx| {
            let f: rquickjs::Function = ctx.globals().get("__lightair_dispatch").map_err(|e| e.to_string())?;
            let r = f.call::<_, ()>((msg.to_string(),)).map_err(|e| exception(&ctx, e));
            // promises the callback settled (its async parts)
            drain(&ctx);
            r
        });
        inner.set_budget(None);

        // replies that came (now, or from a promise a timer settled)
        let replies: Vec<(u64, Result<Value, String>)> = inner.replies.borrow_mut().drain().collect();
        for (rid, r) in replies {
            if let Some(tx) = waiting.remove(&rid) {
                let _ = tx.send(r);
            }
        }
        if let Err(e) = result {
            if let Some(tx) = id.and_then(|i| waiting.remove(&i)) {
                let _ = tx.send(Err(if is_overrun(&e) { "the script took too long".to_string() } else { e.clone() }));
            }
            if is_oom(&e) {
                set_state(&inner, State::Suspended("it used too much memory".into()));
            } else if is_overrun(&e) {
                overruns.retain(|t| t.elapsed() < Duration::from_secs(60));
                overruns.push(Instant::now());
                inner.record("time", "a callback", false, "ran over its time budget");
                if overruns.len() >= inner.budget.overruns {
                    set_state(&inner, State::Suspended("it kept running too long".into()));
                }
            } else {
                inner.host.log("error", &e);
            }
        }
    }
    set_state(&inner, State::Stopped);
}

/// Runs the promise jobs waiting; a job that throws is logged by the prelude's handlers, and the
/// rest still run.
fn drain(ctx: &rquickjs::Ctx<'_>) {
    for _ in 0..100 {
        if ctx.drain_jobs().is_ok() {
            return;
        }
    }
}

/// An error's message, with the JavaScript exception's text when there is one.
fn exception(ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error) -> String {
    if matches!(e, rquickjs::Error::Exception) {
        let v = ctx.catch();
        if let Some(ex) = v.as_exception() {
            return ex.message().unwrap_or_else(|| "an error".into());
        }
        if let Some(s) = v.as_string() {
            return s.to_string().unwrap_or_default();
        }
        return format!("{v:?}");
    }
    e.to_string()
}

fn is_overrun(e: &str) -> bool {
    e.contains("interrupted")
}

fn is_oom(e: &str) -> bool {
    e.contains("out of memory")
}
