//! Calendar-day helpers. "Today" is a date in a named IANA time zone. The rules come from the
//! macOS time-zone database, so daylight-saving changes need no special code.

use jiff::{Timestamp, tz::TimeZone};

pub const DEFAULT_TIME_ZONE: &str = "America/Los_Angeles";

pub struct Zone {
    pub name: String,
    tz: TimeZone,
}

impl Zone {
    pub fn new(name: &str) -> Result<Zone, String> {
        let tz = TimeZone::get(name).map_err(|_| format!("unknown time zone {name}"))?;
        Ok(Zone { name: name.to_string(), tz })
    }

    /// The calendar date (YYYY-MM-DD) of an instant.
    pub fn date(&self, ms: i64) -> String {
        timestamp(ms).to_zoned(self.tz.clone()).date().to_string()
    }

    /// The first instant of the calendar date that contains `ms`.
    pub fn start_of_day(&self, ms: i64) -> i64 {
        let zoned = timestamp(ms).to_zoned(self.tz.clone());
        zoned.start_of_day().map(|z| z.timestamp().as_millisecond()).unwrap_or(ms)
    }

    /// The first instant of the day before the one that contains `ms`.
    pub fn start_of_yesterday(&self, ms: i64) -> i64 {
        // One millisecond before today began is always inside yesterday, even on a 23- or 25-hour day.
        self.start_of_day(self.start_of_day(ms) - 1)
    }
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Parses an RFC 3339 timestamp, such as "2026-10-02T19:00:00.123Z", to epoch milliseconds.
pub fn parse_ms(s: &str) -> Option<i64> {
    if s.len() > 40 {
        return None;
    }
    s.parse::<Timestamp>().ok().map(|t| t.as_millisecond())
}

/// Parses a process start time in the format of `TZ=UTC ps -o lstart=`, such as
/// "Thu Oct  1 08:11:20 2026", to epoch seconds. Claude Code writes this format to its registry.
pub fn parse_lstart_utc(s: &str) -> Option<i64> {
    let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let broken = jiff::fmt::strtime::parse("%a %b %d %H:%M:%S %Y", &normalized).ok()?;
    let zoned = broken.to_datetime().ok()?.to_zoned(TimeZone::UTC).ok()?;
    Some(zoned.timestamp().as_second())
}

/// UTC with millisecond precision and a Z suffix, as the contract requires.
pub fn iso_ms(ms: i64) -> String {
    timestamp(ms).strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn la() -> Zone {
        Zone::new(DEFAULT_TIME_ZONE).unwrap()
    }

    #[test]
    fn date_rolls_over_at_seattle_midnight_not_utc() {
        let z = la();
        // 06:59 UTC on 2 October is 23:59 PDT on 1 October.
        assert_eq!(z.date(parse_ms("2026-10-02T06:59:59.999Z").unwrap()), "2026-10-01");
        assert_eq!(z.date(parse_ms("2026-10-02T07:00:00.000Z").unwrap()), "2026-10-02");
    }

    #[test]
    fn daylight_saving_days_have_23_and_25_hours() {
        let z = la();
        // Spring forward: 2026-03-08 is 23 hours long.
        let start = z.start_of_day(parse_ms("2026-03-08T12:00:00Z").unwrap());
        let next = z.start_of_day(parse_ms("2026-03-09T12:00:00Z").unwrap());
        assert_eq!(iso_ms(start), "2026-03-08T08:00:00.000Z");
        assert_eq!((next - start) / 3_600_000, 23);
        // Fall back: 2026-11-01 is 25 hours long.
        let start = z.start_of_day(parse_ms("2026-11-01T12:00:00Z").unwrap());
        let next = z.start_of_day(parse_ms("2026-11-02T12:00:00Z").unwrap());
        assert_eq!(iso_ms(start), "2026-11-01T07:00:00.000Z");
        assert_eq!((next - start) / 3_600_000, 25);
        // The repeated hour 01:00–02:00 PST still belongs to 1 November.
        assert_eq!(z.date(parse_ms("2026-11-01T09:30:00Z").unwrap()), "2026-11-01");
    }

    #[test]
    fn yesterday_starts_one_calendar_day_earlier() {
        let z = la();
        let now = parse_ms("2026-03-09T10:00:00Z").unwrap(); // 03:00 PDT on 9 March
        assert_eq!(iso_ms(z.start_of_yesterday(now)), "2026-03-08T08:00:00.000Z");
    }

    #[test]
    fn parses_the_claude_registry_start_time() {
        assert_eq!(parse_lstart_utc("Thu Oct  1 08:11:20 2026"), parse_ms("2026-10-01T08:11:20Z").map(|m| m / 1000));
        assert_eq!(parse_lstart_utc("Fri Oct 2 10:16:17 2026"), parse_ms("2026-10-02T10:16:17Z").map(|m| m / 1000));
        assert_eq!(parse_lstart_utc("garbage"), None);
    }

    #[test]
    fn iso_format_matches_the_contract() {
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(parse_ms("2026-10-02T19:00:00Z"), Some(1_790_967_600_000));
        assert_eq!(parse_ms("not a time"), None);
    }
}
