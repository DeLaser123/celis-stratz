//! Timeframe resampling (spec §17). Bars are aggregated to higher timeframes
//! on epoch-aligned boundaries (weeks = Monday 00:00 UTC). The trailing
//! partial HTF bar is included in the series but can never be *observed*
//! before its close time (observability is enforced by the strategy runtime).

use crate::bar::{Bar, BarSeries};
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::floor_to_interval;

pub fn resample(series: &BarSeries, target_secs: i64) -> CoreResult<BarSeries> {
    if target_secs < series.interval_secs {
        return Err(CoreError::UnsupportedConfiguration(format!(
            "timeframe {target_secs}s is lower than base timeframe {}s; higher timeframe required",
            series.interval_secs
        )));
    }
    if target_secs % series.interval_secs != 0 {
        return Err(CoreError::UnsupportedConfiguration(format!(
            "timeframe {target_secs}s must be an exact multiple of base timeframe {}s",
            series.interval_secs
        )));
    }
    if target_secs == series.interval_secs {
        return Ok(series.clone());
    }

    let mut out: Vec<Bar> = Vec::new();
    for bar in &series.bars {
        let bucket = floor_to_interval(bar.open_time, target_secs);
        match out.last_mut() {
            Some(last) if last.open_time == bucket => {
                if bar.high > last.high {
                    last.high = bar.high;
                }
                if bar.low < last.low {
                    last.low = bar.low;
                }
                last.close = bar.close;
                last.volume = match (last.volume.take(), bar.volume) {
                    (Some(a), Some(b)) => Some(a + b),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
            }
            _ => out.push(Bar {
                open_time: bucket,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            }),
        }
    }

    Ok(BarSeries {
        symbol: series.symbol.clone(),
        interval_secs: target_secs,
        bars: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_core::time::{format_ts, parse_timestamp};
    use bt_core::D;
    use rust_decimal_macros::dec;

    fn bar(t: &str, o: D, h: D, l: D, c: D) -> Bar {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        Bar {
            open_time: parse_timestamp(t, utc, "t").unwrap(),
            open: o,
            high: h,
            low: l,
            close: c,
            volume: Some(dec!(10)),
        }
    }

    #[test]
    fn hourly_to_daily() {
        let series = BarSeries {
            symbol: "X".into(),
            interval_secs: 3600,
            bars: vec![
                bar(
                    "2024-01-02T00:00:00Z",
                    dec!(1),
                    dec!(2),
                    dec!(0.5),
                    dec!(1.5),
                ),
                bar(
                    "2024-01-02T01:00:00Z",
                    dec!(1.5),
                    dec!(3),
                    dec!(1),
                    dec!(2.5),
                ),
                bar(
                    "2024-01-02T02:00:00Z",
                    dec!(2.5),
                    dec!(2.8),
                    dec!(2),
                    dec!(2.2),
                ),
                // next day
                bar(
                    "2024-01-03T00:00:00Z",
                    dec!(2.2),
                    dec!(2.4),
                    dec!(2.1),
                    dec!(2.3),
                ),
            ],
        };
        let daily = resample(&series, 86_400).unwrap();
        assert_eq!(daily.bars.len(), 2);
        let d0 = &daily.bars[0];
        assert_eq!(format_ts(d0.open_time), "2024-01-02T00:00:00Z");
        assert_eq!(d0.open, dec!(1));
        assert_eq!(d0.high, dec!(3));
        assert_eq!(d0.low, dec!(0.5));
        assert_eq!(d0.close, dec!(2.2));
        assert_eq!(d0.volume, Some(dec!(30)));
        assert_eq!(daily.bars[1].volume, Some(dec!(10)));
    }

    #[test]
    fn weekly_alignment_is_monday() {
        let series = BarSeries {
            symbol: "X".into(),
            interval_secs: 86_400,
            bars: vec![bar(
                "2024-01-10T07:00:00Z",
                dec!(1),
                dec!(1),
                dec!(1),
                dec!(1),
            )],
        };
        let weekly = resample(&series, 604_800).unwrap();
        assert_eq!(format_ts(weekly.bars[0].open_time), "2024-01-08T00:00:00Z");
    }

    #[test]
    fn rejects_lower_timeframe() {
        let series = BarSeries {
            symbol: "X".into(),
            interval_secs: 3600,
            bars: vec![],
        };
        assert!(resample(&series, 900).is_err());
        assert!(resample(&series, 3700).is_err());
    }
}
