//! Joins the adapters, the ledger and the payload. The payload types below are the allowlist:
//! a field cannot leave the machine unless it is declared here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    claude::{ClaudeAdapter, ClaudeState},
    codex::{CodexAdapter, CodexState},
    config::{Config, FirstRun, Paths},
    ledger::Ledger,
    model::{Coverage, Health, Provider, Reading, Tokens},
    system::Probe,
    time::{self, Zone},
};

// ---------- The payload (contract/heartbeat.v1.schema.json) ----------

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat<'a> {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub observed_at: String,
    pub ttl_seconds: u32,
    pub usage_date: String,
    pub time_zone: &'a str,
    pub agents: Agents,
    pub tokens: TokensOut,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsage<'a> {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub observed_at: String,
    pub usage_date: String,
    pub time_zone: &'a str,
    pub tokens: TokensOut,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Agents {
    pub active: Option<u32>,
    pub coverage: Coverage,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokensOut {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
    pub total: Option<u64>,
    pub coverage: Coverage,
}

// ---------- Private state ----------

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// Usage before this instant is never counted. Set on the first run.
    pub count_from_ms: i64,
    pub claude: ClaudeState,
    pub codex: CodexState,
    pub ledger: Ledger,
    /// The day totals that the receiver last accepted, to know when a correction is due.
    #[serde(default)]
    pub sent_days: BTreeMap<String, Tokens>,
}

const STATE_VERSION: u32 = 1;

pub struct Engine {
    pub zone: Zone,
    sources: Vec<Provider>,
    probe: Box<dyn Probe>,
    claude: ClaudeAdapter,
    codex: CodexAdapter,
    pub ledger: Ledger,
    count_from_ms: i64,
    pub sent_days: BTreeMap<String, Tokens>,
    pub last: Vec<Reading>,
}

impl Engine {
    pub fn new(config: &Config, paths: &Paths, probe: Box<dyn Probe>, state: Option<State>, now_ms: i64) -> Result<Engine, String> {
        let zone = Zone::new(&config.time_zone)?;
        let state = state.filter(|s| s.version == STATE_VERSION);
        let count_from_ms = match &state {
            Some(s) => s.count_from_ms,
            None if config.first_run == FirstRun::FromInstall => now_ms,
            None => zone.start_of_day(now_ms),
        };
        let (claude_state, codex_state, ledger, sent_days) = match state {
            Some(s) => (Some(s.claude), Some(s.codex), s.ledger, s.sent_days),
            None => (None, None, Ledger::default(), BTreeMap::new()),
        };
        Ok(Engine {
            zone,
            sources: config.sources.clone(),
            probe,
            claude: ClaudeAdapter::new(paths.claude_home.clone(), claude_state),
            codex: CodexAdapter::new(paths.codex_home.clone(), codex_state),
            ledger,
            count_from_ms,
            sent_days,
            last: Vec::new(),
        })
    }

    /// Reads every configured source once and updates the ledger. Returns true when a total changed.
    pub fn observe(&mut self, now_ms: i64) -> bool {
        let window_start = self.count_from_ms.max(self.zone.start_of_yesterday(now_ms));
        let mut readings = Vec::new();
        for provider in &self.sources {
            readings.push(match provider {
                Provider::Claude => self.claude.poll(self.probe.as_ref(), now_ms, window_start),
                Provider::Codex => self.codex.poll(self.probe.as_ref(), window_start),
            });
        }
        let mut changed = false;
        for reading in &readings {
            for record in &reading.usage {
                if record.at_ms >= window_start {
                    let day = self.zone.date(record.at_ms);
                    changed |= self.ledger.observe(reading.provider, record, &day, now_ms);
                }
            }
        }
        self.ledger.tidy(now_ms, &self.zone.date(self.zone.start_of_yesterday(now_ms)));
        let oldest = self.zone.date(self.zone.start_of_yesterday(now_ms));
        self.sent_days.retain(|day, _| day.as_str() >= oldest.as_str());
        self.last = readings;
        changed
    }

    pub fn agents(&self) -> Agents {
        let known: Vec<_> = self.last.iter().filter_map(|r| r.agents).collect();
        let all_ok = self.last.iter().all(|r| r.agents.is_some() && r.agents_health.health == Health::Ok);
        let coverage = coverage(known.len(), self.last.len(), all_ok);
        let active = (coverage != Coverage::Unavailable).then(|| known.iter().map(|a| a.running).sum());
        Agents { active, coverage }
    }

