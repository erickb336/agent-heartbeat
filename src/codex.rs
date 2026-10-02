//! The Codex adapter, for the CLI and the desktop app. Codex says its session-file format "isn't a
//! stable interface", so all knowledge of it lives in this file:
//!
//! - `sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl` and `archived_sessions/rollout-*.jsonl`:
//!   one file per thread. A child agent's thread has `source.subagent` in its first record.
//! - `token_usage_record`: one record per model request, with a unique `response_id`.
//! - `task_started`, then `task_complete` or `turn_aborted`: the turn lifecycle.
//! - `thread-writer-locks/<thread>.lock`: held open by the live process that owns the thread.
//!
//! Limitation: Codex records no event while a turn waits for approval, so such a turn counts as running.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::value::RawValue;

use crate::{
    hash::request_key,
    model::{AgentCounts, Provider, Reading, SourceHealth, Tokens, UsageRecord},
    system::Probe,
    tail::{self, Cursor, Line},
    time,
};

/// The record type sits within the first 86 bytes of every line checked on 2026-10-02.
const HEAD_LEN: usize = 256;
const WANTED: [&[u8]; 6] = [
    br#""type":"session_meta""#,
    br#""type":"token_usage_record""#,
    br#""type":"task_started""#,
    br#""type":"task_complete""#,
    br#""type":"turn_aborted""#,
    br#""type":"token_count""#,
];
const LOCK_PROCESS_PREFIX: &str = "codex";

#[derive(Default, Serialize, Deserialize)]
pub struct CodexState {
    files: HashMap<PathBuf, FileState>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct FileState {
    cursor: Cursor,
    /// The latest turn that started and has not ended. A thread runs one turn at a time, so a newer
    /// turn replaces an older one that never got an end event (seen live: an app restart).
    open_turn: Option<String>,
    usage_records: u32,
    token_counts: u32,
}

pub struct CodexAdapter {
    home: PathBuf,
    state: CodexState,
}

#[derive(Deserialize)]
struct Record<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    timestamp: Option<&'a str>,
    #[serde(borrow)]
    payload: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct UsagePayload {
    response_id: Option<String>,
    usage: Option<CodexUsage>,
}

#[derive(Deserialize)]
struct CodexUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    total_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct EventPayload {
    #[serde(rename = "type")]
    kind: String,
    turn_id: Option<String>,
    info: Option<IgnoredAny>,
}

#[derive(Default)]
struct Counters {
    malformed: u32,
    oversize: u32,
    missing_fields: u32,
    inconsistent: u32,
    unreadable_files: u32,
}

impl CodexAdapter {
    pub fn new(home: PathBuf, state: Option<CodexState>) -> Self {
        CodexAdapter { home, state: state.unwrap_or_default() }
    }

    pub fn state(&self) -> &CodexState {
        &self.state
    }

    pub fn poll(&mut self, probe: &dyn Probe, window_start: i64) -> Reading {
        let sessions = self.home.join("sessions");
        if !sessions.is_dir() {
            return Reading {
                provider: Provider::Codex,
                agents: None,
                agents_health: SourceHealth::unavailable("codex session folder not found"),
                usage: Vec::new(),
                usage_health: SourceHealth::unavailable("codex session folder not found"),
            };
        }
        let locks = self.home.join("thread-writer-locks");
        // Only processes named "codex" first: about 1 ms instead of about 13 ms for all processes.
        let held = probe.held_files(&locks, Some(LOCK_PROCESS_PREFIX));
        let held_threads: HashSet<String> = held
            .iter()
            .flatten()
            .filter_map(|name| name.strip_suffix(".lock").map(str::to_string))
            .collect();

        let mut listed = Vec::new();
        tail::list_jsonl(&sessions, 3, &mut listed);
        tail::list_jsonl(&self.home.join("archived_sessions"), 0, &mut listed);
        let mut usage = Vec::new();
        let mut c = Counters::default();
        let mut keep = HashSet::new();
        for (path, meta) in listed {
            let thread = thread_id(&path);
            let loaded = thread.is_some_and(|t| held_threads.contains(t));
            // A loaded thread is read even when it is older than the window: it may hold an open turn.
            if tail::mtime_ms(&meta) < window_start && !loaded {
                continue;
            }
            keep.insert(path.clone());
            let file = self.state.files.entry(path.clone()).or_default();
            if file.cursor.unchanged(&meta) {
                continue;
            }
            let mut cursor = std::mem::take(&mut file.cursor);
            let result = tail::read_new(
                &path,
                &mut cursor,
                HEAD_LEN,
                |head| WANTED.iter().any(|n| tail::contains(head, n)),
                |line| match line {
                    Line::Wanted(bytes) => parse_line(bytes, window_start, file, &mut usage, &mut c),
                    Line::Other => {}
                    Line::Oversize => c.oversize += 1,
                },
            );
            file.cursor = cursor;
            if result.is_err() {
                c.unreadable_files += 1;
            }
        }
        self.state.files.retain(|p, _| keep.contains(p));

        let mut usage_health = SourceHealth::ok();
        let format_changed = self
            .state
            .files
            .values()
            .filter(|f| f.token_counts >= 3 && f.usage_records == 0)
            .count();
        if format_changed > 0 {
            usage_health.degrade(format!(
                "format change: {format_changed} files have token counts without per-request records"
            ));
        }
        for (n, what) in [
            (c.malformed, "malformed lines skipped"),
            (c.missing_fields, "usage records without required fields skipped"),
            (c.inconsistent, "usage records with inconsistent totals counted"),
            (c.oversize, "lines over the size limit skipped"),
            (c.unreadable_files, "files could not be read"),
        ] {
            if n > 0 {
                usage_health.degrade(format!("{n} {what}"));
            }
        }

        let (agents, agents_health) = match held {
            None => (None, SourceHealth::unavailable("lock check failed")),
            Some(_) => (Some(self.agent_counts(probe, &locks, &held_threads)), SourceHealth::ok()),
        };
        Reading { provider: Provider::Codex, agents, agents_health, usage, usage_health }
    }

