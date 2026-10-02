//! agent-heartbeat: how many Claude Code and Codex agents run now, and how many tokens they used
//! today. Only aggregate counts leave the machine. See docs/DESIGN.md.

mod claude;
mod codex;
mod config;
mod daemon;
mod engine;
#[cfg(test)]
mod fixtures;
mod hash;
mod launchd;
mod ledger;
mod logfile;
mod model;
mod secret;
mod sender;
mod system;
mod tail;
mod time;

use std::io::{IsTerminal, Read};

use config::Paths;
use engine::Engine;
use model::{Coverage, Health};

const HELP: &str = "agent-heartbeat: reports how many Claude Code and Codex agents run now, and the tokens they used today.
Only aggregate counts leave this Mac.

Usage: agent-heartbeat <command>

Commands:
  status            Show the local snapshot and the health of each source. Sends nothing.
  dry-run           Print the exact JSON a heartbeat would send. Sends nothing.
  start             Start the tracker in the background.
  stop              Stop the tracker. It sends a final heartbeat with a lease of 0.
  run               Run the tracker in the foreground (launchd uses this).
  secret generate   Make a random secret, store it in the Keychain, and print the hash for the site.
  secret set        Store a secret that you type or pipe in.
  secret status     Say whether a secret is stored.
  secret delete     Remove the secret from the Keychain.
  launchd           Print a per-user LaunchAgent file. It installs nothing.
  version           Print the version.

Files:
  ~/Library/Application Support/agent-heartbeat/   config.json, state.json (private, mode 0600)
  ~/Library/Logs/agent-heartbeat/                   agent-heartbeat.log (operational summaries)
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = cli(&args) {
        eprintln!("agent-heartbeat: {e}");
        std::process::exit(1);
    }
}

fn cli(args: &[String]) -> Result<(), String> {
    let paths = Paths::from_env();
    match args.first().map(String::as_str) {
        Some("run") => daemon::run(&paths, &config::load(&paths)?),
        Some("start") => daemon::start(&paths),
        Some("stop") => daemon::stop(&paths),
        Some("status") => status(&paths),
        Some("dry-run") => dry_run(&paths),
        Some("secret") => secret_command(args.get(1).map(String::as_str)),
        Some("launchd") => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            print!("{}", launchd::plist(&exe.to_string_lossy()));
            Ok(())
        }
        Some("version" | "--version" | "-V") => {
            println!("agent-heartbeat {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None | Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command '{other}'. Run: agent-heartbeat help")),
    }
}

/// A fresh observation that starts from the saved state but does not change it.
fn observe_once(paths: &Paths) -> Result<(config::Config, Engine, i64), String> {
    let config = config::load(paths)?;
    let now = time::now_ms();
    let mut engine = Engine::new(&config, paths, Box::new(system::LibProc), engine::load_state(paths), now)?;
    engine.observe(now);
    Ok((config, engine, now))
}

fn dry_run(paths: &Paths) -> Result<(), String> {
    let (config, engine, now) = observe_once(paths)?;
    let active = engine.agents().active.unwrap_or(0) > 0;
    let interval = if active { config.active_interval_seconds } else { config.idle_interval_seconds };
    let ttl = (3 * interval).min(300) as u32;
    println!("{}", serde_json::to_string(&engine.heartbeat(now, ttl)).map_err(|e| e.to_string())?);
    Ok(())
}

