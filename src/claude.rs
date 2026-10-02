//! The Claude Code adapter. Claude Code does not document these formats, so all knowledge of them
//! lives in this file:
//!
//! - `sessions/<pid>.json`: the live-session registry, with `status` busy / waiting / idle / shell.
//! - `projects/<project>/<session>.jsonl`: a session transcript.
//! - `projects/<project>/<session>/subagents/agent-<id>.jsonl`: a child agent's transcript.
//!
//! From a transcript line the adapter reads only the record type, the timestamp, the request
//! identity, the stop reason and the token counts. Serde skips every other field without keeping it.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    hash::request_key,
    model::{AgentCounts, Provider, Reading, SourceHealth, Tokens, UsageRecord},
    system::Probe,
    tail::{self, Cursor, Line},
    time,
};

/// A child agent that writes nothing for this long counts as stalled, not running.
/// On 2026-10-02, the 99.9th percentile of silent gaps in real child files was 184 s.
pub const CHILD_STALL_MS: i64 = 10 * 60 * 1000;

/// `"role":"assistant"` appeared within the first 290 bytes of every assistant line checked.
const HEAD_LEN: usize = 1024;
const ASSISTANT_MARKER: &[u8] = br#""role":"assistant""#;

#[derive(Default, Serialize, Deserialize)]
pub struct ClaudeState {
    files: HashMap<PathBuf, FileState>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct FileState {
    cursor: Cursor,
    finished: bool,
}

pub struct ClaudeAdapter {
    home: PathBuf,
    state: ClaudeState,
}

#[derive(Deserialize)]
struct RegistryEntry {
    pid: i32,
    #[serde(rename = "sessionId")]
    session_id: String,
    status: Option<String>,
    #[serde(rename = "procStart")]
    proc_start: Option<String>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Default)]
struct Counters {
    malformed: u32,
    oversize: u32,
    missing_fields: u32,
    unreadable_files: u32,
}

impl ClaudeAdapter {
    pub fn new(home: PathBuf, state: Option<ClaudeState>) -> Self {
        ClaudeAdapter { home, state: state.unwrap_or_default() }
    }

    pub fn state(&self) -> &ClaudeState {
        &self.state
    }

    /// `window_start`: usage before this instant is ignored, and files older than it are not read.
    pub fn poll(&mut self, probe: &dyn Probe, now_ms: i64, window_start: i64) -> Reading {
        if !self.home.is_dir() {
            return Reading {
                provider: Provider::Claude,
                agents: None,
                agents_health: SourceHealth::unavailable("claude home not found"),
                usage: Vec::new(),
                usage_health: SourceHealth::unavailable("claude home not found"),
            };
        }
        let (usage, usage_health) = self.read_transcripts(now_ms, window_start);
        let (agents, agents_health) = self.agent_counts(probe, now_ms);
        Reading { provider: Provider::Claude, agents, agents_health, usage, usage_health }
    }

    fn read_transcripts(&mut self, now_ms: i64, window_start: i64) -> (Vec<UsageRecord>, SourceHealth) {
        let projects = self.home.join("projects");
        if !projects.is_dir() {
            return (Vec::new(), SourceHealth::unavailable("transcript folder not found"));
        }
        let mut listed = Vec::new();
        tail::list_jsonl(&projects, 3, &mut listed);
        let mut usage = Vec::new();
        let mut c = Counters::default();
        let mut keep = HashSet::new();
        for (path, meta) in listed {
            let mtime = tail::mtime_ms(&meta);
            let recent_child = is_child(&path) && mtime >= now_ms - CHILD_STALL_MS;
            if mtime < window_start && !recent_child {
                continue;
            }
            keep.insert(path.clone());
            let file = self.state.files.entry(path.clone()).or_default();
            if file.cursor.unchanged(&meta) {
                continue;
            }
            let result = tail::read_new(
                &path,
                &mut file.cursor,
                HEAD_LEN,
                |head| tail::contains(head, ASSISTANT_MARKER),
                |line| match line {
                    Line::Wanted(bytes) => parse_line(bytes, window_start, &mut usage, &mut file.finished, &mut c),
                    Line::Other => file.finished = false,
                    Line::Oversize => c.oversize += 1,
                },
            );
            if result.is_err() {
                c.unreadable_files += 1;
            }
        }
        self.state.files.retain(|p, _| keep.contains(p));

        let mut health = SourceHealth::ok();
        if c.malformed > 0 {
            health.degrade(format!("{} malformed lines skipped", c.malformed));
        }
        if c.missing_fields > 0 {
            health.degrade(format!("{} usage records without required fields skipped", c.missing_fields));
        }
        if c.oversize > 0 {
            health.degrade(format!("{} lines over the size limit skipped", c.oversize));
        }
        if c.unreadable_files > 0 {
            health.degrade(format!("{} files could not be read", c.unreadable_files));
        }
        (usage, health)
    }

