//! Parquet ingestion (spec §5 future-data goal): OHLCV columns with the same
//! validation semantics as CSV. Timestamps may be Int64 (Unix seconds) or
//! Utf8 (RFC3339 / naive-in-tz); prices Float64 or Decimal128; volume
//! nullable.
//!
//! Float64 → Decimal conversion uses the shortest exact decimal that
//! round-trips the f64 (`from_f64_retain`) — deterministic and documented.

use crate::bar::{Bar, BarSeries, Dataset};
use crate::csv::LoadLimits;
use crate::validate::ValidationReport;
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::{parse_timestamp, Ts};
use bt_core::D;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Load OHLCV bars from a Parquet file.
pub fn load_bars_parquet(
    path: &Path,
    tz: chrono_tz::Tz,
    limits: LoadLimits,
    expected_interval_secs: Option<i64>,
) -> CoreResult<Dataset> {
    let _ = limits; // size guard enforced by reader below
    let bytes = std::fs::read(path).map_err(CoreError::Io)?;
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::from(bytes),
    )
    .map_err(|e| CoreError::InvalidData(format!("parquet open {}: {e}", path.display())))?;
    let schema = reader.schema().clone();
    let rows: usize = reader.metadata().file_metadata().num_rows() as usize;

    let batches: Vec<_> = reader
        .build()
        .map_err(|e| CoreError::InvalidData(format!("parquet read {}: {e}", path.display())))?
        .collect::<Result<_, _>>()
        .map_err(|e| CoreError::InvalidData(format!("parquet batch {}: {e}", path.display())))?;

    let col = |name: &str| -> Option<usize> {
        schema.fields().iter().position(|f| f.name().trim() == name)
    };
    for required in ["timestamp", "symbol", "open", "high", "low", "close"] {
        if col(required).is_none() {
            return Err(CoreError::InvalidData(format!(
                "{}: parquet schema missing required column '{required}'",
                path.display()
            )));
        }
    }

    // Flatten columns into per-row accessors.
    type RawRow = (Ts, String, D, D, D, D, Option<D>);
    type PriceRow = (Ts, D, D, D, D, Option<D>);
    let mut raw_rows: Vec<RawRow> = Vec::with_capacity(rows);
    let mut report = ValidationReport {
        rows_read: rows,
        ..Default::default()
    };

    let timestamp_col = col("timestamp").unwrap();
    let symbol_col = col("symbol").unwrap();
    let open_col = col("open").unwrap();
    let high_col = col("high").unwrap();
    let low_col = col("low").unwrap();
    let close_col = col("close").unwrap();
    let volume_col = col("volume");

    for batch in &batches {
        let n = batch.num_rows();
        for i in 0..n {
            let ts_val = arrow_get(batch.column(timestamp_col), i)?;
            let ts_raw = match ts_val {
                ArrowVal::Int(secs) => {
                    Ts::from_timestamp(secs, 0).ok_or_else(|| CoreError::InvalidTimestamp {
                        context: path.display().to_string(),
                        detail: format!("unix seconds {secs} out of range"),
                    })?
                }
                ArrowVal::F64(secs) => Ts::from_timestamp(secs as i64, 0).ok_or_else(|| {
                    CoreError::InvalidTimestamp {
                        context: path.display().to_string(),
                        detail: format!("unix seconds {secs} out of range"),
                    }
                })?,
                ArrowVal::Str(s) => {
                    parse_timestamp(&s, tz, &format!("{} row {i}", path.display()))?
                }
                ArrowVal::None => {
                    report.issues.push(crate::validate::ValidationIssue {
                        row: raw_rows.len() + 1,
                        ts: None,
                        code: crate::validate::IssueCode::MissingValue,
                        detail: "missing timestamp".into(),
                        fatal: true,
                    });
                    continue;
                }
            };
            let symbol = match arrow_get(batch.column(symbol_col), i)? {
                ArrowVal::Str(s) => s,
                _ => {
                    return Err(CoreError::InvalidData(format!(
                        "{}: symbol column must be a string",
                        path.display()
                    )))
                }
            };
            let get_px = |c: usize| -> CoreResult<Option<D>> {
                Ok(match arrow_get(batch.column(c), i)? {
                    ArrowVal::Int(v) => Some(D::from(v)),
                    ArrowVal::F64(v) => Some(Decimal::from_f64_retain(v).ok_or_else(|| {
                        CoreError::InvalidData(format!(
                            "{} row {}: value {v} is not representable",
                            path.display(),
                            raw_rows.len() + 1
                        ))
                    })?),
                    ArrowVal::Str(s) => Some(
                        Decimal::from_str_exact(&s)
                            .or_else(|_| Decimal::from_scientific(&s))
                            .map_err(|_| {
                                CoreError::InvalidData(format!(
                                    "{} row {}: '{s}' is not a decimal",
                                    path.display(),
                                    raw_rows.len() + 1
                                ))
                            })?,
                    ),
                    ArrowVal::None => None,
                })
            };
            let Some(open) = get_px(open_col)? else {
                continue;
            };
            let Some(high) = get_px(high_col)? else {
                continue;
            };
            let Some(low) = get_px(low_col)? else {
                continue;
            };
            let Some(close) = get_px(close_col)? else {
                continue;
            };
            let volume = match volume_col {
                Some(c) => get_px(c)?,
                None => None,
            };
            raw_rows.push((ts_raw, symbol, open, high, low, close, volume));
        }
    }

    // Group per symbol, sort, dedup — same normalization contract as CSV.
    let mut by_symbol: BTreeMap<String, Vec<PriceRow>> = BTreeMap::new();
    for (ts, symbol, o, h, l, c, v) in raw_rows {
        by_symbol
            .entry(symbol)
            .or_default()
            .push((ts, o, h, l, c, v));
    }
    let mut final_series: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
    let mut rows_kept = 0usize;
    for (symbol, mut rows) in by_symbol {
        rows.sort_by_key(|r| r.0);
        rows.dedup_by_key(|r| r.0);
        let interval_secs = expected_interval_secs.unwrap_or_else(|| {
            if rows.len() >= 2 {
                (rows[1].0 - rows[0].0).num_seconds()
            } else {
                0
            }
        });
        if interval_secs <= 0 {
            return Err(CoreError::InvalidData(format!(
                "{symbol}: could not infer bar interval (need at least 2 bars)"
            )));
        }
        let bars: Vec<Bar> = rows
            .into_iter()
            .map(|(open_time, open, high, low, close, volume)| Bar {
                open_time,
                open,
                high,
                low,
                close,
                volume,
            })
            .collect();
        rows_kept += bars.len();
        final_series.insert(symbol, bars);
    }
    report.rows_kept = rows_kept;

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
    let data_hash = bt_core::hash::sha256_hex(
        &std::fs::read(path).map_err(|e| CoreError::InvalidData(format!("read: {e}")))?,
    );
    let normalized_hash = crate::csv::compute_normalized_hash(&final_series, base_interval_secs);
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

