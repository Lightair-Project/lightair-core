//! The system's own voice (`lightair_platform::speech`): free and offline. Each piece is made whole
//! and then handed out, so a long text starts with its first sentence.

use crate::{Chunk, Engine, Sink, Utterance};

pub struct System {
    voice: String,
    speed: f32,
}

impl System {
    pub fn new(voice: &str, speed: f32) -> System {
        System { voice: voice.to_string(), speed }
    }
}

impl Engine for System {
    fn utterance(&self, sink: Sink) -> Box<dyn Utterance> {
        let (voice, speed) = (self.voice.clone(), self.speed);
        Box::new(crate::worker::spawn(
            "system",
            sink,
            Box::new(move |text, _stop, out| {
                let wav = lightair_platform::speech::synthesize(text, &voice, speed)?;
                let (pcm, rate) = crate::read_wav(&wav)?;
                // in pieces of a fifth of a second, as a streamed voice would come
                for part in pcm.chunks((rate / 5).max(1) as usize) {
                    out(Chunk { pcm: part.to_vec(), rate, marks: Vec::new() });
                }
                Ok(())
            }),
        ))
    }
}
