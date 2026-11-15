//! Time parsing and bucket boundaries (local time zone, DST-safe).

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;

pub fn parse_tz(name: &str) -> Result<Tz> {
    name.parse::<Tz>().map_err(|_| anyhow!("unknown time zone {name:?}"))
}

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn local_midnight_ms(date: NaiveDate, tz: Tz) -> i64 {
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    local_to_ms(naive, tz)
}

fn local_to_ms(naive: NaiveDateTime, tz: Tz) -> i64 {
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => t.timestamp_millis(),
        chrono::LocalResult::Ambiguous(a, _) => a.timestamp_millis(),
        // non-existent local time (DST jump): shift forward one hour
        chrono::LocalResult::None => tz
            .from_local_datetime(&(naive + Duration::hours(1)))
            .earliest()
            .map(|t| t.timestamp_millis())
            .unwrap_or_else(|| naive.and_utc().timestamp_millis()),
    }
}

/// Earliest and latest accepted time: 1970-01-01 and 2100-01-01 (UTC). Anything outside is
/// certainly a mistake, and the bound keeps all later date arithmetic far from overflow.
pub const MIN_TIME_MS: i64 = 0;
pub const MAX_TIME_MS: i64 = 4_102_444_800_000;

fn check_range(ms: i64, s: &str) -> Result<i64> {
    if !(MIN_TIME_MS..=MAX_TIME_MS).contains(&ms) {
        bail!("time {s:?} is outside the supported range 1970-01-01 to 2100-01-01");
    }
    Ok(ms)
}

fn to_utc(ms: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single().ok_or_else(|| anyhow!("time {ms} ms is out of range"))
}

/// Accepts: `now`, `today`, `yesterday`, relative (`-6h`, `-7d`, `-30m`, `-2w`),
/// RFC 3339, `YYYY-MM-DD` (local midnight) and `YYYY-MM-DD HH:MM[:SS]` (local time).
/// Times outside [`MIN_TIME_MS`, `MAX_TIME_MS`] are rejected.
pub fn parse_time(s: &str, now: i64, tz: Tz) -> Result<i64> {
    check_range(parse_time_unchecked(s.trim(), now, tz)?, s)
}

fn parse_time_unchecked(s: &str, now: i64, tz: Tz) -> Result<i64> {
    match s.to_ascii_lowercase().as_str() {
        "now" => return Ok(now),
        "today" | "yesterday" => {
            let local = to_utc(now)?.with_timezone(&tz);
            let mut d = local.date_naive();
            if s.eq_ignore_ascii_case("yesterday") {
                d = d.pred_opt().unwrap();
            }
            return Ok(local_midnight_ms(d, tz));
        }
        _ => {}
    }
    if let Some(rest) = s.strip_prefix('-') {
        if let Some(unit) = rest.chars().last() {
            let num = &rest[..rest.len() - unit.len_utf8()];
            if let Ok(n) = num.parse::<i64>() {
                let ms = match unit {
                    's' => 1_000,
                    'm' => 60_000,
                    'h' => 3_600_000,
                    'd' => 86_400_000,
                    'w' => 7 * 86_400_000,
                    _ => bail!("unknown unit in {s:?} (use s, m, h, d or w)"),
                };
                return n
                    .checked_mul(ms)
                    .and_then(|d| now.checked_sub(d))
                    .ok_or_else(|| anyhow!("time {s:?} is out of range"));
            }
        }
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(local_midnight_ms(d, tz));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(local_to_ms(n, tz));
        }
    }
    Err(anyhow!(
        "cannot read time {s:?}; use e.g. \"-24h\", \"today\", \"2026-11-01\" or RFC 3339"
    ))
}

