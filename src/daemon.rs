//! The tracker process: wake on file events or a timer, observe, send when a heartbeat is due.
//!
//! When no agent writes, the process sleeps until the next keepalive. It starts no child processes.

use std::{
    fs,
    io::{Read, Seek, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    config::{self, Config, Paths},
    engine::{self, Engine},
    logfile,
    model::Provider,
    secret,
    sender::{self, Outcome, Sender},
    system::LibProc,
    time,
};

enum Event {
    Files,
    Stop,
}

/// File events are combined: at most one observation every 5 s. One observation costs a few
/// milliseconds, so this keeps CPU near 0.17% of one core while agents write. With the panel's 5 s
/// checks and a 2 s status cache, a change reaches a visitor within about 12 s (docs/CONTRACT.md).
const MIN_OBSERVE_GAP: Duration = Duration::from_secs(5);
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(300);
const SAVE_EVERY: Duration = Duration::from_secs(600);
const MAX_TTL_SECONDS: u64 = 300;
/// The wall clock moved this much more than the monotonic clock: the Mac slept.
const SLEEP_GAP_MS: i64 = 5_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastSend {
    pub at_ms: i64,
    pub outcome: String,
    pub active_agents: Option<u32>,
    pub consecutive_failures: u32,
}

pub fn run(paths: &Paths, config: &Config) -> Result<(), String> {
    paths.ensure_private_dirs().map_err(|e| format!("cannot create private folders: {e}"))?;
    let _lock = acquire_lock(paths)?;
    let (tx, rx) = mpsc::channel();
    forward_signals(tx.clone())?;
    let _watcher = watch(paths, config, tx);
    let log = |m: &str| logfile::log(&paths.log(), m);

    let mut engine = Engine::new(config, paths, Box::new(LibProc), engine::load_state(paths), time::now_ms())?;
    let sender = make_sender(config, &log);
    log(&format!(
        "start: version {}, sources {}, sending {}",
        env!("CARGO_PKG_VERSION"),
        config.sources.iter().map(|p| p.name()).collect::<Vec<_>>().join("+"),
        if sender.is_some() { "on" } else { "off" }
    ));

    let mut rng = seed();
    let mut next_send = Instant::now();
    let mut retry: Option<(Instant, u32)> = None;
    let mut unauthorized_until: Option<Instant> = None;
    let mut last_sent_active: Option<Option<u32>> = None;
    let mut last_outcome: Option<Outcome> = None;
    let mut pending_files = false;
    let mut last_observe = Instant::now() - MIN_OBSERVE_GAP;
    let (mut last_wall, mut last_mono) = (time::now_ms(), Instant::now());
    let mut last_save = Instant::now();
    let mut last_day = engine.zone.date(time::now_ms());
    let mut last_health = String::new();

    loop {
        let mut deadline = next_send;
        if pending_files {
            deadline = deadline.min(last_observe + MIN_OBSERVE_GAP);
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Event::Files) => {
                pending_files = true;
                if rx.try_iter().any(|e| matches!(e, Event::Stop)) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let (now, now_ms) = (Instant::now(), time::now_ms());
        let woke = (now_ms - last_wall) - (now - last_mono).as_millis() as i64 > SLEEP_GAP_MS;
        (last_wall, last_mono) = (now_ms, now);
        if woke {
            log("woke from sleep: sending a new observation");
        }
        let timer_due = now >= next_send;
        if !timer_due && !woke && now < last_observe + MIN_OBSERVE_GAP {
            continue;
        }

        engine.observe(now_ms);
        pending_files = false;
        last_observe = now;
        let health = health_summary(&engine);
        if health != last_health {
            log(&format!("sources: {health}"));
            last_health = health;
        }

        let agents = engine.agents();
        let interval = Duration::from_secs(if agents.active.unwrap_or(0) > 0 {
            config.active_interval_seconds
        } else {
            config.idle_interval_seconds
        });
        let changed = last_sent_active != Some(agents.active);
        let blocked = retry.is_some_and(|(at, _)| now < at) || unauthorized_until.is_some_and(|u| now < u);
        if let Some(sender) = sender.as_ref().filter(|_| (timer_due || woke || changed) && !blocked) {
            let ttl = (3 * interval.as_secs()).min(MAX_TTL_SECONDS) as u32;
            let body = serde_json::to_vec(&engine.heartbeat(now_ms, ttl)).unwrap_or_default();
            let outcome = sender.send(&body);
            let failures = retry.map_or(0, |(_, n)| n);
            match outcome {
                Outcome::Accepted | Outcome::Stale => {
                    last_sent_active = Some(agents.active);
                    retry = None;
                    unauthorized_until = None;
                    next_send = now + interval;
                    engine.mark_sent(&engine.zone.date(now_ms));
                    if let Some(day) = engine.correction_due(now_ms) {
                        let body = serde_json::to_vec(&engine.daily_usage(now_ms, &day)).unwrap_or_default();
                        if matches!(sender.send(&body), Outcome::Accepted | Outcome::Stale) {
                            engine.mark_sent(&day);
                            log("sent a correction for yesterday's totals");
                        }
                    }
                }
                Outcome::Unauthorized => {
                    unauthorized_until = Some(now + UNAUTHORIZED_RETRY);
                    next_send = now + UNAUTHORIZED_RETRY;
                }
                Outcome::RetryLater(after) => {
                    let attempt = failures + 1;
                    let wait = sender::backoff(attempt, after, next_random(&mut rng));
                    retry = Some((now + wait, attempt));
                    next_send = now + wait;
                }
                Outcome::Rejected(_) => {
                    retry = None;
                    next_send = now + interval;
                }
            }
            if last_outcome != Some(outcome) {
                log(&format!("send: {}", outcome.describe()));
                last_outcome = Some(outcome);
            }
            let record = LastSend {
                at_ms: now_ms,
                outcome: outcome.describe(),
                active_agents: agents.active,
                consecutive_failures: retry.map_or(0, |(_, n)| n),
            };
            let _ = config::write_private(&paths.last_send(), &serde_json::to_vec(&record).unwrap_or_default());
        } else if timer_due && !blocked {
            next_send = now + interval;
        }

        let day = engine.zone.date(now_ms);
        if now.duration_since(last_save) >= SAVE_EVERY || day != last_day {
            save(paths, &engine, &log);
            last_save = now;
            last_day = day;
        }
    }

    // Clean stop: a lease of 0 tells the site at once that the tracker is gone.
    if let Some(sender) = sender.as_ref() {
        engine.observe(time::now_ms());
        let body = serde_json::to_vec(&engine.heartbeat(time::now_ms(), 0)).unwrap_or_default();
        sender.send(&body);
    }
    save(paths, &engine, &log);
    log("stop");
    Ok(())
}

fn save(paths: &Paths, engine: &Engine, log: &dyn Fn(&str)) {
    if let Err(e) = config::write_private(&paths.state(), engine.export_state().as_bytes()) {
        log(&format!("state not saved: {}", e.kind()));
    }
}

fn make_sender(config: &Config, log: &dyn Fn(&str)) -> Option<Sender> {
    let endpoint = config.endpoint.clone()?;
    let secret = match &config.secret_file {
        Some(path) => secret::read_file(path),
        None => secret::read_keychain().and_then(|s| s.ok_or_else(|| "no secret in the Keychain".to_string())),
    };
    match secret {
        Ok(s) => Some(Sender::new(endpoint, &s)),
        Err(e) => {
            log(&format!("not sending: {e}"));
            None
        }
    }
}

fn health_summary(engine: &Engine) -> String {
    engine
        .last
        .iter()
        .map(|r| {
            // Notes are fixed phrases with numbers only, never a path, a name or content.
            let notes: Vec<&str> = r.agents_health.notes.iter().chain(&r.usage_health.notes).map(String::as_str).collect();
            let detail = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
            format!("{} agents {:?} usage {:?}{detail}", r.provider.name(), r.agents_health.health, r.usage_health.health).to_lowercase()
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn watch(paths: &Paths, config: &Config, tx: mpsc::Sender<Event>) -> Option<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok_and(|e| !matches!(e.kind, EventKind::Access(_))) {
            let _ = tx.send(Event::Files);
        }
    })
    .ok()?;
    let mut dirs = Vec::new();
    if config.sources.contains(&Provider::Claude) {
        dirs.push((paths.claude_home.join("sessions"), RecursiveMode::NonRecursive));
        dirs.push((paths.claude_home.join("projects"), RecursiveMode::Recursive));
    }
    if config.sources.contains(&Provider::Codex) {
        dirs.push((paths.codex_home.join("sessions"), RecursiveMode::Recursive));
        dirs.push((paths.codex_home.join("archived_sessions"), RecursiveMode::NonRecursive));
        dirs.push((paths.codex_home.join("thread-writer-locks"), RecursiveMode::NonRecursive));
    }
    for (dir, mode) in dirs {
        if dir.is_dir() {
            let _ = watcher.watch(&dir, mode);
        }
    }
    Some(watcher)
}

/// Blocks SIGTERM, SIGINT and SIGHUP in every thread, and turns them into a Stop event on one
/// thread that waits in sigwait. Call before any other thread starts.
fn forward_signals(tx: mpsc::Sender<Event>) -> Result<(), String> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err("cannot set the signal mask".into());
        }
        std::thread::spawn(move || {
            let mut sig = 0;
            libc::sigwait(&set, &mut sig);
            let _ = tx.send(Event::Stop);
        });
    }
    Ok(())
}

