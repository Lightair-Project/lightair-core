//! Emote and badge pictures, downloaded once into a cache folder by a few background workers
//! and drawn from there (the UI draws files, not web addresses).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use parking_lot::Mutex;

/// Downloads at once.
const WORKERS: usize = 4;

#[derive(Clone)]
enum State {
    Loading,
    Ready(PathBuf),
    Failed,
}

struct Inner {
    dir: PathBuf,
    files: Mutex<HashMap<String, State>>,
    jobs: Mutex<Option<mpsc::Sender<String>>>,
    ready: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Clone)]
pub struct Images {
    inner: Arc<Inner>,
}

impl Images {
    /// Pictures cached in `dir` (made if missing).
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Images {
            inner: Arc::new(Inner { dir, files: Mutex::default(), jobs: Mutex::new(None), ready: Mutex::default() }),
        }
    }

    /// Called (from a worker) when a picture has arrived.
    pub fn on_ready(&self, f: impl Fn() + Send + Sync + 'static) {
        self.inner.ready.lock().push(Box::new(f));
    }

    /// The picture's file if it's here; otherwise starts fetching it and says `None` for now.
    pub fn get(&self, url: &str) -> Option<PathBuf> {
        if url.is_empty() {
            return None;
        }
        let mut files = self.inner.files.lock();
        match files.get(url) {
            Some(State::Ready(p)) => return Some(p.clone()),
            Some(_) => return None,
            None => {}
        }
        // on disk from an earlier run
        if let Some(p) = self.on_disk(url) {
            files.insert(url.to_string(), State::Ready(p.clone()));
            return Some(p);
        }
        files.insert(url.to_string(), State::Loading);
        drop(files);
        self.queue(url.to_string());
        None
    }

    fn stem(&self, url: &str) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn on_disk(&self, url: &str) -> Option<PathBuf> {
        let stem = self.stem(url);
        ["png", "webp", "gif", "jpg", "svg"]
            .iter()
            .map(|ext| self.inner.dir.join(format!("{stem}.{ext}")))
            .find(|p| p.exists())
    }

    fn queue(&self, url: String) {
        let mut jobs = self.inner.jobs.lock();
        if jobs.is_none() {
            let (tx, rx) = mpsc::channel::<String>();
            let rx = Arc::new(Mutex::new(rx));
            for i in 0..WORKERS {
                let rx = rx.clone();
                let me = self.clone();
                let _ = std::thread::Builder::new().name(format!("lightair-images-{i}")).spawn(move || {
                    loop {
                        let Ok(url) = rx.lock().recv() else { return };
                        let state = me.fetch(&url).map_or(State::Failed, State::Ready);
                        me.inner.files.lock().insert(url, state);
                        for f in me.inner.ready.lock().iter() {
                            f();
                        }
                    }
                });
            }
            *jobs = Some(tx);
        }
        if let Some(tx) = jobs.as_ref() {
            let _ = tx.send(url);
        }
    }

    fn fetch(&self, url: &str) -> Option<PathBuf> {
        let agent = crate::net::agent();
        let mut res = agent.get(url).header("User-Agent", crate::net::USER_AGENT).call().ok()?;
        if !res.status().is_success() {
            return None;
        }
        let ext = match res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("") {
            t if t.contains("webp") => "webp",
            t if t.contains("gif") => "gif",
            t if t.contains("jpeg") => "jpg",
            t if t.contains("svg") => "svg",
            _ => "png",
        };
        let bytes = res.body_mut().with_config().limit(8 << 20).read_to_vec().ok()?;
        let path = self.inner.dir.join(format!("{}.{ext}", self.stem(url)));
        std::fs::write(&path, bytes).ok()?;
        Some(path)
    }
}