    fn agent_counts(&self, probe: &dyn Probe, locks: &std::path::Path, held: &HashSet<String>) -> AgentCounts {
        let open: Vec<&str> = self
            .state
            .files
            .iter()
            .filter(|(_, f)| f.open_turn.is_some())
            .filter_map(|(p, _)| thread_id(p))
            .collect();
        // If an open turn's lock is not held by a "codex" process, check every process once before
        // calling the turn dead. Another client process may own the thread.
        let all_held;
        let held = if open.iter().any(|t| !held.contains(*t)) {
            all_held = probe
                .held_files(locks, None)
                .map(|s| s.iter().filter_map(|n| n.strip_suffix(".lock").map(str::to_string)).collect())
                .unwrap_or_else(|| held.clone());
            &all_held
        } else {
            held
        };
        let running = open.iter().filter(|t| held.contains(**t)).count() as u32;
        let loaded = held.len() as u32;
        AgentCounts { running, waiting: 0, idle: loaded.saturating_sub(running) }
    }
}

fn parse_line(bytes: &[u8], window_start: i64, file: &mut FileState, usage: &mut Vec<UsageRecord>, c: &mut Counters) {
    let Ok(record) = serde_json::from_slice::<Record>(bytes) else {
        c.malformed += 1;
        return;
    };
    let payload = record.payload.map(RawValue::get).unwrap_or("null");
    match record.kind {
        "token_usage_record" => {
            file.usage_records += 1;
            let parsed = serde_json::from_str::<UsagePayload>(payload).ok();
            let (Some(UsagePayload { response_id: Some(id), usage: Some(u) }), Some(at_ms)) =
                (parsed, record.timestamp.and_then(time::parse_ms))
            else {
                c.missing_fields += 1;
                return;
            };
            // OpenAI's input already includes cached input; output already includes reasoning.
            if u.total_tokens.is_some_and(|t| t != u.input_tokens + u.output_tokens) || u.cached_input_tokens > u.input_tokens {
                c.inconsistent += 1;
            }
            if at_ms >= window_start {
                usage.push(UsageRecord {
                    key: request_key(&["codex", &id]),
                    at_ms,
                    tokens: Tokens { input: u.input_tokens, cached_input: u.cached_input_tokens, output: u.output_tokens },
                });
            }
        }
        "event_msg" => {
            let Ok(event) = serde_json::from_str::<EventPayload>(payload) else {
                c.malformed += 1;
                return;
            };
            match (event.kind.as_str(), event.turn_id) {
                ("task_started", Some(turn)) => file.open_turn = Some(turn),
                ("task_complete" | "turn_aborted", Some(turn)) if file.open_turn.as_ref() == Some(&turn) => file.open_turn = None,
                ("token_count", _) if event.info.is_some() => file.token_counts += 1,
                _ => {}
            }
        }
        _ => {}
    }
}

/// `rollout-<time>-<thread>.jsonl` ends with the 36-character thread ID (175 of 175 files checked).
fn thread_id(path: &std::path::Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    (stem.starts_with("rollout-") && stem.len() > 36 + 9).then(|| &stem[stem.len() - 36..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crate::model::Health;
    use crate::system::fake::FakeProbe;

    const WINDOW: i64 = 1_790_838_000_000; // 2026-10-01T07:00:00Z, the start of 1 October in Seattle

    fn held(threads: &[&str]) -> FakeProbe {
        FakeProbe { held: Some(threads.iter().map(|t| format!("{t}.lock")).collect()), ..Default::default() }
    }

    #[test]
    fn running_needs_an_open_turn_and_a_held_lock() {
        let home = temp_home("codex-run");
        let root_open = codex_rollout(&home, THREAD_A, false);
        append(&root_open, &codex_event("task_started", "turn-1"));
        let child_open = codex_rollout(&home, THREAD_B, true);
        append(&child_open, &codex_event("task_started", "turn-2"));
        let idle = codex_rollout(&home, THREAD_C, false);
        append(&idle, &codex_event("task_started", "turn-3"));
        append(&idle, &codex_event("task_complete", "turn-3"));
        let crashed = codex_rollout(&home, THREAD_D, false);
        append(&crashed, &codex_event("task_started", "turn-4"));
        let mut a = CodexAdapter::new(home.clone(), None);
        // D's lock is not held: the turn has no live owner.
        let r = a.poll(&held(&[THREAD_A, THREAD_B, THREAD_C]), WINDOW);
        let counts = r.agents.unwrap();
        assert_eq!((counts.running, counts.idle), (2, 1));
        // An interrupted turn ends with turn_aborted.
        append(&root_open, &codex_event("turn_aborted", "turn-1"));
        let r = a.poll(&held(&[THREAD_A, THREAD_B, THREAD_C]), WINDOW);
        assert_eq!(r.agents.unwrap().running, 1);
        assert_canary_free(&format!("{r:?}"));
        assert_no_content(&serde_json::to_string(a.state()).unwrap());
    }

    #[test]
    fn an_old_turn_without_an_end_event_does_not_count_after_a_newer_turn_ends() {
        // Seen live on 2026-10-02: a turn from 12 hours earlier had no end event, and later turns
        // in the same thread started and completed normally.
        let home = temp_home("codex-orphan");
        let f = codex_rollout(&home, THREAD_A, false);
        append(&f, &codex_event("task_started", "turn-old"));
        append(&f, &codex_event("task_started", "turn-new"));
        append(&f, &codex_event("task_complete", "turn-new"));
        let mut a = CodexAdapter::new(home, None);
        assert_eq!(a.poll(&held(&[THREAD_A]), WINDOW).agents.unwrap().running, 0);
        // A late end event for the old turn changes nothing.
        append(&f, &codex_event("task_started", "turn-3"));
        append(&f, &codex_event("turn_aborted", "turn-old"));
        assert_eq!(a.poll(&held(&[THREAD_A]), WINDOW).agents.unwrap().running, 1);
    }

    #[test]
    fn usage_uses_per_request_records_only() {
        let home = temp_home("codex-usage");
        let f = codex_rollout(&home, THREAD_A, false);
        append(&f, &codex_usage("resp_1", "2026-10-02T18:00:00Z", 1000, 900, 50));
        append(&f, &codex_token_count(5000, 4000, 70)); // a cumulative snapshot: ignored
        append(&f, &codex_usage("resp_2", "2026-10-02T18:01:00Z", 2000, 1800, 20));
        let r = CodexAdapter::new(home, None).poll(&held(&[]), WINDOW);
        assert_eq!(r.usage.len(), 2);
        assert_eq!(r.usage[0].tokens, Tokens { input: 1000, cached_input: 900, output: 50 });
        assert_eq!(r.usage_health, SourceHealth::ok());
    }

    #[test]
    fn a_format_change_degrades_instead_of_counting_zero() {
        let home = temp_home("codex-format");
        let f = codex_rollout(&home, THREAD_A, false);
        for _ in 0..3 {
            append(&f, &codex_token_count(5000, 4000, 70));
        }
        append(&f, "{\"type\":\"token_usage_record\",broken\n");
        let r = CodexAdapter::new(home, None).poll(&held(&[]), WINDOW);
        assert_eq!(r.usage_health.health, Health::Degraded);
        assert_eq!(r.usage_health.notes.len(), 2, "{:?}", r.usage_health.notes);
    }

    #[test]
    fn a_loaded_thread_older_than_the_window_still_reports_its_open_turn() {
        let home = temp_home("codex-old");
        let f = codex_rollout(&home, THREAD_A, false);
        append(&f, &codex_usage("resp_old", "2026-09-29T18:00:00Z", 10, 0, 1));
        append(&f, &codex_event("task_started", "turn-1"));
        set_mtime(&f, WINDOW - 86_400_000);
        let r = CodexAdapter::new(home, None).poll(&held(&[THREAD_A]), WINDOW);
        assert_eq!(r.agents.unwrap().running, 1);
        assert!(r.usage.is_empty(), "usage before the window is not counted");
    }

    #[test]
    fn a_failed_lock_check_makes_agents_unavailable() {
        let home = temp_home("codex-nolsof");
        codex_rollout(&home, THREAD_A, false);
        let r = CodexAdapter::new(home.clone(), None).poll(&FakeProbe::default(), WINDOW);
        assert!(r.agents.is_none());
        assert_eq!(r.agents_health.health, Health::Unavailable);
        let r = CodexAdapter::new(home.join("missing"), None).poll(&held(&[]), WINDOW);
        assert_eq!(r.usage_health.health, Health::Unavailable);
    }
}
