//! Hidden browser pages, for the platforms that only answer a browser (docs/plans/live-integrations.md,
//! 2.10): a Cloudflare check, a signed-in session, a chat drawn by the page itself. The live crate
//! doesn't carry a browser; the app gives it one with [`set_host`] (Lightair's Chromium, sharing
//! its cookies, so a site signed in to once stays signed in).

use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicBool, Ordering};

/// What a page's script sent back.
pub type OnMessage = Arc<dyn Fn(String) + Send + Sync>;

/// Gets cookies: (name, value).
pub type CookiesDone = Box<dyn FnOnce(Vec<(String, String)>) + Send>;

/// An open hidden page; dropping it closes it.
pub trait Page: Send {
    /// Runs `code` in the page now.
    fn eval(&self, code: &str);
    /// Goes to `url` (the script runs again once it's loaded).
    fn navigate(&self, url: &str);
}

/// Opens hidden pages: `script` runs after every load, and calls `lightair(text)` to send a
/// message (given to `on_message`).
pub trait Host: Send + Sync {
    fn open(&self, url: &str, script: &str, on_message: OnMessage) -> Box<dyn Page>;
    /// `open`, with `early` run as each load starts (before the page's own scripts get far).
    fn open_early(&self, url: &str, early: &str, script: &str, on_message: OnMessage) -> Box<dyn Page> {
        let _ = early;
        self.open(url, script, on_message)
    }
    /// A page in a window for the person to use (signing in); `on_closed` when they close it.
    fn open_window(&self, url: &str, title: &str, on_closed: Arc<dyn Fn() + Send + Sync>) -> Box<dyn Page>;
    /// The cookies the browser sends to `url` (name, value).
    fn cookies(&self, url: &str, done: CookiesDone);
}

/// The browser's cookies for `url`, waited for (at most a few seconds).
pub fn cookies(url: &str) -> Vec<(String, String)> {
    let Ok(host) = host() else { return Vec::new() };
    let (tx, rx) = mpsc::channel();
    host.cookies(url, Box::new(move |c| {
        let _ = tx.send(c);
    }));
    rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
}

/// One cookie's value.
pub fn cookie(url: &str, name: &str) -> Option<String> {
    cookies(url).into_iter().find(|(n, _)| n == name).map(|(_, v)| v).filter(|v| !v.is_empty())
}

static HOST: OnceLock<Arc<dyn Host>> = OnceLock::new();

/// The browser the readers that need one use (set once, by the app).
pub fn set_host(host: Arc<dyn Host>) {
    let _ = HOST.set(host);
}

pub fn host() -> Result<Arc<dyn Host>, String> {
    HOST.get().cloned().ok_or_else(|| "this needs Lightair's built-in browser".to_string())
}

/// What every script can call to send a message: `lightair("text")`. The host defines how it
/// gets out (Chromium: a console line with a marker).
pub const PRELUDE: &str = "window.lightair = window.lightair || function (m) { console.log('\\u0001lightair:' + m); };\n";

/// Opens `url` with `script` (which can call `lightair(text)`) and gives back the first message
/// it sends, within `timeout`, or sooner if `stop` is set. The page is closed after.
pub fn run(url: &str, script: &str, timeout: Duration, stop: &AtomicBool) -> Result<String, String> {
    let host = host()?;
    let (tx, rx) = mpsc::channel::<String>();
    let tx = parking_lot::Mutex::new(tx);
    let page = host.open(url, &format!("{PRELUDE}{script}"), Arc::new(move |m| {
        let _ = tx.lock().send(m);
    }));
    let started = Instant::now();
    let out = loop {
        if stop.load(Ordering::Acquire) {
            break Err("stopped".to_string());
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(text) => break Ok(text),
            Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() < timeout => continue,
            Err(_) => break Err(format!("{url}: no answer")),
        }
    };
    drop(page);
    out
}

/// The JSON a URL answers when a browser asks for it (the page body, once it parses).
pub fn fetch_json(url: &str, timeout: Duration, stop: &AtomicBool) -> Result<serde_json::Value, String> {
    let script = "(function () { try { var t = document.body && document.body.innerText; JSON.parse(t); lightair(t); } catch (e) {} })();";
    let text = run(url, script, timeout, stop)?;
    serde_json::from_str(&text).map_err(|e| format!("{url}: {e}"))
}

/// The page scripts run in (blank, kept while used), its answers by run number.
struct Scripts {
    page: Box<dyn Page>,
    answers: Arc<parking_lot::Mutex<std::collections::HashMap<u64, String>>>,
    used: Instant,
}

static SCRIPTS: parking_lot::Mutex<Option<Scripts>> = parking_lot::Mutex::new(None);
static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Runs `code` (a JavaScript function body: `ctx` is given, `return` an object; `await` works)
/// in a blank hidden page, within `timeout`: what it returned.
pub fn run_script(code: &str, ctx: &serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let answers = {
        let mut s = SCRIPTS.lock();
        if s.as_ref().is_none_or(|s| s.used.elapsed() > Duration::from_secs(600)) {
            let answers: Arc<parking_lot::Mutex<std::collections::HashMap<u64, String>>> = Default::default();
            let store = answers.clone();
            let page = host()?.open("data:text/html,<title>Lightair scripts</title>", &format!("{PRELUDE}lightair('0:ready');"), Arc::new(move |m: String| {
                if let Some((id, rest)) = m.split_once(':')
                    && let Ok(id) = id.parse::<u64>()
                {
                    store.lock().insert(id, rest.to_string());
                }
            }));
            // the page says when it's loaded and can run something
            let started = Instant::now();
            while answers.lock().remove(&0).is_none() {
                if started.elapsed() > Duration::from_secs(10) {
                    return Err("the script page didn't load".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            *s = Some(Scripts { page, answers, used: Instant::now() });
        }
        let s = s.as_mut().unwrap();
        s.used = Instant::now();
        let wrapped = format!(
            "{PRELUDE}(async function (ctx) {{\n{code}\n}})({ctx}).then(function (r) {{ lightair('{n}:' + JSON.stringify({{ ok: true, result: r === undefined ? null : r }})); }}, function (e) {{ lightair('{n}:' + JSON.stringify({{ ok: false, error: String(e && e.message || e) }})); }});",
            ctx = serde_json::to_string(ctx).unwrap_or_else(|_| "{}".into())
        );
        s.page.eval(&wrapped);
        s.answers.clone()
    };
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Some(a) = answers.lock().remove(&n) {
            let v: serde_json::Value = serde_json::from_str(&a).map_err(|e| e.to_string())?;
            return if v["ok"].as_bool() == Some(true) { Ok(v["result"].clone()) } else { Err(v["error"].as_str().unwrap_or("the script failed").to_string()) };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("the script didn't finish in time".into())
}