/// Loosely-typed arrow cell accessor (int/float/string/decimal/null).
enum ArrowVal {
    Int(i64),
    F64(f64),
    Str(String),
    None,
}

fn arrow_get(col: &Arc<dyn arrow::array::Array>, i: usize) -> CoreResult<ArrowVal> {
    use arrow::array::Array;
    if col.is_null(i) {
        return Ok(ArrowVal::None);
    }
    let any = col.as_any();
    if let Some(a) = any.downcast_ref::<arrow::array::Int64Array>() {
        return Ok(ArrowVal::Int(a.value(i)));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::Int32Array>() {
        return Ok(ArrowVal::Int(i64::from(a.value(i))));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::Float64Array>() {
        return Ok(ArrowVal::F64(a.value(i)));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::Float32Array>() {
        return Ok(ArrowVal::F64(f64::from(a.value(i))));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::StringArray>() {
        return Ok(ArrowVal::Str(a.value(i).to_string()));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::LargeStringArray>() {
        return Ok(ArrowVal::Str(a.value(i).to_string()));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::Decimal128Array>() {
        let v = a.value(i);
        let scale = u32::try_from(a.scale()).unwrap_or(0);
        return Ok(ArrowVal::Str(
            rust_decimal::Decimal::from_i128_with_scale(v, scale).to_string(),
        ));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::TimestampMicrosecondArray>() {
        return Ok(ArrowVal::Int(a.value(i) / 1_000_000));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::TimestampNanosecondArray>() {
        return Ok(ArrowVal::Int(a.value(i) / 1_000_000_000));
    }
    if let Some(a) = any.downcast_ref::<arrow::array::TimestampSecondArray>() {
        return Ok(ArrowVal::Int(a.value(i)));
    }
    Err(CoreError::InvalidData(format!(
        "unsupported arrow column type {}",
        col.data_type()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_parquet(path: &Path) {
        use arrow::array::{Float64Array, Int64Array, StringArray};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        let symbol = StringArray::from(vec!["X"; 3]);
        let timestamp = Int64Array::from(vec![1_704_067_200i64, 1_704_067_560, 1_704_067_920]);
        let open = Float64Array::from(vec![100.0, 101.0, 102.0]);
        let high = Float64Array::from(vec![101.0, 102.0, 103.0]);
        let low = Float64Array::from(vec![99.0, 100.0, 101.0]);
        let close = Float64Array::from(vec![100.5, 101.5, 102.5]);
        let volume = Float64Array::from(vec![10.0, 20.0, 30.0]);
        let batch = RecordBatch::try_from_iter(vec![
            (
                "timestamp",
                Arc::new(timestamp) as Arc<dyn arrow::array::Array>,
            ),
            ("symbol", Arc::new(symbol)),
            ("open", Arc::new(open)),
            ("high", Arc::new(high)),
            ("low", Arc::new(low)),
            ("close", Arc::new(close)),
            ("volume", Arc::new(volume)),
        ])
        .unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn parquet_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bt_parquet_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.parquet");
        write_parquet(&path);
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ds = load_bars_parquet(&path, utc, LoadLimits::default(), Some(3600)).unwrap();
        assert_eq!(ds.series.len(), 1);
        let bars = &ds.series["X"].bars;
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].open.to_string(), "100");
        assert_eq!(bars[2].close.to_string(), "102.5");
        assert_eq!(bars[1].volume, Some(D::from_str_exact("20").unwrap()));
        // determinism: same content -> same normalized hash
        let ds2 = load_bars_parquet(&path, utc, LoadLimits::default(), Some(3600)).unwrap();
        assert_eq!(ds.normalized_hash(), ds2.normalized_hash());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
