//! The report a check run writes (catalog formats §8, §10): checks with their verdicts, what was
//! observed, previews, the code for the auditor, and the files measured.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A check's verdict on one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Note,
    Fail,
}

/// One result: a verdict, a sentence, and the file it's about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub verdict: Verdict,
    pub message: String,
    #[serde(default)]
    pub file: Option<String>,
}

/// One check and its results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub version: String,
    pub results: Vec<Finding>,
}

impl Check {
    pub fn new(name: &str) -> Check {
        Check { name: name.to_string(), version: crate::CHECKS_VERSION.to_string(), results: Vec::new() }
    }

    pub fn fail(&mut self, message: impl Into<String>, file: Option<&str>) {
        self.results.push(Finding { verdict: Verdict::Fail, message: message.into(), file: file.map(str::to_string) });
    }

    pub fn note(&mut self, message: impl Into<String>, file: Option<&str>) {
        self.results.push(Finding { verdict: Verdict::Note, message: message.into(), file: file.map(str::to_string) });
    }

    /// Adds a pass when nothing failed or was noted, so every check reports something.
    pub fn finish(mut self, passed: &str) -> Check {
        if self.results.is_empty() {
            self.results.push(Finding { verdict: Verdict::Pass, message: passed.to_string(), file: None });
        }
        self
    }

    /// The worst verdict.
    pub fn verdict(&self) -> Verdict {
        self.results.iter().map(|r| r.verdict).max().unwrap_or(Verdict::Pass)
    }
}

/// Network, file and process activity a run saw.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    #[serde(default)]
    pub network: Vec<NetworkUse>,
    #[serde(default)]
    pub files: Vec<FileUse>,
    #[serde(default)]
    pub processes: Vec<String>,
    #[serde(default)]
    pub sandbox_violations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkUse {
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileUse {
    pub path: String,
    pub op: String,
    pub allowed: bool,
}

/// A rendered preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub name: String,
    pub webp_base64: String,
}

/// A piece of code for the auditor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Code {
    pub path: String,
    pub text: String,
}

/// A package file as measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    pub platform: String,
    pub url: String,
    pub size: u64,
    pub archive_sha256: String,
    pub content_sha256: String,
    pub manifest: Value,
}

/// What a checker command writes to `--json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Output {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<Observed>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previews: Vec<Preview>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<Code>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Measured>,
}

impl Output {
    /// The worst verdict of every check.
    pub fn verdict(&self) -> Verdict {
        self.checks.iter().map(Check::verdict).max().unwrap_or(Verdict::Pass)
    }
}
