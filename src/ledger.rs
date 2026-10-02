//! Deduplicates usage and adds it up by day and provider.
//!
//! Each request has one key. The ledger keeps the per-field maximum for a key, so a repeated record
//! adds nothing, and a Claude response that grew while it streamed adds only the difference.
//! Keys stay "recent", with their numbers, for 10 minutes after they were last seen. Then only the
//! key is kept ("settled"), which is enough to ignore a later replay. Days before yesterday are
//! dropped, so the state stays small.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{Provider, Tokens, UsageRecord};

const SETTLE_AFTER_MS: i64 = 10 * 60 * 1000;

#[derive(Default, Serialize, Deserialize)]
pub struct Ledger {
    days: BTreeMap<String, BTreeMap<Provider, Tokens>>,
    recent: HashMap<u64, Recent>,
    #[serde(with = "hex_keys")]
    settled: BTreeMap<String, HashSet<u64>>,
}

#[derive(Serialize, Deserialize)]
struct Recent {
    day: String,
    provider: Provider,
    tokens: Tokens,
    seen_ms: i64,
}

impl Ledger {
    /// Counts a record for `day`. Returns true when a total changed.
    pub fn observe(&mut self, provider: Provider, record: &UsageRecord, day: &str, now_ms: i64) -> bool {
        if self.settled.get(day).is_some_and(|keys| keys.contains(&record.key)) {
            return false;
        }
        let (day, delta) = match self.recent.get_mut(&record.key) {
            Some(r) => {
                let max = Tokens {
                    input: r.tokens.input.max(record.tokens.input),
                    cached_input: r.tokens.cached_input.max(record.tokens.cached_input),
                    output: r.tokens.output.max(record.tokens.output),
                };
                let delta = Tokens {
                    input: max.input - r.tokens.input,
                    cached_input: max.cached_input - r.tokens.cached_input,
                    output: max.output - r.tokens.output,
                };
                r.tokens = max;
                r.seen_ms = now_ms;
                (r.day.clone(), delta)
            }
            None => {
                let r = Recent { day: day.to_string(), provider, tokens: record.tokens, seen_ms: now_ms };
                self.recent.insert(record.key, r);
                (day.to_string(), record.tokens)
            }
        };
        if delta == Tokens::default() {
            return false;
        }
        self.days.entry(day).or_default().entry(provider).or_default().add(delta);
        true
    }

    pub fn totals(&self, day: &str, provider: Provider) -> Tokens {
        self.days.get(day).and_then(|d| d.get(&provider)).copied().unwrap_or_default()
    }

    /// Moves keys that have not changed for 10 minutes to the settled sets, and drops days before
    /// `oldest_day` (YYYY-MM-DD sorts by date).
    pub fn tidy(&mut self, now_ms: i64, oldest_day: &str) {
        let old: Vec<u64> = self.recent.iter().filter(|(_, r)| now_ms - r.seen_ms > SETTLE_AFTER_MS).map(|(k, _)| *k).collect();
        for key in old {
            if let Some(r) = self.recent.remove(&key) {
                self.settled.entry(r.day).or_default().insert(key);
            }
        }
        self.days.retain(|day, _| day.as_str() >= oldest_day);
        self.settled.retain(|day, _| day.as_str() >= oldest_day);
        self.recent.retain(|_, r| r.day.as_str() >= oldest_day);
    }
}

/// Stores each day's settled keys as one string of 16-digit hex numbers: about 17 bytes a key.
mod hex_keys {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(map: &BTreeMap<String, HashSet<u64>>, s: S) -> Result<S::Ok, S::Error> {
        let packed: BTreeMap<&String, String> = map
            .iter()
            .map(|(day, keys)| {
                let mut sorted: Vec<&u64> = keys.iter().collect();
                sorted.sort();
                (day, sorted.iter().map(|k| format!("{k:016x}")).collect::<String>())
            })
            .collect();
        packed.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, HashSet<u64>>, D::Error> {
        let packed = BTreeMap::<String, String>::deserialize(d)?;
        let mut out = BTreeMap::new();
        for (day, hex) in packed {
            let keys = hex
                .as_bytes()
                .chunks(16)
                .filter_map(|c| std::str::from_utf8(c).ok().and_then(|s| u64::from_str_radix(s, 16).ok()))
                .collect();
            out.insert(day, keys);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: u64, input: u64, output: u64) -> UsageRecord {
        UsageRecord { key, at_ms: 0, tokens: Tokens { input, cached_input: 0, output } }
    }

    #[test]
    fn repeats_add_nothing_and_growth_adds_the_difference() {
        let mut l = Ledger::default();
        assert!(l.observe(Provider::Claude, &rec(1, 100, 2), "2026-10-02", 0));
        assert!(!l.observe(Provider::Claude, &rec(1, 100, 2), "2026-10-02", 0));
        assert!(l.observe(Provider::Claude, &rec(1, 100, 50), "2026-10-02", 0));
        assert!(!l.observe(Provider::Claude, &rec(1, 100, 10), "2026-10-02", 0), "a smaller repeat adds nothing");
        assert_eq!(l.totals("2026-10-02", Provider::Claude), Tokens { input: 100, cached_input: 0, output: 50 });
    }

    #[test]
    fn providers_and_days_stay_separate() {
        let mut l = Ledger::default();
        l.observe(Provider::Claude, &rec(1, 10, 1), "2026-10-01", 0);
        l.observe(Provider::Codex, &rec(2, 20, 2), "2026-10-02", 0);
        assert_eq!(l.totals("2026-10-01", Provider::Claude).total(), 11);
        assert_eq!(l.totals("2026-10-02", Provider::Claude).total(), 0);
        assert_eq!(l.totals("2026-10-02", Provider::Codex).total(), 22);
    }

    #[test]
    fn settled_keys_survive_a_restart_and_block_replays() {
        let mut l = Ledger::default();
        l.observe(Provider::Codex, &rec(7, 10, 1), "2026-10-02", 0);
        l.tidy(SETTLE_AFTER_MS + 1, "2026-10-01");
        let saved = serde_json::to_string(&l).unwrap();
        let mut restored: Ledger = serde_json::from_str(&saved).unwrap();
        assert!(!restored.observe(Provider::Codex, &rec(7, 10, 1), "2026-10-02", 0), "a replayed record");
        assert_eq!(restored.totals("2026-10-02", Provider::Codex).total(), 11);
    }

    #[test]
    fn tidy_drops_days_before_the_oldest_kept_day() {
        let mut l = Ledger::default();
        l.observe(Provider::Claude, &rec(1, 10, 1), "2026-09-30", 0);
        l.observe(Provider::Claude, &rec(2, 10, 1), "2026-10-01", 0);
        l.tidy(SETTLE_AFTER_MS + 1, "2026-10-01");
        assert_eq!(l.totals("2026-09-30", Provider::Claude).total(), 0);
        assert_eq!(l.totals("2026-10-01", Provider::Claude).total(), 11);
        let saved = serde_json::to_string(&l).unwrap();
        assert!(!saved.contains("2026-09-30"));
    }
}
