//! Synthetic Claude and Codex files for tests. No real transcript is ever used or committed.
//!
//! Every field that holds content in the real formats (prompts, replies, tool input and output,
//! working folders, branch and session names) holds `CANARY`. Folder names hold `CANARY_PATH`.
//! Tests assert that neither string reaches a reading, a payload or a log.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

pub const CANARY: &str = "CANARY-CONTENT-7f3a";
pub const CANARY_PATH: &str = "CANARY-PATH-91c2";
/// A process start time, in epoch seconds (2026-10-01T08:11:20Z).
pub const START: i64 = 1_790_928_680;
pub const THREAD_A: &str = "01a0aaaa-0000-7000-8000-00000000000a";
pub const THREAD_B: &str = "01a0bbbb-0000-7000-8000-00000000000b";
pub const THREAD_C: &str = "01a0cccc-0000-7000-8000-00000000000c";
pub const THREAD_D: &str = "01a0dddd-0000-7000-8000-00000000000d";

pub fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ah-fixture-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn append(path: &Path, text: &str) {
    fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
}

pub fn set_mtime(path: &Path, ms: i64) {
    let t = UNIX_EPOCH + Duration::from_millis(ms as u64);
    fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

/// Content markers must never appear. Use for readings, payloads and logs.
pub fn assert_canary_free(text: &str) {
    assert!(!text.contains(CANARY), "content leaked: {text}");
    assert!(!text.contains(CANARY_PATH), "a path leaked: {text}");
}

/// Private local state may hold file paths, but never content.
pub fn assert_no_content(text: &str) {
    assert!(!text.contains(CANARY), "content leaked into private state");
}

fn lstart(secs: i64) -> String {
    jiff::Timestamp::from_second(secs).unwrap().strftime("%a %b %e %H:%M:%S %Y").to_string()
}

// ---------- Claude ----------

pub fn claude_registry(home: &Path, pid: i32, session: &str, status: &str, start: i64) {
    let dir = home.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let entry = serde_json::json!({
        "pid": pid, "sessionId": session, "cwd": format!("/Users/x/{CANARY}"), "startedAt": 1,
        "procStart": lstart(start), "version": "2.1.285", "kind": "interactive",
        "entrypoint": "claude-desktop", "name": CANARY, "status": status, "statusUpdatedAt": 1,
        "messagingSocketPath": format!("/tmp/{CANARY}.sock")
    });
    fs::write(dir.join(format!("{pid}.json")), entry.to_string()).unwrap();
    fs::write(dir.join(format!("{pid}.abc123.key")), format!("{{\"peerToken\":\"{CANARY}\"}}")).unwrap();
}

fn project_dir(home: &Path) -> PathBuf {
    let dir = home.join("projects").join(format!("-Users-x-{CANARY_PATH}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn claude_transcript(home: &Path, session: &str) -> PathBuf {
    let path = project_dir(home).join(format!("{session}.jsonl"));
    append(&path, &claude_user("2026-10-02T17:59:00Z"));
    path
}

pub fn claude_child(home: &Path, session: &str, agent: &str, mtime_ms: i64, finished: bool) -> PathBuf {
    let dir = project_dir(home).join(session).join("subagents");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{agent}.jsonl"));
    append(&path, &claude_user("2026-10-02T18:00:00Z"));
    let stop = if finished { "end_turn" } else { "tool_use" };
    append(&path, &claude_assistant(&format!("msg_{agent}"), &format!("req_{agent}"), "2026-10-02T18:00:01Z", (3, 10, 100, 5), Some(stop)));
    if !finished {
        append(&path, &claude_user("2026-10-02T18:00:02Z")); // a tool result: the child still works
    }
    set_mtime(&path, mtime_ms);
    path
}

/// usage = (input_tokens, cache_creation_input_tokens, cache_read_input_tokens, output_tokens)
pub fn claude_assistant(id: &str, request: &str, at: &str, usage: (u64, u64, u64, u64), stop: Option<&str>) -> String {
    let (input, write, read, output) = usage;
    let line = serde_json::json!({
        "parentUuid": "p", "isSidechain": false,
        "message": {
            "model": "claude-opus-5-5", "id": id, "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": CANARY}, {"type": "tool_use", "input": {"command": CANARY}}],
            "stop_reason": stop,
            "usage": {"input_tokens": input, "cache_creation_input_tokens": write, "cache_read_input_tokens": read,
                      "output_tokens": output, "service_tier": "standard"}
        },
        "requestId": request, "type": "assistant", "uuid": "u", "timestamp": at,
        "cwd": format!("/Users/x/{CANARY}"), "gitBranch": CANARY, "sessionId": "s", "slug": CANARY
    });
    format!("{line}\n")
}

pub fn claude_user(at: &str) -> String {
    let line = serde_json::json!({
        "parentUuid": "p", "isSidechain": false, "type": "user",
        "message": {"role": "user", "content": [{"type": "tool_result", "content": CANARY}]},
        "timestamp": at, "cwd": format!("/Users/x/{CANARY}")
    });
    format!("{line}\n")
}

pub fn claude_synthetic(at: &str) -> String {
    let line = serde_json::json!({
        "type": "assistant", "requestId": "r",
        "message": {"model": "<synthetic>", "id": "m", "role": "assistant", "content": [{"type": "text", "text": CANARY}],
                    "usage": {"input_tokens": 0, "output_tokens": 0}},
        "timestamp": at
    });
    format!("{line}\n")
}

// ---------- Codex ----------

pub fn codex_rollout(home: &Path, thread: &str, child: bool) -> PathBuf {
    let dir = home.join("sessions/2026/10/02");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(home.join("thread-writer-locks")).unwrap();
    fs::write(home.join(format!("thread-writer-locks/{thread}.lock")), b"").unwrap();
    let path = dir.join(format!("rollout-2026-10-02T10-00-00-{thread}.jsonl"));
    let source = if child { serde_json::json!({"subagent": {"other": CANARY}}) } else { serde_json::json!("vscode") };
    let meta = serde_json::json!({
        "timestamp": "2026-10-02T17:00:00.000Z", "ordinal": 0, "type": "session_meta",
        "payload": {"id": thread, "cwd": format!("/Users/x/{CANARY}"), "originator": "codex_work_desktop",
                    "source": source, "base_instructions": {"text": CANARY}}
    });
    append(&path, &format!("{meta}\n"));
    path
}

pub fn codex_event(kind: &str, turn: &str) -> String {
    let line = serde_json::json!({
        "timestamp": "2026-10-02T18:00:00.000Z", "ordinal": 1, "type": "event_msg",
        "payload": {"type": kind, "turn_id": turn, "last_agent_message": CANARY}
    });
    format!("{line}\n")
}

pub fn codex_usage(response: &str, at: &str, input: u64, cached: u64, output: u64) -> String {
    let u = serde_json::json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
                               "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output});
    let line = serde_json::json!({
        "timestamp": at, "ordinal": 2, "type": "token_usage_record",
        "payload": {"thread_id": "t", "turn_id": CANARY, "response_id": response, "usage": u,
                    "turn_token_usage": u, "thread_token_usage": u}
    });
    format!("{line}\n")
}

pub fn codex_token_count(input: u64, cached: u64, output: u64) -> String {
    let u = serde_json::json!({"input_tokens": input, "cached_input_tokens": cached, "output_tokens": output, "total_tokens": input + output});
    let line = serde_json::json!({
        "timestamp": "2026-10-02T18:00:00.000Z", "ordinal": 3, "type": "event_msg",
        "payload": {"type": "token_count", "info": {"total_token_usage": u, "last_token_usage": u}}
    });
    format!("{line}\n")
}
