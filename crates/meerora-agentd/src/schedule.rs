//! When scheduled agents wake (D23). Intervals (`every`), fixed local times
//! (`at`) and quiet hours, all in the local time zone. Times are unix
//! milliseconds so they survive a restart in `state.json`.

use std::time::Duration;

use jiff::Zoned;
use jiff::civil::Time;
use meerora_protocol::{ScheduleEntry, parse_duration};

/// Example shown in error messages about `at`.
const AT_EXAMPLE: &str = "\"07:00, 18:30\"";

/// A local time range without runs, for example 22:00 to 07:00.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietHours {
    start: Time,
    end: Time,
}

impl QuietHours {
    pub fn parse(text: &str) -> Result<Self, String> {
        let range = text.split_once('-').and_then(|(a, b)| Some((parse_hm(a)?, parse_hm(b)?)));
        let (start, end) = range.ok_or_else(|| format!("quiet_hours = {text:?} is not a range like \"22:00-07:00\""))?;
        Ok(Self { start, end })
    }

    /// Whether `time` falls into the range. Ranges may pass midnight.
    pub fn contains(&self, time: Time) -> bool {
        if self.start <= self.end { self.start <= time && time < self.end } else { time >= self.start || time < self.end }
    }
}

/// "07:00" or "7:00".
fn parse_hm(text: &str) -> Option<Time> {
    let (h, m) = text.trim().split_once(':')?;
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if !digits(h) || h.len() > 2 || !digits(m) || m.len() != 2 {
        return None;
    }
    Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()
}

/// Comma-separated local times such as "07:00, 18:30", sorted.
pub fn parse_times(text: &str) -> Result<Vec<Time>, String> {
    let mut times = Vec::new();
    for part in text.split(',') {
        let time = parse_hm(part)
            .ok_or_else(|| format!("at = {text:?} is not a list of local times like {AT_EXAMPLE}, and cron syntax is not supported yet"))?;
        times.push(time);
    }
    times.sort();
    times.dedup();
    Ok(times)
}

/// How a schedule entry wakes its agent.
#[derive(Clone, Debug, PartialEq)]
pub enum When {
    Every(Duration),
    At(Vec<Time>),
}

impl When {
    pub fn parse(entry: &ScheduleEntry) -> Result<When, String> {
        match (&entry.every, &entry.at) {
            (Some(_), Some(_)) => Err("the entry uses both every and at".into()),
            (Some(every), None) => {
                parse_duration(every).map(When::Every).ok_or_else(|| format!("every = {every:?} is not a duration like \"5m\""))
            }
            (None, Some(at)) => parse_times(at).map(When::At),
            (None, None) => Err("the entry needs every or at".into()),
        }
    }

    /// Next wake after a wake at `now`.
    pub fn next_after(&self, now: &Zoned) -> i64 {
        match self {
            When::Every(every) => now.timestamp().as_millisecond() + millis(*every),
            When::At(times) => next_at(times, now),
        }
    }

    /// First wake when the daemon starts, given the last recorded wake. A
    /// wake that was missed while the daemon was off happens once, right away.
    pub fn first(&self, last: Option<i64>, now: &Zoned) -> i64 {
        let now_ms = now.timestamp().as_millisecond();
        match self {
            When::Every(every) => last.map_or(now_ms + millis(*every), |last| last + millis(*every)),
            When::At(times) => match (last, last_at(times, now)) {
                (Some(last), Some(previous)) if last < previous => now_ms,
                _ => next_at(times, now),
            },
        }
    }
}

/// Validates the `every`, `at` and `quiet_hours` of one schedule entry.
pub fn check_entry(entry: &ScheduleEntry) -> Result<(), String> {
    When::parse(entry)?;
    if let Some(quiet) = &entry.quiet_hours {
        QuietHours::parse(quiet)?;
    }
    Ok(())
}

/// The key under which `state.json` remembers the last wake of an entry.
pub fn entry_key(entry: &ScheduleEntry) -> String {
    match (&entry.every, &entry.at) {
        (Some(every), _) => format!("every {every}"),
        (None, Some(at)) => format!("at {at}"),
        (None, None) => "never".into(),
    }
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX / 2)
}

