//! Time model. Timestamps are canonical UTC; formatting is fixed (never
//! locale-dependent). Intervals are whole-second durations parsed from
//! strings like `15m`, `1h`, `4h`, `1d`, `1w` (spec: no magic time units).

use crate::error::{CoreError, CoreResult};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};

pub type Ts = DateTime<Utc>;

const TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
const NAIVE_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d",
];

/// Format a timestamp deterministically: whole seconds as `...Z`, otherwise
/// RFC3339 with automatic sub-second precision (still UTC, still fixed).
pub fn format_ts(ts: Ts) -> String {
    if ts.timestamp_subsec_nanos() == 0 {
        ts.format(TS_FORMAT).to_string()
    } else {
        ts.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

/// Parse an interval string (`15m`, `1h`, `1d`, `1w`, ...) into seconds.
/// `s`=seconds, `m`=minutes, `h`=hours, `d`=days, `w`=weeks (Monday-aligned).
pub fn parse_interval(s: &str) -> CoreResult<i64> {
    let t = s.trim();
    let bytes = t.as_bytes();
    if bytes.len() < 2 {
        return Err(CoreError::UnsupportedConfiguration(format!(
            "invalid timeframe '{s}': expected e.g. 15m, 1h, 1d, 1w"
        )));
    }
    let (num, unit) = t.split_at(bytes.len() - 1);
    let n: i64 = num
        .parse()
        .map_err(|_| CoreError::UnsupportedConfiguration(format!("invalid timeframe '{s}'")))?;
    if n <= 0 {
        return Err(CoreError::UnsupportedConfiguration(format!(
            "timeframe '{s}' must be positive"
        )));
    }
    let secs = match unit {
        "s" | "S" => n,
        "m" | "M" => n * 60,
        "h" | "H" => n * 3600,
        "d" | "D" => n * 86_400,
        "w" | "W" => n * 604_800,
        _ => {
            return Err(CoreError::UnsupportedConfiguration(format!(
                "unknown timeframe unit in '{s}' (use s/m/h/d/w)"
            )))
        }
    };
    Ok(secs)
}

/// Parse a CSV timestamp. Accepted: RFC3339 with offset, naive local datetime
/// interpreted in `tz`, or Unix seconds (all-digit string, <= 12 chars).
pub fn parse_timestamp(raw: &str, tz: chrono_tz::Tz, context: &str) -> CoreResult<Ts> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(CoreError::InvalidTimestamp {
            context: context.to_string(),
            detail: "empty timestamp".into(),
        });
    }
    // Unix seconds (allow a fractional part? no: seconds only, documented).
    if s.bytes().all(|b| b.is_ascii_digit()) && s.len() <= 12 {
        let secs: i64 = s.parse().map_err(|_| CoreError::InvalidTimestamp {
            context: context.into(),
            detail: "unix seconds out of range".into(),
        })?;
        return DateTime::<Utc>::from_timestamp(secs, 0).ok_or_else(|| {
            CoreError::InvalidTimestamp {
                context: context.into(),
                detail: "unix seconds out of range".into(),
            }
        });
    }
    // RFC3339 with explicit offset.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Naive datetime in configured timezone.
    for fmt in NAIVE_FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return match tz.from_local_datetime(&naive) {
                chrono::LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
                chrono::LocalResult::Ambiguous(_, _) => Err(CoreError::InvalidTimestamp {
                    context: context.into(),
                    detail: format!("ambiguous local time '{s}' (DST fold)"),
                }),
                chrono::LocalResult::None => Err(CoreError::InvalidTimestamp {
                    context: context.into(),
                    detail: format!("local time '{s}' does not exist (DST gap)"),
                }),
            };
        }
    }
    Err(CoreError::InvalidTimestamp {
        context: context.into(),
        detail: format!("unparseable timestamp '{s}'"),
    })
}

/// Floor a timestamp to an interval boundary. Weeks align to Monday 00:00 UTC
/// (anchor: 1970-01-05, the first Monday of the Unix epoch, = day 4).
pub fn floor_to_interval(ts: Ts, interval_secs: i64) -> Ts {
    if interval_secs % 604_800 == 0 {
        let weeks = interval_secs / 604_800;
        // Days since epoch; 1970-01-01 is a Thursday (Mon=0 index 3).
        let days = ts.timestamp().div_euclid(86_400);
        let weekday_idx = (days + 3).rem_euclid(7); // Monday=0
        let monday = days - weekday_idx;
        // Boundaries are every `weeks`-th Monday anchored at the first epoch Monday.
        let k = (monday - 4).div_euclid(7 * weeks);
        let aligned_days = 4 + k * (7 * weeks);
        DateTime::<Utc>::from_timestamp(aligned_days * 86_400, 0).unwrap_or(ts)
    } else {
        let secs = ts.timestamp();
        let aligned = secs.div_euclid(interval_secs) * interval_secs;
        DateTime::<Utc>::from_timestamp(aligned, 0).unwrap_or(ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_parse() {
        assert_eq!(parse_interval("15m").unwrap(), 900);
        assert_eq!(parse_interval("1H").unwrap(), 3600);
        assert_eq!(parse_interval("4h").unwrap(), 14_400);
        assert_eq!(parse_interval("1d").unwrap(), 86_400);
        assert_eq!(parse_interval("1w").unwrap(), 604_800);
        assert!(parse_interval("0h").is_err());
        assert!(parse_interval("1x").is_err());
        assert!(parse_interval("h").is_err());
    }

    #[test]
    fn timestamps_parse_rfc3339_and_naive() {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let a = parse_timestamp("2024-01-02T03:04:05Z", utc, "t").unwrap();
        let b = parse_timestamp("2024-01-02 03:04:05", utc, "t").unwrap();
        assert_eq!(a, b);
        let tokyo: chrono_tz::Tz = "Asia/Tokyo".parse().unwrap();
        let c = parse_timestamp("2024-01-02 12:00:00", tokyo, "t").unwrap();
        assert_eq!(
            c,
            parse_timestamp("2024-01-02T03:00:00Z", utc, "t").unwrap()
        );
        assert!(parse_timestamp("not a time", utc, "t").is_err());
    }

    #[test]
    fn unix_seconds_parse() {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let t = parse_timestamp("1704153600", utc, "t").unwrap();
        assert_eq!(format_ts(t), "2024-01-02T00:00:00Z");
    }

    #[test]
    fn formatting_is_fixed() {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let t = parse_timestamp("2024-01-02T03:04:05Z", utc, "t").unwrap();
        assert_eq!(format_ts(t), "2024-01-02T03:04:05Z");
    }

    #[test]
    fn floor_alignment() {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let t = parse_timestamp("2024-01-10T07:23:00Z", utc, "t").unwrap();
        assert_eq!(
            format_ts(floor_to_interval(t, 3600)),
            "2024-01-10T07:00:00Z"
        );
        assert_eq!(
            format_ts(floor_to_interval(t, 86_400)),
            "2024-01-10T00:00:00Z"
        );
        // 2024-01-08 is a Monday: weekly floor of 2024-01-10 07:23 is Mon Jan 8.
        assert_eq!(
            format_ts(floor_to_interval(t, 604_800)),
            "2024-01-08T00:00:00Z"
        );
    }
}
