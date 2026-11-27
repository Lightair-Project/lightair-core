//! A small command-line reader: words, `--flag value` pairs and boolean `--flags`.

use std::path::PathBuf;

/// Flags that take no value.
const BOOLEAN: &[&str] = &["--force", "--undo", "--no-register", "--deep", "--help"];

pub struct Args {
    items: Vec<String>,
    next: usize,
}

impl Args {
    pub fn new(items: Vec<String>) -> Args {
        Args { items, next: 0 }
    }

    /// The next word that isn't a flag (a command or subcommand), consumed.
    pub fn next_word(&mut self) -> Option<String> {
        let w = self.items.get(self.next).filter(|w| !w.starts_with("--")).cloned();
        if w.is_some() {
            self.next += 1;
        }
        w
    }

    /// The value after the first `flag`.
    pub fn value(&self, flag: &str) -> Option<String> {
        self.peek_value(flag)
    }

    pub fn peek_value(&self, flag: &str) -> Option<String> {
        self.items.iter().position(|a| a == flag).and_then(|i| self.items.get(i + 1)).cloned()
    }

    /// The values after every `flag`.
    pub fn values(&self, flag: &str) -> Vec<String> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == flag)
            .filter_map(|(i, _)| self.items.get(i + 1).cloned())
            .collect()
    }

    pub fn path(&self, flag: &str) -> Option<PathBuf> {
        self.value(flag).map(PathBuf::from)
    }

    pub fn has(&self, flag: &str) -> bool {
        self.items.iter().any(|a| a == flag)
    }

    /// The remaining words that aren't flags or flags' values.
    pub fn positionals(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = self.next;
        while i < self.items.len() {
            let a = &self.items[i];
            if a.starts_with("--") {
                i += if BOOLEAN.contains(&a.as_str()) { 1 } else { 2 };
                continue;
            }
            out.push(a.clone());
            i += 1;
        }
        out
    }
}
