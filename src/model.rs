//! Shared types. Nothing here holds content, names, paths or identifiers that leave the machine.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// ok: the source works. degraded: it works, but some records were skipped.
/// unavailable: it does not work, so it reports no numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ok,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Coverage {
    Complete,
    Partial,
    Unavailable,
}

/// One model request's usage, normalised. The same key can arrive again with larger numbers
/// (a Claude response that streamed); the ledger keeps the per-field maximum.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    /// Local dedup key: the first 8 bytes of SHA-256 of the request identity. Never sent.
    pub key: u64,
    pub at_ms: i64,
    pub tokens: Tokens,
}

/// Processed input (cached input included), the cached part of it, and output (reasoning included).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }
    pub fn add(&mut self, other: Tokens) {
        self.input += other.input;
        self.cached_input += other.cached_input;
        self.output += other.output;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentCounts {
    /// Agents with a running turn now. This is the "active" number.
    pub running: u32,
    /// Agents that wait for the user (input or approval). Not active.
    pub waiting: u32,
    /// Live agents with no running turn. Not active.
    pub idle: u32,
}

/// Operational facts for the local health report: fixed phrases with numbers only.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourceHealth {
    pub health: Health,
    pub notes: Vec<String>,
}

impl SourceHealth {
    pub fn ok() -> Self {
        SourceHealth { health: Health::Ok, notes: Vec::new() }
    }
    pub fn unavailable(note: &str) -> Self {
        SourceHealth { health: Health::Unavailable, notes: vec![note.to_string()] }
    }
    /// Marks the source degraded (unless it is already unavailable) and records why.
    pub fn degrade(&mut self, note: String) {
        if self.health == Health::Ok {
            self.health = Health::Degraded;
        }
        self.notes.push(note);
    }
}

/// What one adapter returns from one poll.
#[derive(Clone, Debug)]
pub struct Reading {
    pub provider: Provider,
    /// None when the live-state source does not work.
    pub agents: Option<AgentCounts>,
    pub agents_health: SourceHealth,
    /// Records that are new or grew since the last poll.
    pub usage: Vec<UsageRecord>,
    pub usage_health: SourceHealth,
}
