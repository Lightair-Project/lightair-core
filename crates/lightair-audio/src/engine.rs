//! The two sides of the engine: [`Engine`] (main thread: compiles specs, hands out meters) and
//! [`Renderer`] (audio thread: processes, picks up new graphs without allocating or blocking).

use std::collections::HashMap;
use std::sync::Arc;

use crate::graph::{Graph, Inputs};
use crate::meter::Meter;
use crate::spec::{GraphSpec, NodeId, Warning};

/// The meter id of the program (master).
pub const PROGRAM: NodeId = 0;

/// Main-thread side.
pub struct Engine {
    sample_rate: u32,
    meters: HashMap<NodeId, Arc<Meter>>,
    to_audio: rtrb::Producer<Box<Graph>>,
    returned: rtrb::Consumer<Box<Graph>>,
    spec: GraphSpec,
}

/// Audio-thread side.
pub struct Renderer {
    graph: Box<Graph>,
    incoming: rtrb::Consumer<Box<Graph>>,
    to_main: rtrb::Producer<Box<Graph>>,
}

impl Engine {
    /// An engine running `spec` at `sample_rate`, and its renderer for the audio thread.
    pub fn new(spec: GraphSpec, sample_rate: u32) -> (Engine, Renderer) {
        let (to_audio, incoming) = rtrb::RingBuffer::new(8);
        let (to_main, returned) = rtrb::RingBuffer::new(16);
        let mut engine = Engine { sample_rate, meters: HashMap::new(), to_audio, returned, spec: spec.clone() };
        let graph = Box::new(engine.compile(&spec));
        (engine, Renderer { graph, incoming, to_main })
    }

    fn compile(&mut self, spec: &GraphSpec) -> Graph {
        let meters = &mut self.meters;
        Graph::compile(spec, self.sample_rate, |id| meters.entry(id).or_insert_with(Meter::shared).clone())
    }

    /// Changes the mixer. Compiles here; the renderer swaps it in at its next block, keeping
    /// effect state and ramping levels. Returns what the compiler had to correct.
    pub fn set(&mut self, spec: GraphSpec) -> Vec<Warning> {
        self.collect();
        let graph = self.compile(&spec);
        let warnings = graph.warnings.clone();
        self.spec = spec;
        if self.to_audio.push(Box::new(graph)).is_err() {
            // the renderer is far behind (not running): keep the newest only, next time
        }
        warnings
    }

    pub fn spec(&self) -> &GraphSpec {
        &self.spec
    }

    /// A strip's, mix's or the program's meter ([`PROGRAM`]); the same handle for its life.
    pub fn meter(&mut self, id: NodeId) -> Arc<Meter> {
        self.meters.entry(id).or_insert_with(Meter::shared).clone()
    }

    /// Frees graphs the renderer has finished with (call now and then; `set` does).
    pub fn collect(&mut self) {
        while self.returned.pop().is_ok() {}
    }
}

impl Renderer {
    /// Processes one block: takes up the newest graph first. Real-time safe.
    pub fn process(&mut self, inputs: &(impl Inputs + ?Sized), frames: usize) -> &Graph {
        while let Ok(mut next) = self.incoming.pop() {
            next.adopt(&mut self.graph);
            let old = std::mem::replace(&mut self.graph, next);
            // the main thread frees it; if its queue is full, keep it alive here rather than free
            if let Err(rtrb::PushError::Full(old)) = self.to_main.push(old) {
                std::mem::forget(old);
            }
        }
        self.graph.process(inputs, frames);
        &self.graph
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }
}