    fn agent_counts(&self, probe: &dyn Probe, now_ms: i64) -> (Option<AgentCounts>, SourceHealth) {
        let Ok(entries) = fs::read_dir(self.home.join("sessions")) else {
            return (None, SourceHealth::unavailable("session registry not found"));
        };
        let mut counts = AgentCounts::default();
        let mut health = SourceHealth::ok();
        let mut live_sessions = HashSet::new();
        let (mut unreadable, mut unknown_status) = (0, 0);
        for entry in entries.flatten() {
            let path = entry.path();
            // `<pid>.json` only; `<pid>.<hash>.key` files hold secrets and are never opened.
            let is_entry = path.extension().is_some_and(|e| e == "json")
                && path.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()));
            if !is_entry {
                continue;
            }
            let entry = match read_registry_entry(&path) {
                Entry::Found(entry) => entry,
                Entry::Gone => continue, // the session ended after the listing
                Entry::Unreadable => {
                    unreadable += 1;
                    continue;
                }
            };
            if !is_alive(probe, entry.pid, entry.proc_start.as_deref()) {
                continue; // The process ended or crashed, or a new process reuses its ID.
            }
            live_sessions.insert(entry.session_id);
            match entry.status.as_deref() {
                Some("busy") => counts.running += 1,
                Some("waiting") => counts.waiting += 1,
                // `shell`: idle while a background shell command runs. No turn runs.
                Some("idle") | Some("shell") => counts.idle += 1,
                _ => {
                    counts.idle += 1;
                    unknown_status += 1;
                }
            }
        }
        for (path, file) in &self.state.files {
            let Some(parent) = child_parent_session(path) else { continue };
            if !live_sessions.contains(parent) || file.finished {
                continue;
            }
            if file.cursor.mtime_ns / 1_000_000 >= now_ms - CHILD_STALL_MS {
                counts.running += 1;
            }
        }
        if unreadable > 0 {
            health.degrade(format!("{unreadable} registry entries unreadable"));
        }
        if unknown_status > 0 {
            health.degrade(format!("{unknown_status} sessions with an unknown status counted as idle"));
        }
        (Some(counts), health)
    }
}

enum Entry {
    Found(RegistryEntry),
    Gone,
    Unreadable,
}

/// Claude Code rewrites a registry file in place when the status changes, and deletes it when the
/// session exits. A read can land in the middle of either, so a deleted file is skipped and a
/// half-written one gets one more try.
fn read_registry_entry(path: &Path) -> Entry {
    for attempt in 0..2 {
        match fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Entry::Gone,
            Ok(bytes) => {
                if let Ok(entry) = serde_json::from_slice::<RegistryEntry>(&bytes) {
                    return Entry::Found(entry);
                }
            }
            Err(_) => {}
        }
        if attempt == 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Entry::Unreadable
}

