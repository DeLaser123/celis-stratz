//! CSV ingestion with strict/permissive validation (spec §5, §38).
//!
//! Files are treated as untrusted input: size/row limits, schema checks,
//! numeric sanity, timestamp parsing, ordering and duplicate detection.
//! Nothing is repaired silently — every action is an issue in the report.

use crate::bar::{Bar, BarSeries, Dataset};
use crate::validate::{check_ohlc, IssueCode, ValidationIssue, ValidationReport};
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::{format_ts, parse_timestamp};
use bt_core::D;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    #[default]
    /// Any issue aborts the run.
    Strict,
    /// Issues are reported and fixed in the documented order:
    /// drop invalid rows -> deduplicate timestamps (keep first) -> stable sort.
    Permissive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TimestampConvention {
    #[default]
    Open,
    Close,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LoadLimits {
    pub max_file_mb: u64,
    pub max_rows: usize,
}

impl Default for LoadLimits {
    fn default() -> Self {
        LoadLimits {
            max_file_mb: 512,
            max_rows: 10_000_000,
        }
    }
}

struct RawRow {
    csv_row: usize,
    ts: bt_core::time::Ts,
    symbol: String,
    open: D,
    high: D,
    low: D,
    close: D,
    volume: Option<D>,
}

/// Add an issue and mark the row fatal (row will be dropped).
fn fail(
    report: &mut ValidationReport,
    csv_row: usize,
    ts_raw: &str,
    fatal: &mut bool,
    code: IssueCode,
    detail: String,
) {
    report.issues.push(ValidationIssue {
        row: csv_row,
        ts: Some(ts_raw.to_string()),
        code,
        detail,
        fatal: true,
    });
    *fatal = true;
}

/// Parse a decimal field; `fatal_on_missing` distinguishes prices (fatal) from
/// volume (warning: kept as null).
fn parse_num(
    report: &mut ValidationReport,
    csv_row: usize,
    ts_raw: &str,
    fatal: &mut bool,
    raw: Option<&str>,
    field: &str,
    fatal_on_missing: bool,
) -> Option<D> {
    match raw {
        None | Some("") => {
            if fatal_on_missing {
                fail(
                    report,
                    csv_row,
                    ts_raw,
                    fatal,
                    IssueCode::MissingValue,
                    format!("missing {field}"),
                );
                None
            } else {
                None
            }
        }
        Some(txt) => {
            match Decimal::from_str_exact(txt).or_else(|_| Decimal::from_scientific(txt)) {
                Ok(v) => Some(v),
                _ => {
                    fail(
                        report,
                        csv_row,
                        ts_raw,
                        fatal,
                        IssueCode::InvalidNumber,
                        format!("{txt:?} is not a finite decimal for {field}"),
                    );
                    None
                }
            }
        }
    }
}

/// Load and validate bars from CSV. Returns a fully normalized `Dataset`.
pub fn load_bars_csv(
    path: &Path,
    tz: chrono_tz::Tz,
    convention: TimestampConvention,
    mode: ValidationMode,
    limits: LoadLimits,
    expected_interval_secs: Option<i64>,
) -> CoreResult<Dataset> {
    let meta = std::fs::metadata(path).map_err(|e| {
        CoreError::InvalidData(format!("cannot open data file {}: {e}", path.display()))
    })?;
    let size_mb = meta.len() / (1024 * 1024);
    if size_mb > limits.max_file_mb {
        return Err(CoreError::InvalidData(format!(
            "data file is {size_mb} MB, exceeds limit {} MB",
            limits.max_file_mb
        )));
    }
    let bytes = std::fs::read(path)?;
    let data_hash = bt_core::hash::sha256_hex(&bytes);

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(&bytes[..]);

    let headers = reader
        .headers()
        .map_err(|e| CoreError::InvalidData(format!("cannot read CSV header: {e}")))?
        .clone();
    let header_names: Vec<String> = headers.iter().map(|s| s.trim().to_string()).collect();
    let required = ["timestamp", "symbol", "open", "high", "low", "close"];

    let mut col: BTreeMap<String, usize> = BTreeMap::new();
    for (i, name) in header_names.iter().enumerate() {
        col.insert(name.clone(), i);
    }
    let mut schema_missing: Vec<String> = Vec::new();
    for r in required {
        if !col.contains_key(r) {
            schema_missing.push(r.to_string());
        }
    }
    if !schema_missing.is_empty() {
        return Err(CoreError::InvalidData(format!(
            "CSV schema error: missing required columns {schema_missing:?}"
        )));
    }
    let has_volume_col = col.contains_key("volume");
    let mut unknown_cols: Vec<String> = Vec::new();
    for name in &header_names {
        if name != "volume" && !required.contains(&name.as_str()) {
            unknown_cols.push(name.clone());
        }
    }

    let mut report = ValidationReport::default();
    if !unknown_cols.is_empty() {
        report.issues.push(ValidationIssue {
            row: 0,
            ts: None,
            code: IssueCode::SchemaError,
            detail: format!("unknown columns {unknown_cols:?} (ignored)"),
            fatal: false,
        });
    }

    let mut raw_rows: Vec<RawRow> = Vec::new();
    let mut rows_seen: usize = 0;
    for (ri, result) in reader.records().enumerate() {
        let csv_row = ri + 1; // 1-based data row
        rows_seen = csv_row;
        if raw_rows.len() >= limits.max_rows {
            report.issues.push(ValidationIssue {
                row: csv_row,
                ts: None,
                code: IssueCode::RowLimitExceeded,
                detail: format!(
                    "row limit {} exceeded; remaining rows ignored",
                    limits.max_rows
                ),
                fatal: true,
            });
            break;
        }
        let record = result.map_err(|e| {
            CoreError::InvalidData(format!("CSV parse error at row {csv_row}: {e}"))
        })?;
        let get = |i: usize| record.get(i).map(|s| s.trim());

        let ts_raw = get(col["timestamp"]).unwrap_or("");
        let symbol = get(col["symbol"]).unwrap_or("").to_string();
        if symbol.is_empty() {
            report.issues.push(ValidationIssue {
                row: csv_row,
                ts: Some(ts_raw.to_string()),
                code: IssueCode::SchemaError,
                detail: "empty symbol".into(),
                fatal: true,
            });
            continue;
        }

        let ts = match parse_timestamp(ts_raw, tz, &format!("row {csv_row}")) {
            Ok(t) => t,
            Err(CoreError::InvalidTimestamp { detail, .. }) => {
                let code = if detail.contains("ambiguous") {
                    IssueCode::AmbiguousLocalTime
                } else {
                    IssueCode::UnparseableTimestamp
                };
                report.issues.push(ValidationIssue {
                    row: csv_row,
                    ts: Some(ts_raw.to_string()),
                    code,
                    detail,
                    fatal: true,
                });
                continue;
            }
            Err(e) => return Err(e),
        };

        let mut fatal = false;
        let mut next_num = |raw: Option<&str>, field: &str, fatal_on_missing: bool| -> Option<D> {
            parse_num(
                &mut report,
                csv_row,
                ts_raw,
                &mut fatal,
                raw,
                field,
                fatal_on_missing,
            )
        };
        let Some(open) = next_num(get(col["open"]), "open", true) else {
            continue;
        };
        let Some(high) = next_num(get(col["high"]), "high", true) else {
            continue;
        };
        let Some(low) = next_num(get(col["low"]), "low", true) else {
            continue;
        };
        let Some(close) = next_num(get(col["close"]), "close", true) else {
            continue;
        };
        let volume_missing =
            has_volume_col && get(col["volume"]).map(|s| s.is_empty()).unwrap_or(true);
        let volume = if has_volume_col {
            next_num(get(col["volume"]), "volume", false)
        } else {
            None
        };
        let _ = next_num; // end the mutable borrow before direct report access
        if volume_missing {
            // Missing volume is a warning-level issue, not fatal.
            report.issues.push(ValidationIssue {
                row: csv_row,
                ts: Some(ts_raw.to_string()),
                code: IssueCode::MissingValue,
                detail: "missing volume (kept as null)".into(),
                fatal: false,
            });
        }

        if !fatal {
            for (name, v) in [
                ("open", open),
                ("high", high),
                ("low", low),
                ("close", close),
            ] {
                if v <= Decimal::ZERO {
                    fail(
                        &mut report,
                        csv_row,
                        ts_raw,
                        &mut fatal,
                        IssueCode::NonPositivePrice,
                        format!("{name}={v}"),
                    );
                }
            }
        }
        if !fatal {
            if let Some(v) = volume {
                if v < Decimal::ZERO {
                    fail(
                        &mut report,
                        csv_row,
                        ts_raw,
                        &mut fatal,
                        IssueCode::NegativeVolume,
                        format!("volume={v}"),
                    );
                }
            }
        }
        if !fatal {
            if let Err(detail) = check_ohlc(open, high, low, close) {
                fail(
                    &mut report,
                    csv_row,
                    ts_raw,
                    &mut fatal,
                    IssueCode::OhlcViolation,
                    detail,
                );
            }
        }
        if fatal {
            continue;
        }
        let ts = match convention {
            TimestampConvention::Open => ts,
            TimestampConvention::Close => ts - chrono::Duration::seconds(1),
        };
        raw_rows.push(RawRow {
            csv_row,
            ts,
            symbol,
            open,
            high,
            low,
            close,
            volume,
        });
    }
    report.rows_read = rows_seen;

    // Strict mode: any fatal issue collected above aborts with the full report.
    if mode == ValidationMode::Strict && report.issues.iter().any(|i| i.fatal) {
        return Err(strict_fail(&report));
    }

    // ---- duplicates / ordering / interval checks -------------------------
    let mut by_symbol: BTreeMap<String, Vec<RawRow>> = BTreeMap::new();
    for r in raw_rows {
        by_symbol.entry(r.symbol.clone()).or_default().push(r);
    }

    let mut final_series: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
    for (symbol, mut rows) in by_symbol {
        // duplicates: keep the FIRST occurrence, report every duplicate
        let mut seen: BTreeMap<bt_core::time::Ts, usize> = BTreeMap::new();
        let mut kept: Vec<RawRow> = Vec::with_capacity(rows.len());
        for r in rows.drain(..) {
            if let Some(&first_csv_row) = seen.get(&r.ts) {
                report.issues.push(ValidationIssue {
                    row: r.csv_row,
                    ts: Some(format_ts(r.ts)),
                    code: IssueCode::DuplicateTimestamp,
                    detail: format!(
                        "duplicate timestamp; first occurrence kept (csv row {first_csv_row})"
                    ),
                    fatal: true,
                });
                if mode == ValidationMode::Strict {
                    return Err(strict_fail(&report));
                }
                continue;
            }
            seen.insert(r.ts, r.csv_row);
            kept.push(r);
        }
        // ordering
        let out_of_order = kept.windows(2).any(|w| w[1].ts < w[0].ts);
        if out_of_order {
            report.issues.push(ValidationIssue {
                row: 0,
                ts: None,
                code: IssueCode::OutOfOrder,
                detail: format!("rows out of order for symbol {symbol}; stable sort applied"),
                fatal: true,
            });
            if mode == ValidationMode::Strict {
                return Err(strict_fail(&report));
            }
            kept.sort_by_key(|r: &RawRow| r.ts); // stable
        }
        // interval consistency
        let interval_secs =
            expected_interval_secs.unwrap_or_else(|| infer_interval(&kept, &mut report));
        for w in kept.windows(2) {
            let d = (w[1].ts - w[0].ts).num_seconds();
            if d != interval_secs {
                report.issues.push(ValidationIssue {
                    row: 0,
                    ts: Some(format_ts(w[1].ts)),
                    code: IssueCode::MixedInterval,
                    detail: format!("expected {interval_secs}s between bars, found {d}s"),
                    fatal: true,
                });
                if mode == ValidationMode::Strict {
                    return Err(strict_fail(&report));
                }
            }
        }
        let bars = kept
            .into_iter()
            .map(|r| Bar {
                open_time: r.ts,
                open: r.open,
                high: r.high,
                low: r.low,
                close: r.close,
                volume: r.volume,
            })
            .collect::<Vec<_>>();
        report.rows_kept += bars.len();
        final_series.insert(symbol, bars);
    }

    if final_series.is_empty() {
        return Err(CoreError::InvalidData(
            "no valid rows in data file".to_string(),
        ));
    }

    let base_interval_secs = expected_interval_secs.unwrap_or_else(|| {
        final_series
            .values()
            .next()
            .and_then(|bars| {
                if bars.len() >= 2 {
                    Some((bars[1].open_time - bars[0].open_time).num_seconds())
                } else {
                    None
                }
            })
            .unwrap_or(0)
    });
    if base_interval_secs <= 0 {
        return Err(CoreError::InvalidData(
            "could not infer a constant bar interval (need at least 2 bars)".into(),
        ));
    }

    let normalized_hash = compute_normalized_hash(&final_series, base_interval_secs);
    let series = final_series
        .into_iter()
        .map(|(symbol, bars)| {
            (
                symbol.clone(),
                BarSeries {
                    symbol,
                    interval_secs: base_interval_secs,
                    bars,
                },
            )
        })
        .collect();

    Ok(Dataset {
        series,
        actions: Vec::new(),
        base_interval_secs,
        data_hash,
        normalized_hash,
        report,
    })
}

fn infer_interval(rows: &[RawRow], report: &mut ValidationReport) -> i64 {
    if rows.len() < 2 {
        return 0;
    }
    let mut diffs: Vec<i64> = rows
        .windows(2)
        .map(|w| (w[1].ts - w[0].ts).num_seconds())
        .collect();
    diffs.sort_unstable();
    let median = diffs[diffs.len() / 2];
    if diffs.iter().any(|d| *d != median) {
        report.issues.push(ValidationIssue {
            row: 0,
            ts: None,
            code: IssueCode::MixedInterval,
            detail: format!("irregular bar spacing; inferred interval {median}s from median diff"),
            fatal: false,
        });
    }
    median
}

fn strict_fail(report: &ValidationReport) -> CoreError {
    CoreError::InvalidData(format!(
        "data validation failed (strict mode): {}",
        report.summary()
    ))
}

pub fn compute_normalized_hash(series: &BTreeMap<String, Vec<Bar>>, interval_secs: i64) -> String {
    let mut doc = serde_json::Map::new();
    doc.insert("interval_secs".into(), serde_json::json!(interval_secs));
    let mut symbols = serde_json::Map::new();
    for (symbol, bars) in series {
        let arr: Vec<serde_json::Value> = bars
            .iter()
            .map(|b| {
                serde_json::json!({
                    "t": format_ts(b.open_time),
                    "o": b.open.to_string(),
                    "h": b.high.to_string(),
                    "l": b.low.to_string(),
                    "c": b.close.to_string(),
                    "v": b.volume.as_ref().map(|v| v.to_string()),
                })
            })
            .collect();
        symbols.insert(symbol.clone(), serde_json::Value::Array(arr));
    }
    doc.insert("symbols".into(), serde_json::Value::Object(symbols));
    bt_core::hash::hash_json(&serde_json::Value::Object(doc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpfile(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("bt_data_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    const GOOD: &str = "timestamp,symbol,open,high,low,close,volume\n\
        2024-01-01T00:00:00Z,EURUSD,1.1000,1.1010,1.0990,1.1005,1000\n\
        2024-01-01T01:00:00Z,EURUSD,1.1005,1.1020,1.1000,1.1015,1200\n";

    #[test]
    fn loads_good_data() {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ds = load_bars_csv(
            &tmpfile("good.csv", GOOD),
            utc,
            TimestampConvention::Open,
            ValidationMode::Strict,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        assert_eq!(ds.series.len(), 1);
        assert!(ds.report.is_clean());
        assert_eq!(ds.base_interval_secs, 3600);
    }

    #[test]
    fn strict_rejects_ohlc_violation() {
        let bad = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T00:00:00Z,EURUSD,1.1000,1.0900,1.0990,1.1005,1000\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let err = load_bars_csv(
            &tmpfile("bad_ohlc.csv", bad),
            utc,
            TimestampConvention::Open,
            ValidationMode::Strict,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap_err();
        assert!(err.to_string().contains("OHLC_VIOLATION"));
    }

    #[test]
    fn permissive_drops_and_reports() {
        let bad = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T00:00:00Z,EURUSD,1.1000,1.1010,1.0990,1.1005,1000\n\
            2024-01-01T01:00:00Z,EURUSD,1.1005,1.1020,1.1000,0,1200\n\
            2024-01-01T02:00:00Z,EURUSD,1.1015,1.1030,1.1010,1.1025,900\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ds = load_bars_csv(
            &tmpfile("perm.csv", bad),
            utc,
            TimestampConvention::Open,
            ValidationMode::Permissive,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        assert_eq!(ds.series["EURUSD"].bars.len(), 2);
        assert!(ds
            .report
            .issues
            .iter()
            .any(|i| i.code == IssueCode::NonPositivePrice));
    }

    #[test]
    fn duplicate_timestamps_detected() {
        let dup = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T00:00:00Z,EURUSD,1.1,1.1,1.1,1.1,1\n\
            2024-01-01T00:00:00Z,EURUSD,1.1,1.1,1.1,1.1,1\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let strict = load_bars_csv(
            &tmpfile("dup.csv", dup),
            utc,
            TimestampConvention::Open,
            ValidationMode::Strict,
            LoadLimits::default(),
            Some(3600),
        );
        assert!(strict.is_err());
        let perm = load_bars_csv(
            &tmpfile("dup2.csv", dup),
            utc,
            TimestampConvention::Open,
            ValidationMode::Permissive,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        assert_eq!(perm.series["EURUSD"].bars.len(), 1);
        assert!(perm
            .report
            .issues
            .iter()
            .any(|i| i.code == IssueCode::DuplicateTimestamp));
    }

    #[test]
    fn shuffled_rows_normalize_identically() {
        let shuffled = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T01:00:00Z,EURUSD,1.1005,1.1020,1.1000,1.1015,1200\n\
            2024-01-01T00:00:00Z,EURUSD,1.1000,1.1010,1.0990,1.1005,1000\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let a = load_bars_csv(
            &tmpfile("ord.csv", GOOD),
            utc,
            TimestampConvention::Open,
            ValidationMode::Permissive,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        let b = load_bars_csv(
            &tmpfile("shuf.csv", shuffled),
            utc,
            TimestampConvention::Open,
            ValidationMode::Permissive,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        assert_eq!(a.normalized_hash, b.normalized_hash);
    }

    #[test]
    fn missing_volume_is_warning_not_fatal() {
        let novol = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T00:00:00Z,EURUSD,1.1,1.1,1.1,1.1,\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ds = load_bars_csv(
            &tmpfile("novol.csv", novol),
            utc,
            TimestampConvention::Open,
            ValidationMode::Strict,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        assert_eq!(ds.series["EURUSD"].bars[0].volume, None);
        assert!(ds
            .report
            .issues
            .iter()
            .any(|i| i.code == IssueCode::MissingValue));
    }

    #[test]
    fn rejects_nan_inf() {
        let nan = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T00:00:00Z,EURUSD,NaN,1.1,1.1,1.1,1\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let err = load_bars_csv(
            &tmpfile("nan.csv", nan),
            utc,
            TimestampConvention::Open,
            ValidationMode::Strict,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap_err();
        assert!(err.to_string().contains("INVALID_NUMBER"));
    }

    #[test]
    fn timestamp_convention_close_shifts() {
        // A close-convention file: timestamps are bar END times; open times
        // are reconstructed as ts-1s (documented convention).
        let content = "timestamp,symbol,open,high,low,close,volume\n\
            2024-01-01T01:00:00Z,EURUSD,1.1,1.1,1.1,1.1,1\n\
            2024-01-01T02:00:00Z,EURUSD,1.1,1.1,1.1,1.1,1\n";
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ds = load_bars_csv(
            &tmpfile("conv.csv", content),
            utc,
            TimestampConvention::Close,
            ValidationMode::Strict,
            LoadLimits::default(),
            Some(3600),
        )
        .unwrap();
        assert_eq!(
            format_ts(ds.series["EURUSD"].bars[0].open_time),
            "2024-01-01T00:59:59Z"
        );
    }
}