    pub fn tokens(&self, day: &str) -> TokensOut {
        let mut sum = Tokens::default();
        let mut known = 0;
        for r in &self.last {
            if r.usage_health.health != Health::Unavailable {
                known += 1;
                sum.add(self.ledger.totals(day, r.provider));
            }
        }
        let all_ok = self.last.iter().all(|r| r.usage_health.health == Health::Ok);
        let coverage = coverage(known, self.last.len(), all_ok);
        let some = coverage != Coverage::Unavailable;
        TokensOut {
            input: some.then_some(sum.input),
            cached_input: some.then_some(sum.cached_input),
            output: some.then_some(sum.output),
            total: some.then_some(sum.total()),
            coverage,
        }
    }

    /// A heartbeat for the instant `now_ms`. usageDate and observedAt come from the same instant.
    pub fn heartbeat(&self, now_ms: i64, ttl_seconds: u32) -> Heartbeat<'_> {
        let day = self.zone.date(now_ms);
        Heartbeat {
            schema_version: 1,
            kind: "heartbeat",
            observed_at: time::iso_ms(now_ms),
            ttl_seconds,
            tokens: self.tokens(&day),
            usage_date: day,
            time_zone: &self.zone.name,
            agents: self.agents(),
        }
    }

    pub fn daily_usage(&self, now_ms: i64, day: &str) -> DailyUsage<'_> {
        DailyUsage {
            schema_version: 1,
            kind: "dailyUsage",
            observed_at: time::iso_ms(now_ms),
            usage_date: day.to_string(),
            time_zone: &self.zone.name,
            tokens: self.tokens(day),
        }
    }

    /// Yesterday's totals, when they differ from what the receiver last accepted for that day.
    /// This catches requests that finished after the last heartbeat of the day.
    pub fn correction_due(&self, now_ms: i64) -> Option<String> {
        let yesterday = self.zone.date(self.zone.start_of_yesterday(now_ms));
        let sent = self.sent_days.get(&yesterday)?;
        let current: Tokens = self.last.iter().fold(Tokens::default(), |mut t, r| {
            t.add(self.ledger.totals(&yesterday, r.provider));
            t
        });
        (current != *sent).then_some(yesterday)
    }

    /// Records the totals of `day` that the receiver accepted.
    pub fn mark_sent(&mut self, day: &str) {
        let t = self.last.iter().fold(Tokens::default(), |mut t, r| {
            t.add(self.ledger.totals(day, r.provider));
            t
        });
        self.sent_days.insert(day.to_string(), t);
    }

    pub fn per_provider(&self, day: &str) -> Vec<(Provider, Tokens)> {
        self.last.iter().map(|r| (r.provider, self.ledger.totals(day, r.provider))).collect()
    }

    pub fn export_state(&self) -> String {
        #[derive(Serialize)]
        struct Out<'a> {
            version: u32,
            count_from_ms: i64,
            claude: &'a ClaudeState,
            codex: &'a CodexState,
            ledger: &'a Ledger,
            sent_days: &'a BTreeMap<String, Tokens>,
        }
        serde_json::to_string(&Out {
            version: STATE_VERSION,
            count_from_ms: self.count_from_ms,
            claude: self.claude.state(),
            codex: self.codex.state(),
            ledger: &self.ledger,
            sent_days: &self.sent_days,
        })
        .unwrap_or_default()
    }
}

/// complete: every configured source reports and is healthy. partial: some report, so numbers
/// are a known subtotal. unavailable: none report, so numbers are null.
fn coverage(known: usize, configured: usize, all_ok: bool) -> Coverage {
    match known {
        0 => Coverage::Unavailable,
        n if n == configured && all_ok => Coverage::Complete,
        _ => Coverage::Partial,
    }
}