fn parse_line(bytes: &[u8], window_start: i64, usage: &mut Vec<UsageRecord>, finished: &mut bool, c: &mut Counters) {
    let Ok(record) = serde_json::from_slice::<Record>(bytes) else {
        c.malformed += 1;
        return;
    };
    if record.kind.as_deref() != Some("assistant") {
        *finished = false;
        return;
    }
    let Some(message) = record.message else {
        c.missing_fields += 1;
        return;
    };
    *finished = matches!(
        message.stop_reason.as_deref(),
        Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")
    );
    let Some(u) = message.usage else { return };
    if message.model.as_deref() == Some("<synthetic>") {
        return; // Local placeholder messages report zero usage.
    }
    let (Some(id), Some(request_id), Some(at_ms), Some(input), Some(output)) = (
        message.id.as_deref(),
        record.request_id.as_deref(),
        record.timestamp.as_deref().and_then(time::parse_ms),
        u.input_tokens,
        u.output_tokens,
    ) else {
        c.missing_fields += 1;
        return;
    };
    if at_ms < window_start {
        return;
    }
    // Anthropic reports uncached input, cache writes and cache reads separately: they add up.
    let cache_write = u.cache_creation_input_tokens.unwrap_or(0);
    let cache_read = u.cache_read_input_tokens.unwrap_or(0);
    usage.push(UsageRecord {
        key: request_key(&["claude", id, request_id]),
        at_ms,
        tokens: Tokens { input: input + cache_write + cache_read, cached_input: cache_read, output },
    });
}

fn is_alive(probe: &dyn Probe, pid: i32, proc_start: Option<&str>) -> bool {
    let Some(started) = probe.start_time(pid) else { return false };
    match proc_start.and_then(time::parse_lstart_utc) {
        Some(registered) => (started - registered).abs() <= 1,
        None => true,
    }
}

fn is_child(path: &Path) -> bool {
    child_parent_session(path).is_some()
}