/// The first of `times` strictly after `now`, today or on one of the next days.
fn next_at(times: &[Time], now: &Zoned) -> i64 {
    let now_ms = now.timestamp().as_millisecond();
    let mut day = now.date();
    for _ in 0..3 {
        for time in times {
            if let Ok(zoned) = day.to_datetime(*time).to_zoned(now.time_zone().clone())
                && zoned.timestamp().as_millisecond() > now_ms
            {
                return zoned.timestamp().as_millisecond();
            }
        }
        match day.tomorrow() {
            Ok(next) => day = next,
            Err(_) => break,
        }
    }
    now_ms + 86_400_000
}

/// The last of `times` at or before `now`, today or yesterday.
fn last_at(times: &[Time], now: &Zoned) -> Option<i64> {
    let now_ms = now.timestamp().as_millisecond();
    let mut day = now.date();
    for _ in 0..2 {
        for time in times.iter().rev() {
            if let Ok(zoned) = day.to_datetime(*time).to_zoned(now.time_zone().clone())
                && zoned.timestamp().as_millisecond() <= now_ms
            {
                return Some(zoned.timestamp().as_millisecond());
            }
        }
        day = day.yesterday().ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::{date, time};
    use jiff::tz::TimeZone;

    fn at(h: i8, m: i8) -> Zoned {
        date(2026, 10, 14).at(h, m, 0, 0).to_zoned(TimeZone::UTC).unwrap()
    }

    fn ms(z: &Zoned) -> i64 {
        z.timestamp().as_millisecond()
    }

    fn entry(every: Option<&str>, at: Option<&str>) -> ScheduleEntry {
        ScheduleEntry { every: every.map(Into::into), at: at.map(Into::into), task: "t".into(), station: None, quiet_hours: None }
    }

    #[test]
    fn quiet_hours_pass_midnight() {
        let night = QuietHours::parse("22:00-07:00").unwrap();
        assert!(night.contains(time(23, 30, 0, 0)));
        assert!(night.contains(time(3, 0, 0, 0)));
        assert!(!night.contains(time(7, 0, 0, 0)));
        assert!(!night.contains(time(12, 0, 0, 0)));
        let day = QuietHours::parse("9:00-17:00").unwrap();
        assert!(day.contains(time(12, 0, 0, 0)));
        assert!(!day.contains(time(18, 0, 0, 0)));
        for bad in ["22-07", "25:00-07:00", "22:00", "22:0-07:00"] {
            assert!(QuietHours::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn at_times_and_cron() {
        assert_eq!(parse_times("18:30, 07:00").unwrap(), [time(7, 0, 0, 0), time(18, 30, 0, 0)]);
        let err = parse_times("*/5 * * * *").unwrap_err();
        assert!(err.contains("cron syntax is not supported"), "{err}");
        assert!(parse_times("7:5").is_err());
        assert!(When::parse(&entry(Some("5m"), Some("07:00"))).is_err());
        assert!(When::parse(&entry(None, None)).is_err());
        assert_eq!(When::parse(&entry(Some("5m"), None)).unwrap(), When::Every(Duration::from_secs(300)));
    }

    #[test]
    fn next_at_time_is_strictly_later() {
        let when = When::parse(&entry(None, Some("07:00, 18:30"))).unwrap();
        assert_eq!(when.next_after(&at(6, 0)), ms(&at(7, 0)));
        assert_eq!(when.next_after(&at(7, 0)), ms(&at(18, 30)));
        let tomorrow = date(2026, 10, 15).at(7, 0, 0, 0).to_zoned(TimeZone::UTC).unwrap();
        assert_eq!(when.next_after(&at(19, 0)), ms(&tomorrow));
    }

    #[test]
    fn first_wake_after_a_restart() {
        let every = When::Every(Duration::from_secs(300));
        let now = at(9, 0);
        assert_eq!(every.first(None, &now), ms(&now) + 300_000);
        assert_eq!(every.first(Some(ms(&at(8, 59))), &now), ms(&at(9, 4)), "a recent run is not repeated");
        assert!(every.first(Some(ms(&at(7, 0))), &now) <= ms(&now), "a missed run happens once, now");

        let times = When::parse(&entry(None, Some("07:00, 18:30"))).unwrap();
        let yesterday = date(2026, 10, 13).at(18, 30, 5, 0).to_zoned(TimeZone::UTC).unwrap();
        assert_eq!(times.first(Some(ms(&yesterday)), &now), ms(&now), "07:00 was missed");
        assert_eq!(times.first(Some(ms(&at(7, 0)) + 5_000), &now), ms(&at(18, 30)));
        assert_eq!(times.first(None, &now), ms(&at(18, 30)), "no catch-up without a record");
    }
}
