//! Hosted plugins (Audio Units, VST3): the host makes and keeps the instance; the graph holds a
//! shared handle and processes it in place like any insert.

use std::sync::Arc;

use super::{Ctx, Params, Processor};

/// A plugin instance, made and owned by its host (off the audio thread).
///
/// `process` runs on the audio thread, called by one graph at a time; it must not block or
/// allocate (as far as the plugin allows). The host's other calls (state, editor) come from other
/// threads, which plugin formats allow.
pub trait Plugin: Send + Sync {
    /// Processes a stereo block in place. False when the plugin failed: the block passes through.
    fn process(&self, l: &mut [f32], r: &mut [f32]) -> bool;
    /// Frames of delay it adds.
    fn latency(&self) -> usize {
        0
    }
    /// Forgets its history.
    fn reset(&self) {}
}

/// A plugin insert: the loaded instance, or none yet (loading, missing, failed) and it passes
/// the audio through.
#[derive(Clone, Default)]
pub struct PluginParams {
    pub plugin: Option<Arc<dyn Plugin>>,
}

impl PluginParams {
    fn identity(&self) -> usize {
        self.plugin.as_ref().map_or(0, |p| Arc::as_ptr(p) as *const () as usize)
    }
}

impl PartialEq for PluginParams {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl std::fmt::Debug for PluginParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginParams").field("loaded", &self.plugin.is_some()).finish()
    }
}

pub(super) struct Hosted {
    params: PluginParams,
}

impl Hosted {
    pub(super) fn new(params: &PluginParams) -> Self {
        Self { params: params.clone() }
    }
}

impl Processor for Hosted {
    fn kind(&self) -> &'static str {
        "audio.plugin"
    }

    fn set(&mut self, _params: &Params) {
        // the instance never changes in place: a different one is a different processor
        // (see `identity`), so its handle is never dropped here, on the audio thread
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32], _ctx: &Ctx) {
        if let Some(p) = &self.params.plugin {
            p.process(l, r);
        }
    }

    fn reset(&mut self) {
        if let Some(p) = &self.params.plugin {
            p.reset();
        }
    }

    fn latency(&self) -> usize {
        self.params.plugin.as_ref().map_or(0, |p| p.latency())
    }

    fn identity(&self) -> usize {
        self.params.identity()
    }
}
