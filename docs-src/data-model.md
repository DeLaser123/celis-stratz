# Data Model

## Bar schema (CSV)

Required columns (header row required, order-free):

```
timestamp,symbol,open,high,low,close,volume
```

- `timestamp`: RFC3339 with offset (`2024-01-01T10:00:00Z`), or naive
  `YYYY-MM-DD HH:MM:SS` interpreted in `market.timezone` (default `UTC`), or Unix
  seconds (all-digit string ≤ 12 chars).
- `symbol`: string, case-sensitive.
- `open/high/low/close`: decimal numbers. Positive, finite. `NaN`, `inf`, empty → error.
- `volume`: non-negative decimal; may be empty only if strategy does not use volume
  (reported as warning).

Timestamp convention: `market.timestamp_convention` ∈ {`open` (default), `close`}.
Bar interval must be given (`market.timeframe`, e.g. `1h`, `15m`, `1d`, `1w`) and must
be **constant** across the file (validated: min diff of consecutive timestamps).

## OHLC invariants (checked per row)

```
high >= max(open, close, low)
low  <= min(open, close, high)
open, high, low, close > 0
volume >= 0
```

## Validation modes

- **strict** (default): any issue aborts the run with a full report.
- **permissive**: issues are reported; fixes applied in a fixed order:
  1. drop rows with invalid values/OHLC violations,
  2. deduplicate timestamps (keep first occurrence),
  3. sort by timestamp (stable).
  Every fix is reported and recorded in `experiment.json` (`data_issues` count by code).
  **Never silent**: `RunStarted`/`DataWarning` events list all issues.

Issue codes: `MISSING_VALUE, INVALID_NUMBER, NON_POSITIVE_PRICE, NEGATIVE_VOLUME,
OHLC_VIOLATION, DUPLICATE_TIMESTAMP, OUT_OF_ORDER, MIXED_INTERVAL, UNPARSEABLE_TIMESTAMP,
AMBIGUOUS_LOCAL_TIME, SCHEMA_ERROR`.

Safety limits: `limits.max_file_mb` (default 512), `limits.max_rows` (default 10,000,000).

## Hashing

- `data_hash` = SHA-256 of the raw file bytes.
- `normalized_data_hash` = SHA-256 of the canonical serialization of the validated
  dataset (symbol, interval, bars as RFC3339+decimals). Two differently-ordered files
  with identical normalized content therefore share this hash (determinism test #9).

## Multi-timeframe

Base bars are resampled to every higher timeframe listed in `strategy.timeframes`.
Resample rule: align bar open-times to epoch boundaries (`1w` = ISO week, Monday 00:00 UTC);
`open`=first, `high`=max, `low`=min, `close`=last, `volume`=sum, `open_time`=aligned floor.
The trailing (possibly partial) HTF bar is never observable before its close time.

## Corporate actions (optional side-file)

`Data/corporate_actions.csv` (loaded automatically when present next to the
data file):

```
timestamp,symbol,kind,value
2024-02-01T00:00:00Z,X,dividend,0.50
2024-03-01T00:00:00Z,X,split,2.0
```

- `dividend`: cash per unit in quote currency, credited on the ex-date bar to
  longs (shorts pay). Included in trade `net_pnl` and reported separately.
- `split`: new units per old unit. Adjusts open positions, protective levels,
  and pending orders at the ex-date bar. **Use with unadjusted price data only.**

## Parquet

`--data file.parquet` is auto-detected. Same schema: timestamp (Int64 Unix
seconds / Utf8 / timestamp micros), symbol, open, high, low, close (Float64 or
Decimal128), volume nullable. Same validation and normalization semantics as
CSV (same `normalized_data_hash` formula).

## Future data layers (out of scope, interface reserved)

tick data, bid/ask, L1/L2, funding — see LIMITATIONS.md.