pub fn fmt_ts(ms: i64, tz: Tz) -> String {
    match Utc.timestamp_millis_opt(ms).single() {
        Some(t) => t.with_timezone(&tz).format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        None => ms.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Raw,
    Fixed(i64),
    Day,
    Week,
    Month,
}

impl Bucket {
    pub fn label(&self) -> String {
        match self {
            Bucket::Raw => "raw".into(),
            Bucket::Fixed(ms) if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
            Bucket::Fixed(ms) => format!("{}m", ms / 60_000),
            Bucket::Day => "1d".into(),
            Bucket::Week => "1w".into(),
            Bucket::Month => "1mo".into(),
        }
    }
}

pub fn parse_bucket(s: &str) -> Result<Bucket> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "raw" => Bucket::Raw,
        "1m" => Bucket::Fixed(60_000),
        "5m" => Bucket::Fixed(5 * 60_000),
        "15m" => Bucket::Fixed(15 * 60_000),
        "30m" => Bucket::Fixed(30 * 60_000),
        "1h" => Bucket::Fixed(3_600_000),
        "3h" => Bucket::Fixed(3 * 3_600_000),
        "6h" => Bucket::Fixed(6 * 3_600_000),
        "12h" => Bucket::Fixed(12 * 3_600_000),
        "1d" | "day" => Bucket::Day,
        "1w" | "week" => Bucket::Week,
        "1mo" | "month" => Bucket::Month,
        other => bail!("unknown bucket {other:?}; choose raw, 1m, 5m, 15m, 30m, 1h, 3h, 6h, 12h, 1d, 1w or 1mo"),
    })
}

pub const MAX_BUCKETS: usize = 2000;

/// Bucket boundaries `[start, end)` covering `[from, to)`. Day/week/month follow the local calendar.
pub fn bucket_edges(from: i64, to: i64, bucket: Bucket, tz: Tz) -> Result<Vec<(i64, i64)>> {
    if to <= from {
        bail!("`to` must be after `from`");
    }
    check_range(from, "from")?;
    check_range(to, "to")?;
    let mut out = Vec::new();
    match bucket {
        Bucket::Raw => bail!("raw has no buckets"),
        Bucket::Fixed(size) => {
            let off = tz
                .offset_from_utc_datetime(&to_utc(from)?.naive_utc())
                .fix()
                .local_minus_utc() as i64
                * 1000;
            let mut start = (from + off).div_euclid(size) * size - off;
            while start < to {
                out.push((start, start + size));
                start += size;
                if out.len() > MAX_BUCKETS {
                    bail!("too many buckets (>{MAX_BUCKETS}); choose a larger bucket or a shorter range");
                }
            }
        }
        Bucket::Day | Bucket::Week | Bucket::Month => {
            let first = to_utc(from)?.with_timezone(&tz).date_naive();
            let mut d = match bucket {
                Bucket::Day => first,
                Bucket::Week => first - Duration::days(first.weekday().num_days_from_monday() as i64),
                _ => first.with_day(1).unwrap(),
            };
            loop {
                let next = match bucket {
                    Bucket::Day => d + Duration::days(1),
                    Bucket::Week => d + Duration::days(7),
                    _ => {
                        let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
                        NaiveDate::from_ymd_opt(y, m, 1).unwrap()
                    }
                };
                let (s, e) = (local_midnight_ms(d, tz), local_midnight_ms(next, tz));
                if s >= to {
                    break;
                }
                out.push((s, e));
                d = next;
                if out.len() > MAX_BUCKETS {
                    bail!("too many buckets (>{MAX_BUCKETS}); choose a larger bucket or a shorter range");
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_follow_local_calendar_across_dst() {
        let tz = parse_tz("Europe/Amsterdam").unwrap();
        // 25 Oct 2026 is the switch to winter time: that day lasts 25 hours
        let from = parse_time("2026-10-24", 0, tz).unwrap();
        let to = parse_time("2026-10-27", 0, tz).unwrap();
        let e = bucket_edges(from, to, Bucket::Day, tz).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[1].1 - e[1].0, 25 * 3_600_000);
    }

    #[test]
    fn relative_and_date_parse() {
        let tz = parse_tz("Europe/Amsterdam").unwrap();
        assert_eq!(parse_time("-6h", 10 * 3_600_000, tz).unwrap(), 4 * 3_600_000);
        assert!(parse_time("2026-11-15T10:00:00+01:00", 0, tz).is_ok());
        assert!(parse_time("nonsense", 0, tz).is_err());
    }

    #[test]
    fn out_of_range_times_are_errors_not_panics() {
        let tz = parse_tz("Europe/Amsterdam").unwrap();
        let now = parse_time("2026-11-15", 0, tz).unwrap();
        for s in ["-300000000d", "-9223372036854775807w", "-60y", "1900-01-01", "9999-12-31", "2200-01-01T00:00:00Z"] {
            assert!(parse_time(s, now, tz).is_err(), "{s} should be rejected");
        }
        assert!(bucket_edges(-1_000_000_000_000_000, now, Bucket::Day, tz).is_err());
        assert!(bucket_edges(i64::MIN, now, Bucket::Fixed(3_600_000), tz).is_err());
    }
}
