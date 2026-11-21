//! Everyone who has chatted: first seen, messages, points (per platform and user). Points come
//! from chatting (a few per message, at most once a minute each) and from steps.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Viewer {
    pub platform: String,
    pub id: String,
    pub name: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub messages: u64,
    pub points: f64,
    /// When points were last given for chatting.
    #[serde(default)]
    pub paid_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointsRules {
    /// What points are called in chat.
    pub name: String,
    pub per_message: f64,
    /// Seconds between paid messages per person.
    pub every: f64,
}

impl Default for PointsRules {
    fn default() -> Self {
        PointsRules { name: "points".into(), per_message: 5.0, every: 60.0 }
    }
}

pub fn key(platform: &str, id: &str) -> String {
    format!("{platform}:{id}")
}

#[derive(Default, Serialize, Deserialize)]
pub struct Viewers {
    pub by_key: HashMap<String, Viewer>,
}

impl Viewers {
    /// Counts a message: whether it's their first ever.
    pub fn chatted(&mut self, platform: &str, id: &str, name: &str, now_ms: i64, rules: &PointsRules) -> bool {
        let v = self.by_key.entry(key(platform, id)).or_insert_with(|| Viewer {
            platform: platform.into(),
            id: id.into(),
            name: name.into(),
            first_seen: now_ms,
            ..Default::default()
        });
        let first = v.messages == 0;
        v.messages += 1;
        v.last_seen = now_ms;
        v.name = name.into();
        if now_ms - v.paid_at >= (rules.every * 1000.0) as i64 {
            v.points += rules.per_message;
            v.paid_at = now_ms;
        }
        first
    }

    pub fn get(&self, platform: &str, id: &str) -> Option<&Viewer> {
        self.by_key.get(&key(platform, id))
    }

    pub fn add_points(&mut self, platform: &str, id: &str, name: &str, amount: f64) -> f64 {
        let v = self.by_key.entry(key(platform, id)).or_insert_with(|| Viewer {
            platform: platform.into(),
            id: id.into(),
            name: name.into(),
            ..Default::default()
        });
        v.points = (v.points + amount).max(0.0);
        v.points
    }

    /// The most points first.
    pub fn top(&self, n: usize) -> Vec<Viewer> {
        let mut all: Vec<Viewer> = self.by_key.values().cloned().collect();
        all.sort_by(|a, b| b.points.total_cmp(&a.points));
        all.truncate(n);
        all
    }
}