/// For `.../<session>/subagents/agent-<id>.jsonl`, the parent session ID.
fn child_parent_session(path: &Path) -> Option<&str> {
    let dir = path.parent()?;
    if dir.file_name()? != "subagents" {
        return None;
    }
    dir.parent()?.file_name()?.to_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crate::system::fake::FakeProbe;

    const NOW: i64 = 1_790_967_600_000; // 2026-10-02T19:00:00Z

    fn adapter(home: &Path) -> ClaudeAdapter {
        ClaudeAdapter::new(home.to_path_buf(), None)
    }

    #[test]
    fn counts_busy_sessions_and_running_children_only() {
        let home = temp_home("claude-count");
        let mut probe = FakeProbe::default();
        // Two busy sessions, one waiting, one idle, one `shell`.
        for (pid, sid, status) in [(101, "s-busy-1", "busy"), (102, "s-busy-2", "busy"), (103, "s-wait", "waiting"), (104, "s-idle", "idle"), (105, "s-shell", "shell")] {
            claude_registry(&home, pid, sid, status, START);
            probe.starts.insert(pid, START);
        }
        // Children of a busy session: two running, one finished.
        claude_child(&home, "s-busy-1", "agent-a", NOW - 1_000, false);
        claude_child(&home, "s-busy-1", "agent-b", NOW - 2_000, false);
        claude_child(&home, "s-busy-1", "agent-c", NOW - 3_000, true);
        // A background child that keeps running after its parent's turn ended (parent idle).
        claude_child(&home, "s-idle", "agent-d", NOW - 4_000, false);
        let mut a = adapter(&home);
        let r = a.poll(&probe, NOW, NOW - 86_400_000);
        let counts = r.agents.unwrap();
        assert_eq!((counts.running, counts.waiting, counts.idle), (2 + 2 + 1, 1, 2));
        assert_canary_free(&format!("{r:?}"));
    }

    #[test]
    fn dead_reused_or_stalled_agents_do_not_count() {
        let home = temp_home("claude-dead");
        let mut probe = FakeProbe::default();
        claude_registry(&home, 201, "s-dead", "busy", START); // process gone
        claude_registry(&home, 202, "s-reused", "busy", START);
        probe.starts.insert(202, START + 5_000); // same PID, different process
        claude_registry(&home, 203, "s-live", "busy", START);
        probe.starts.insert(203, START);
        claude_child(&home, "s-dead", "agent-orphan", NOW - 1_000, false); // parent crashed
        claude_child(&home, "s-live", "agent-stalled", NOW - CHILD_STALL_MS - 1_000, false);
        let r = adapter(&home).poll(&probe, NOW, NOW - 86_400_000);
        assert_eq!(r.agents.unwrap().running, 1);
    }

    #[test]
    fn an_ended_session_is_skipped_but_a_broken_entry_degrades() {
        let home = temp_home("claude-registry");
        let mut probe = FakeProbe::default();
        claude_registry(&home, 401, "s-live", "busy", START);
        probe.starts.insert(401, START);
        // Deleted between the listing and the read: a dangling link reads as "not found".
        std::os::unix::fs::symlink(home.join("sessions/gone.json"), home.join("sessions/402.json")).unwrap();
        let r = adapter(&home).poll(&probe, NOW, 0);
        assert_eq!((r.agents.unwrap().running, r.agents_health.health), (1, crate::model::Health::Ok));
        fs::write(home.join("sessions/403.json"), "{\"pid\": 403, \"sessionId\"").unwrap();
        let r = adapter(&home).poll(&probe, NOW, 0);
        assert_eq!(r.agents_health.health, crate::model::Health::Degraded);
        assert_eq!(r.agents_health.notes, vec!["1 registry entries unreadable".to_string()]);
    }

    #[test]
    fn usage_is_normalised_and_streamed_repeats_keep_one_key() {
        let home = temp_home("claude-usage");
        let path = claude_transcript(&home, "s-1");
        append(&path, &claude_assistant("msg_1", "req_1", "2026-10-02T18:00:00Z", (3, 100, 1000, 2), None));
        append(&path, &claude_assistant("msg_1", "req_1", "2026-10-02T18:00:01Z", (3, 100, 1000, 50), Some("end_turn")));
        append(&path, &claude_user("2026-10-02T18:00:02Z"));
        append(&path, &claude_synthetic("2026-10-02T18:00:03Z"));
        let r = adapter(&home).poll(&FakeProbe::default(), NOW, NOW - 86_400_000);
        assert_eq!(r.usage.len(), 2, "both repeats are reported; the ledger keeps the maximum");
        assert_eq!(r.usage[0].key, r.usage[1].key);
        assert_eq!(r.usage[1].tokens, Tokens { input: 1103, cached_input: 1000, output: 50 });
        assert_eq!(r.usage_health, SourceHealth::ok());
    }

    #[test]
    fn restart_resumes_from_the_cursor_and_bad_lines_degrade_safely() {
        let home = temp_home("claude-restart");
        let path = claude_transcript(&home, "s-1");
        append(&path, &claude_assistant("msg_1", "req_1", "2026-10-02T18:00:00Z", (1, 0, 0, 1), Some("end_turn")));
        let mut a = adapter(&home);
        assert_eq!(a.poll(&FakeProbe::default(), NOW, 0).usage.len(), 1);
        let saved = serde_json::to_string(a.state()).unwrap();
        assert_no_content(&saved);
        // Restart: only the new lines are read.
        append(&path, "{not json and not an assistant line: never parsed, never counted\n");
        append(&path, "{\"message\":{\"role\":\"assistant\" broken\n");
        append(&path, r#"{"type":"assistant","message":{"role":"assistant","id":"msg_2","usage":{"input_tokens":1}}}"#);
        append(&path, "\n");
        append(&path, &claude_assistant("msg_3", "req_3", "2026-10-02T18:05:00Z", (1, 0, 0, 1), Some("end_turn")));
        let mut b = ClaudeAdapter::new(home.clone(), serde_json::from_str(&saved).ok());
        let r = b.poll(&FakeProbe::default(), NOW, 0);
        assert_eq!(r.usage.len(), 1);
        assert_eq!(r.usage_health.health, crate::model::Health::Degraded);
        assert_eq!(r.usage_health.notes.len(), 2);
        assert_canary_free(&format!("{r:?}"));
    }

    #[test]
    fn old_usage_is_outside_the_window_and_missing_home_is_unavailable() {
        let home = temp_home("claude-window");
        let path = claude_transcript(&home, "s-1");
        append(&path, &claude_assistant("msg_old", "req_old", "2026-09-30T18:00:00Z", (1, 0, 0, 1), Some("end_turn")));
        append(&path, &claude_assistant("msg_new", "req_new", "2026-10-02T18:00:00Z", (1, 0, 0, 1), Some("end_turn")));
        let window = time::parse_ms("2026-10-01T07:00:00Z").unwrap();
        assert_eq!(adapter(&home).poll(&FakeProbe::default(), NOW, window).usage.len(), 1);
        let r = adapter(&home.join("missing")).poll(&FakeProbe::default(), NOW, window);
        assert!(r.agents.is_none());
        assert_eq!(r.usage_health.health, crate::model::Health::Unavailable);
    }
}