pub fn load_state(paths: &Paths) -> Option<State> {
    let bytes = std::fs::read(paths.state()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crate::system::fake::FakeProbe;

    const NOW: i64 = 1_790_967_600_000; // 2026-10-02T19:00:00Z = 12:00 PDT

    fn paths(claude: &std::path::Path, codex: &std::path::Path) -> Paths {
        Paths { dir: claude.join("state"), logs: claude.join("logs"), claude_home: claude.into(), codex_home: codex.into() }
    }

    fn setup(name: &str) -> (Paths, FakeProbe) {
        let claude = temp_home(&format!("{name}-claude"));
        let codex = temp_home(&format!("{name}-codex"));
        claude_registry(&claude, 301, "s-1", "busy", START);
        let t = claude_transcript(&claude, "s-1");
        append(&t, &claude_assistant("m1", "r1", "2026-10-02T18:00:00Z", (10, 0, 90, 5), Some("end_turn")));
        append(&t, &claude_assistant("m0", "r0", "2026-10-02T06:00:00Z", (1000, 0, 0, 0), Some("end_turn"))); // 23:00 on 1 Oct
        let f = codex_rollout(&codex, THREAD_A, false);
        append(&f, &codex_event("task_started", "turn-1"));
        append(&f, &codex_usage("resp_1", "2026-10-02T18:00:00Z", 200, 150, 7));
        let mut probe = FakeProbe::default();
        probe.starts.insert(301, START);
        probe.held = Some([format!("{THREAD_A}.lock")].into_iter().collect());
        (paths(&claude, &codex), probe)
    }

    fn engine(p: &Paths, probe: FakeProbe, state: Option<State>, first_run: FirstRun) -> Engine {
        let config = Config { first_run, ..Config::default() };
        Engine::new(&config, p, Box::new(probe), state, NOW).unwrap()
    }

    #[test]
    fn heartbeat_has_only_contract_fields_and_complete_coverage() {
        let (p, probe) = setup("engine-hb");
        let mut e = engine(&p, probe, None, FirstRun::ImportToday);
        e.observe(NOW);
        let json = serde_json::to_string(&e.heartbeat(NOW, 90)).unwrap();
        assert_eq!(
            json,
            r#"{"schemaVersion":1,"type":"heartbeat","observedAt":"2026-10-02T19:00:00.000Z","ttlSeconds":90,"usageDate":"2026-10-02","timeZone":"America/Los_Angeles","agents":{"active":2,"coverage":"complete"},"tokens":{"input":300,"cachedInput":240,"output":12,"total":312,"coverage":"complete"}}"#
        );
        assert_canary_free(&json);
        assert_no_content(&e.export_state());
    }

    #[test]
    fn restart_from_saved_state_does_not_double_count() {
        let (p, probe) = setup("engine-restart");
        let mut e = engine(&p, probe, None, FirstRun::ImportToday);
        e.observe(NOW);
        let saved: State = serde_json::from_str(&e.export_state()).unwrap();
        let (_, probe2) = setup_probe();
        let mut again = engine(&p, probe2, Some(saved), FirstRun::ImportToday);
        again.observe(NOW + 1000);
        assert_eq!(again.tokens("2026-10-02").total, Some(312));
        // A fresh engine with no state imports today again and gets the same total.
        let (_, probe3) = setup_probe();
        let mut fresh = engine(&p, probe3, None, FirstRun::ImportToday);
        fresh.observe(NOW);
        assert_eq!(fresh.tokens("2026-10-02").total, Some(312));
    }

    fn setup_probe() -> ((), FakeProbe) {
        let mut probe = FakeProbe::default();
        probe.starts.insert(301, START);
        probe.held = Some([format!("{THREAD_A}.lock")].into_iter().collect());
        ((), probe)
    }

    #[test]
    fn from_install_counts_nothing_before_the_first_start() {
        let (p, probe) = setup("engine-install");
        let mut e = engine(&p, probe, None, FirstRun::FromInstall);
        e.observe(NOW);
        assert_eq!(e.tokens("2026-10-02").total, Some(0));
    }

    #[test]
    fn one_failed_adapter_gives_partial_coverage_and_a_subtotal() {
        let (p, mut probe) = setup("engine-partial");
        probe.held = None; // the Codex lock check fails
        let mut e = engine(&p, probe, None, FirstRun::ImportToday);
        e.observe(NOW);
        assert_eq!(e.agents(), Agents { active: Some(1), coverage: Coverage::Partial });
        assert_eq!(e.tokens("2026-10-02").coverage, Coverage::Complete, "token reading still works");
        let p2 = Paths { claude_home: p.claude_home.join("missing"), codex_home: p.codex_home.join("missing"), ..p };
        let mut none = engine(&p2, FakeProbe::default(), None, FirstRun::ImportToday);
        none.observe(NOW);
        assert_eq!(none.agents(), Agents { active: None, coverage: Coverage::Unavailable });
        let t = none.tokens("2026-10-02");
        assert_eq!((t.total, t.coverage), (None, Coverage::Unavailable));
    }

    #[test]
    fn a_request_before_seattle_midnight_belongs_to_yesterday() {
        let (p, probe) = setup("engine-day");
        // Count from the start of yesterday, so the 23:00 request on 1 October is inside the window.
        let state = State { version: 1, count_from_ms: NOW - 2 * 86_400_000, ..State::default() };
        let mut e = engine(&p, probe, Some(state), FirstRun::ImportToday);
        e.observe(NOW);
        assert_eq!(e.tokens("2026-10-01").total, Some(1000));
        assert_eq!(e.tokens("2026-10-02").total, Some(312));
        e.mark_sent("2026-10-01");
        assert_eq!(e.correction_due(NOW), None);
        e.sent_days.insert("2026-10-01".into(), Tokens::default());
        assert_eq!(e.correction_due(NOW).as_deref(), Some("2026-10-01"));
    }
}