fn status(paths: &Paths) -> Result<(), String> {
    let (config, engine, now) = observe_once(paths)?;
    let day = engine.zone.date(now);
    let agents = engine.agents();
    let tokens = engine.tokens(&day);
    println!("agent-heartbeat {} · local status · nothing is sent by this command\n", env!("CARGO_PKG_VERSION"));

    let tracker = match daemon::running_pid(paths) {
        Some(pid) => format!("running (pid {pid})"),
        None => "not running".to_string(),
    };
    println!("  Tracker       {tracker}");
    if let Some(last) = daemon::read_last_send(&paths.last_send()) {
        let age = (now - last["atMs"].as_i64().unwrap_or(now)) / 1000;
        println!("  Last send     {} s ago: {}", age, last["outcome"].as_str().unwrap_or("?"));
    }
    let active = agents.active.map_or("unknown".to_string(), |n| n.to_string());
    println!("  Active now    {active} agents ({})", coverage_word(agents.coverage));
    for r in &engine.last {
        match r.agents {
            Some(a) => println!("                {:<7} {} running · {} waiting · {} idle", r.provider.name(), a.running, a.waiting, a.idle),
            None => println!("                {:<7} unavailable", r.provider.name()),
        }
    }
    let total = tokens.total.map_or("unknown".to_string(), compact);
    println!("  Tokens today  {total} processed on {day} ({}, {})", engine.zone.name, coverage_word(tokens.coverage));
    if let (Some(cached), Some(output)) = (tokens.cached_input, tokens.output) {
        println!("                of which {} cached input · {} output", compact(cached), compact(output));
    }
    for (provider, t) in engine.per_provider(&day) {
        println!("                {:<7} {}", provider.name(), compact(t.total()));
    }
    println!("  Sources");
    for r in &engine.last {
        for (what, h) in [("live state", &r.agents_health), ("usage", &r.usage_health)] {
            let word = match h.health {
                Health::Ok => "ok",
                Health::Degraded => "degraded",
                Health::Unavailable => "unavailable",
            };
            let notes = if h.notes.is_empty() { String::new() } else { format!(": {}", h.notes.join("; ")) };
            println!("                {:<7} {what} {word}{notes}", r.provider.name());
        }
    }
    let endpoint = config.endpoint.as_deref().unwrap_or("none (nothing is sent)");
    println!("  Endpoint      {endpoint}");
    let secret = match &config.secret_file {
        Some(_) => "file".to_string(),
        None => match secret::read_keychain() {
            Ok(Some(_)) => "stored in the Keychain".to_string(),
            Ok(None) => "not set (run: agent-heartbeat secret generate)".to_string(),
            Err(e) => e,
        },
    };
    println!("  Secret        {secret}");
    Ok(())
}

fn coverage_word(c: Coverage) -> &'static str {
    match c {
        Coverage::Complete => "complete coverage",
        Coverage::Partial => "partial coverage: a known subtotal",
        Coverage::Unavailable => "no source available",
    }
}

/// 755312345 → "755.3M", 128400 → "128.4k".
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

fn secret_command(sub: Option<&str>) -> Result<(), String> {
    match sub {
        Some("generate") => {
            let s = secret::generate()?;
            secret::store(&s)?;
            println!("A new secret is stored in your login Keychain (service \"agent-heartbeat\").");
            println!("Give the site only this SHA-256 hash, as WORKSTATION_INGEST_SECRET_SHA256:\n");
            println!("{}", secret::fingerprint(&s));
            Ok(())
        }
        Some("set") => {
            let s = read_secret_input()?;
            secret::store(&s)?;
            println!("Stored. SHA-256 for the site: {}", secret::fingerprint(&s));
            Ok(())
        }
        Some("status") => {
            match secret::read_keychain()? {
                Some(s) => println!("A secret is stored. SHA-256: {}", secret::fingerprint(&s)),
                None => println!("No secret is stored."),
            }
            Ok(())
        }
        Some("delete") => {
            println!("{}", if secret::delete()? { "Deleted." } else { "No secret was stored." });
            Ok(())
        }
        _ => Err("use: agent-heartbeat secret generate | set | status | delete".into()),
    }
}

/// Reads a secret from stdin. On a terminal, echo is off while you type.
fn read_secret_input() -> Result<String, String> {
    let stdin = std::io::stdin();
    let mut text = String::new();
    if stdin.is_terminal() {
        eprint!("Secret (input hidden): ");
        let fd = 0;
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::tcgetattr(fd, &mut term) };
        let saved = term;
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
        let result = stdin.read_line(&mut text);
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        eprintln!();
        result.map_err(|e| e.to_string())?;
    } else {
        stdin.lock().read_to_string(&mut text).map_err(|e| e.to_string())?;
    }
    let s = text.trim().to_string();
    if s.len() < 32 {
        return Err("use a secret of at least 32 characters (agent-heartbeat secret generate makes one)".into());
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn compact_numbers_match_the_panel() {
        assert_eq!(super::compact(999), "999");
        assert_eq!(super::compact(128_400), "128.4k");
        assert_eq!(super::compact(755_312_345), "755.3M");
    }
}
