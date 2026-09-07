# Stratz

A professional-grade, deterministic, auditable trading stratzing engine in Rust.
OHLCV CSV in → machine-readable strategy spec → exact, reproducible simulation →
complete trade ledger, equity curve, event log, and performance report.

**Design order (hard):** CORRECTNESS > DETERMINISM > AUDITABILITY > TESTABILITY >
EXTENSIBILITY > PERFORMANCE.

## Quick start

```bash
cargo build --release
target/release/stratz validate-data examples/data_eurusd_1h.csv
target/release/stratz run \
  --data examples/data_eurusd_1h.csv \
  --strategy examples/strategy_sma_cross.yaml \
  --config examples/config_eurusd.yaml \
  --out results/run1
target/release/stratz report results/run1
```

Outputs in `results/run1/`: `summary.json metrics.json experiment.json trades.csv
orders.csv fills.csv equity_curve.csv drawdown.csv daily_returns.csv
monthly_returns.csv rolling_metrics.csv event_log.jsonl monte_carlo.json`.

Same inputs ⇒ byte-identical outputs (`experiment.json.experiment_id` /
`result_hash` prove it). See DETERMINISM.md.

## Packaging & operations

- **Sample enterprise project**: [sample-project/](sample-project/) — a
  complete runnable project (two strategy versions, data, signals, notes,
  stress scenarios). `scripts/demo.sh` runs the entire workflow on a scratch
  copy: doctor → runs → sweep → walk-forward → stress → robustness → query →
  HTML report → AI dry-run.
- **Installers**: `scripts/install.sh` (Linux/macOS) and
  `scripts/install.ps1` (Windows) build from source and install the binary.
  Tagged releases attach per-OS archives automatically (`.github/workflows/release.yml`).
- **CI gates**: fmt, clippy (`-D warnings`), full test suite, determinism
  suite, and sample-project doctor on every push (`.github/workflows/ci.yml`).
- **Docs site**: mdBook ([book.toml](book.toml)) built and published by
  `.github/workflows/docs.yml`.
- **API stability & changelog**: [RELEASES.md](RELEASES.md).

## Documentation

| Doc | Contents |
|---|---|
| ARCHITECTURE.md | components, data flow, event model |
| DATA_MODEL.md | CSV schema, validation rules, resampling |
| STRATEGY_SPEC.md | full machine-readable strategy schema + expression language |
| EXECUTION_MODEL.md | time semantics, intrabar triggers, ambiguity policies, costs |
| ACCOUNTING.md | account model, netting positions, ledger, invariants |
| METRICS.md | every metric's exact definition + conventions |
| TESTING.md | test architecture, fixtures, acceptance gates |
| DETERMINISM.md | nondeterminism elimination, experiment identity |
| LIMITATIONS.md | what this engine does NOT (yet) do |

## Workspace

`bt-core` · `bt-data` · `bt-strategy` · `bt-execution` · `bt-accounting` · `bt-risk` ·
`bt-analytics` · `bt-simulation` · `bt-cli` (binary `stratz`).

## Dependency justification

| Crate | Why |
|---|---|
| `rust_decimal` (+macros) | exact fixed-precision money/price arithmetic; no float accounting |
| `serde`, `serde_json`, `serde_yaml` | config/spec/output serialization; canonical JSON for hashing |
| `csv` | streaming CSV ingestion |
| `chrono`, `chrono-tz` | timestamps, timezones, sessions; UTC canonicalization |
| `sha2` | experiment/result hashes (determinism, golden tests) |
| `clap` | CLI |
| `thiserror` | typed error hierarchy |
| `tracing` | structured runtime logging (opt-in) |
| `rayon` | parallel *independent* runs only (never inside one simulation) |
| `proptest` (dev) | property-based invariant tests |
| `statrs` — intentionally omitted | needed stats (mean/std/skew/kurtosis/median) are implemented directly for auditability |

No dependency is added without a purpose above.