/// Holds an exclusive lock on the PID file for the life of the process, so only one tracker runs.
fn acquire_lock(paths: &Paths) -> Result<fs::File, String> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.pid())
        .map_err(|e| format!("cannot open the PID file: {e}"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another agent-heartbeat is already running".into());
    }
    file.set_len(0).and_then(|_| file.rewind()).and_then(|_| write!(file, "{}", std::process::id())).map_err(|e| e.to_string())?;
    Ok(file)
}

/// The PID of the running tracker, if one holds the lock.
pub fn running_pid(paths: &Paths) -> Option<i32> {
    let mut file = fs::File::open(paths.pid()).ok()?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

pub fn start(paths: &Paths) -> Result<(), String> {
    if let Some(pid) = running_pid(paths) {
        println!("agent-heartbeat is already running (pid {pid}).");
        return Ok(());
    }
    config::load(paths)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command.arg("run").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map_err(|e| format!("cannot start: {e}"))?;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(pid) = running_pid(paths) {
            println!("agent-heartbeat started (pid {pid}). Log: {}", paths.log().display());
            return Ok(());
        }
    }
    Err(format!("the tracker did not start; see {}", paths.log().display()))
}

pub fn stop(paths: &Paths) -> Result<(), String> {
    let Some(pid) = running_pid(paths) else {
        println!("agent-heartbeat is not running.");
        return Ok(());
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if running_pid(paths).is_none() {
            println!("agent-heartbeat stopped.");
            return Ok(());
        }
    }
    Err(format!("pid {pid} did not stop within 10 s"))
}

pub fn read_last_send(paths: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&fs::read(paths).ok()?).ok()
}

fn seed() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    t ^ ((std::process::id() as u64) << 32) | 1
}

/// xorshift64: enough randomness for retry jitter.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
