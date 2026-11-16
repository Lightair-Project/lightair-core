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
