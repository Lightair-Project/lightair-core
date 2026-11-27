//! An utterance for vendors with no streaming text input (Gemini, the system's voice): a thread
//! of its own takes the pieces in order. The first goes alone, so it's heard soonest; the ones that
//! arrive while a piece is being made are joined into the next request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::{Chunk, Event, Sink, Utterance};

enum Msg {
    Piece(String),
    End,
}

/// Makes the sound for some text, handing chunks out as they come; stops early when `stop` is
/// set.
pub type Synth = Box<dyn FnMut(&str, &AtomicBool, &mut dyn FnMut(Chunk)) -> Result<(), String> + Send>;

pub struct Worker {
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    ended: bool,
}

pub fn spawn(name: &'static str, sink: Sink, synth: Synth) -> Worker {
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let _ = std::thread::Builder::new().name(format!("voice-{name}")).spawn(move || run(rx, s, sink, synth));
    Worker { tx, stop, ended: false }
}

/// Joined pieces stay under this many characters per request.
const GATHER: usize = 280;

fn run(rx: Receiver<Msg>, stop: Arc<AtomicBool>, mut sink: Sink, mut synth: Synth) {
    let mut first = true;
    let mut ended = false;
    let mut waiting: Vec<String> = Vec::new();
    loop {
        if waiting.is_empty() && !ended {
            match rx.recv() {
                Ok(Msg::Piece(p)) => waiting.push(p),
                Ok(Msg::End) => ended = true,
                Err(_) => return,
            }
        }
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Piece(p) => waiting.push(p),
                Msg::End => ended = true,
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if waiting.is_empty() {
            if ended {
                sink(Event::Done);
                return;
            }
            continue;
        }
        let take = if first {
            1
        } else {
            let mut n = 0;
            let mut len = 0;
            for p in &waiting {
                if n > 0 && len + p.len() > GATHER {
                    break;
                }
                len += p.len() + 1;
                n += 1;
            }
            n
        };
        first = false;
        let text = waiting.drain(..take).collect::<Vec<_>>().join(" ");
        let mut out = |c: Chunk| {
            if !stop.load(Ordering::Relaxed) && !c.pcm.is_empty() {
                sink(Event::Audio(c));
            }
        };
        if let Err(why) = synth(&text, &stop, &mut out) {
            if !stop.load(Ordering::Relaxed) {
                sink(Event::Failed(why));
            }
            return;
        }
    }
}

impl Utterance for Worker {
    fn say(&mut self, piece: &str) {
        if !self.ended && !piece.trim().is_empty() {
            let _ = self.tx.send(Msg::Piece(piece.trim().to_string()));
        }
    }

    fn end(&mut self) {
        if !self.ended {
            self.ended = true;
            let _ = self.tx.send(Msg::End);
        }
    }

    fn cancel(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.ended = true;
        let _ = self.tx.send(Msg::End);
    }
}
