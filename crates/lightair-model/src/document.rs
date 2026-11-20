//! The authoritative document: current snapshot, undo/redo, gesture coalescing.

use std::sync::Arc;

use crate::command::{Command, CommandError};
use crate::model::{Id, Workspace};

#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub label: String,
    before: Arc<Workspace>,
    after: Arc<Workspace>,
    coalesce: Option<(&'static str, Id, Id)>,
    /// True while a gesture is still adding to this entry.
    open: bool,
}

/// Owns the current workspace snapshot and its history. Snapshots are `Arc`s, so the UI and the
/// engine can hold a snapshot while new ones are produced.
#[derive(Debug, Clone)]
pub struct Document {
    current: Arc<Workspace>,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    in_gesture: bool,
    limit: usize,
}

/// Result of a successful change: the snapshot before and after (for diffing).
#[derive(Debug, Clone)]
pub struct Change {
    pub before: Arc<Workspace>,
    pub after: Arc<Workspace>,
    pub label: String,
}

impl Document {
    pub fn new(workspace: Workspace) -> Self {
        Self { current: Arc::new(workspace), undo: Vec::new(), redo: Vec::new(), in_gesture: false, limit: 1000 }
    }

    pub fn workspace(&self) -> &Arc<Workspace> {
        &self.current
    }

    /// Starts a continuous gesture (drag, scrub): coalescable commands merge into one step.
    pub fn begin_gesture(&mut self) {
        self.in_gesture = true;
    }

    /// Ends the gesture; the next command starts a new undo step.
    pub fn end_gesture(&mut self) {
        self.in_gesture = false;
        if let Some(last) = self.undo.last_mut() {
            last.open = false;
        }
    }

    pub fn apply(&mut self, command: &Command) -> Result<Change, CommandError> {
        let before = self.current.clone();
        let mut next = (*before).clone();
        command.apply(&mut next)?;
        let after = Arc::new(next);
        self.current = after.clone();
        let label = command.label();

        if command.is_undoable() {
            let key = command.coalesce_key();
            let merge = self.in_gesture
                && key.is_some()
                && self
                    .undo
                    .last()
                    .is_some_and(|last| last.open && last.coalesce == key && Arc::ptr_eq(&last.after, &before));
            if merge {
                let last = self.undo.last_mut().expect("checked");
                last.after = after.clone();
            } else {
                if let Some(last) = self.undo.last_mut() {
                    last.open = false;
                }
                self.undo.push(UndoEntry {
                    label: label.clone(),
                    before: before.clone(),
                    after: after.clone(),
                    coalesce: key,
                    open: self.in_gesture && key.is_some(),
                });
                if self.undo.len() > self.limit {
                    self.undo.remove(0);
                }
            }
            self.redo.clear();
        }
        Ok(Change { before, after, label })
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|e| e.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|e| e.label.as_str())
    }

    /// Restores the snapshot before the last undoable change. Runtime state that isn't undoable
    /// (the program scene) keeps its current value.
    pub fn undo(&mut self) -> Option<Change> {
        let mut entry = self.undo.pop()?;
        entry.open = false;
        let before = self.current.clone();
        self.current = keep_runtime(&entry.before, &before);
        let change = Change { before, after: self.current.clone(), label: entry.label.clone() };
        self.redo.push(entry);
        Some(change)
    }

    pub fn redo(&mut self) -> Option<Change> {
        let entry = self.redo.pop()?;
        let before = self.current.clone();
        self.current = keep_runtime(&entry.after, &before);
        let change = Change { before, after: self.current.clone(), label: entry.label.clone() };
        self.undo.push(entry);
        Some(change)
    }
}

/// `target`, but keeping what is live on air: if the scene that is on air now still exists in
/// `target`, it stays on air. Otherwise (it was added by the step being undone, or nothing is
/// on air) the snapshot's own program applies, so structural edits that moved the program
/// (removing the on-air scene, adding the first scene) undo and redo exactly.
fn keep_runtime(target: &Arc<Workspace>, live: &Arc<Workspace>) -> Arc<Workspace> {
    let keep_live = live.program.is_some_and(|p| target.scene(p).is_some());
    if !keep_live || target.program == live.program {
        return target.clone();
    }
    let mut p = (**target).clone();
    p.program = live.program;
    Arc::new(p)
}
