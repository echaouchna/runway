//! Parsing and formatting human-readable durations (`5s`, `1h30m`, `500ms`).

use std::time::Duration;

const SECS_PER_MIN: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MIN;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;
const SECS_PER_WEEK: u64 = 7 * SECS_PER_DAY;
// Match humantime's month (30.44 days) and year (365.25 days) definitions.
const SECS_PER_MONTH: u64 = 2_630_016;
const SECS_PER_YEAR: u64 = 31_557_600;

/// Parses a human-readable duration such as `5s`, `10m`, `1h30m`, `2h 30m` or `500ms`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let mut rest = s.trim();
    if rest.is_empty() {
        return Err("empty duration".into());
    }
    let mut total = Duration::ZERO;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if num_end == 0 {
            return Err(format!("expected number at `{rest}`"));
        }
        let value: u64 = rest[..num_end]
            .parse()
            .map_err(|_| format!("number out of range at `{}`", &rest[..num_end]))?;
        rest = rest[num_end..].trim_start();
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(rest.len());
        if unit_end == 0 {
            return Err(format!("missing unit after `{value}`"));
        }
        let unit = &rest[..unit_end];
        rest = rest[unit_end..].trim_start();
        let term = match unit {
            "ns" | "nsec" | "nsecs" | "nanosecond" | "nanoseconds" => Duration::from_nanos(value),
            "us" | "µs" | "usec" | "usecs" | "microsecond" | "microseconds" => {
                Duration::from_micros(value)
            }
            "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => {
                Duration::from_millis(value)
            }
            "s" | "sec" | "secs" | "second" | "seconds" => Duration::from_secs(value),
            "m" | "min" | "mins" | "minute" | "minutes" => value
                .checked_mul(SECS_PER_MIN)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("duration overflow at `{value}{unit}`"))?,
            "h" | "hr" | "hrs" | "hour" | "hours" => value
                .checked_mul(SECS_PER_HOUR)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("duration overflow at `{value}{unit}`"))?,
            "d" | "day" | "days" => value
                .checked_mul(SECS_PER_DAY)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("duration overflow at `{value}{unit}`"))?,
            "w" | "week" | "weeks" => value
                .checked_mul(SECS_PER_WEEK)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("duration overflow at `{value}{unit}`"))?,
            "M" | "month" | "months" => value
                .checked_mul(SECS_PER_MONTH)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("duration overflow at `{value}{unit}`"))?,
            "y" | "yr" | "year" | "years" => value
                .checked_mul(SECS_PER_YEAR)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("duration overflow at `{value}{unit}`"))?,
            _ => return Err(format!("unknown time unit `{unit}`")),
        };
        total = total
            .checked_add(term)
            .ok_or_else(|| "duration overflow".to_string())?;
    }
    Ok(total)
}

/// Formats a duration concisely (`0s`, `5s`, `1m 30s`, `500ms`).
pub fn format_duration(d: Duration) -> String {
    if d.is_zero() {
        return "0s".into();
    }
    let mut parts = Vec::new();
    let mut secs = d.as_secs();
    for (unit_secs, suffix) in [
        (SECS_PER_DAY, "d"),
        (SECS_PER_HOUR, "h"),
        (SECS_PER_MIN, "m"),
        (1, "s"),
    ] {
        let n = secs / unit_secs;
        if n > 0 {
            parts.push(format!("{n}{suffix}"));
            secs %= unit_secs;
        }
    }
    let mut nanos = d.subsec_nanos();
    for (unit_nanos, suffix) in [(1_000_000, "ms"), (1_000, "us"), (1, "ns")] {
        let n = nanos / unit_nanos;
        if n > 0 {
            parts.push(format!("{n}{suffix}"));
            nanos %= unit_nanos;
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_and_compound_durations() {
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(
            parse_duration(" 1d 2h 3m 4s ").unwrap(),
            Duration::from_secs(86400 + 7200 + 180 + 4)
        );
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(14 * 86400)
        );
        assert_eq!(
            parse_duration("1M").unwrap(),
            Duration::from_secs(SECS_PER_MONTH)
        );
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(
            parse_duration("1y").unwrap(),
            Duration::from_secs(SECS_PER_YEAR)
        );
        assert_eq!(parse_duration("250µs").unwrap(), Duration::from_micros(250));
    }

    #[test]
    fn rejects_invalid_durations() {
        for bad in ["", "   ", "10", "ten", "-5s", "10x", "999999999999999999w"] {
            assert!(parse_duration(bad).is_err(), "expected error for `{bad}`");
        }
    }

    #[test]
    fn formats_and_round_trips() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1s 500ms");
        for d in [
            Duration::ZERO,
            Duration::from_millis(250),
            Duration::from_secs(45),
            Duration::from_secs(3661),
            Duration::from_secs(90061),
        ] {
            assert_eq!(parse_duration(&format_duration(d)).unwrap(), d);
        }
    }
}
