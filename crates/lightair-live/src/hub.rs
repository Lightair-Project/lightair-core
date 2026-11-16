//! The chat hub: one reader per channel (each on a thread of its own, reconnecting with backoff),
//! every event in one history, and a callback for whoever shows it.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use crate::event::{Channel, ChatEvent, Kind, Status};

/// Events kept for the panel and late subscribers.
const HISTORY: usize = 2_000;

type Listener = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Inner {
    readers: Mutex<HashMap<Channel, Arc<AtomicBool>>>,
    history: Mutex<VecDeque<ChatEvent>>,
    status: Mutex<HashMap<Channel, Status>>,
    listeners: Mutex<Vec<Listener>>,
    seq: AtomicU64,
}

/// Where a reader puts what it reads.
#[derive(Clone)]
pub struct Sink {
    inner: Arc<Inner>,
    channel: Channel,
}

impl Sink {
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Adds an event (numbered here); a deletion marks what it deletes.
    pub fn emit(&self, mut event: ChatEvent) {
        event.id = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut history = self.inner.history.lock();
            if let Kind::Deletion { native_id, user_id } = &event.kind {
                for e in history.iter_mut().filter(|e| e.platform == event.platform) {
                    let by_id = native_id.is_some() && e.native_id == *native_id;
                    let by_user = user_id.is_some() && e.user.as_ref().map(|u| &u.id) == user_id.as_ref();
                    if by_id || by_user {
                        e.deleted = true;
                    }
                }
            }
            history.push_back(event);
            while history.len() > HISTORY {
                history.pop_front();
            }
        }
        self.changed();
    }

    pub fn status(&self, status: Status) {
        let changed = self.inner.status.lock().insert(self.channel.clone(), status.clone()) != Some(status);
        if changed {
            self.changed();
        }
    }

    fn changed(&self) {
        for l in self.inner.listeners.lock().iter() {
            l();
        }
    }
}

/// Runs a reader until it returns (an error, the stream ending) or `stop` is set.
pub type Reader = fn(&Sink, &AtomicBool) -> Result<(), String>;

#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called (from any thread) whenever there's something new to show.
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        self.inner.listeners.lock().push(Box::new(f));
    }

    /// Reads exactly these channels: new ones start, ones no longer listed stop. Channels on a
    /// platform without a reader are skipped.
    pub fn set_channels(&self, channels: &[Channel]) {
        let mut readers = self.inner.readers.lock();
        readers.retain(|c, stop| {
            let keep = channels.contains(c);
            if !keep {
                stop.store(true, Ordering::Release);
                self.inner.status.lock().remove(c);
            }
            keep
        });
        for c in channels {
            if readers.contains_key(c) {
                continue;
            }
            let Some(reader) = crate::reader_for(&c.platform) else { continue };
            let stop = Arc::new(AtomicBool::new(false));
            readers.insert(c.clone(), stop.clone());
            let sink = Sink { inner: self.inner.clone(), channel: c.clone() };
            let name = format!("lightair-chat-{}", c.platform);
            let _ = std::thread::Builder::new().name(name).spawn(move || run(reader, sink, stop));
        }
    }

    /// Everything read, oldest first.
    pub fn history(&self) -> Vec<ChatEvent> {
        self.inner.history.lock().iter().cloned().collect()
    }

    /// Events after `id`, oldest first.
    pub fn since(&self, id: u64) -> Vec<ChatEvent> {
        self.inner.history.lock().iter().filter(|e| e.id > id).cloned().collect()
    }

    pub fn status(&self) -> HashMap<Channel, Status> {
        self.inner.status.lock().clone()
    }

    /// Puts back events kept from before (numbered anew; nobody is told: they aren't new).
    pub fn restore(&self, events: Vec<ChatEvent>) {
        let mut history = self.inner.history.lock();
        for mut e in events {
            e.id = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
            history.push_back(e);
        }
        while history.len() > HISTORY {
            history.pop_front();
        }
    }

    /// Adds an event as if a reader had (tests and test events).
    pub fn inject(&self, channel: Channel, event: ChatEvent) {
        Sink { inner: self.inner.clone(), channel }.emit(event);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for stop in self.readers.get_mut().values() {
            stop.store(true, Ordering::Release);
        }
    }
}

/// A reader, again and again, waiting longer after each failure (1 s up to 30 s).
fn run(reader: Reader, sink: Sink, stop: Arc<AtomicBool>) {
    let mut wait = Duration::from_secs(1);
    while !stop.load(Ordering::Acquire) {
        if !crate::rules::enabled(&sink.channel.platform) {
            sink.status(Status::Error { message: "Lightair turned reading this platform off while it's fixed; it comes back on its own".into() });
            sleep_unless(&stop, Duration::from_secs(600));
            continue;
        }
        sink.status(Status::Connecting);
        match reader(&sink, &stop) {
            Ok(()) => wait = Duration::from_secs(1),
            Err(message) => {
                tracing::debug!(platform = sink.channel.platform, channel = sink.channel.channel, %message, "chat reader stopped");
                sink.status(Status::Error { message });
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
        sleep_unless(&stop, wait);
    }
}

/// Sleeps, waking early when `stop` is set. False when stopped.
pub fn sleep_unless(stop: &AtomicBool, d: Duration) -> bool {
    let until = std::time::Instant::now() + d;
    while std::time::Instant::now() < until {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100).min(until.saturating_duration_since(std::time::Instant::now())));
    }
    !stop.load(Ordering::Acquire)
}
