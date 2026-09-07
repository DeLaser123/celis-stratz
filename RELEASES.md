# Releases & API Stability Policy

## Versioning

- The workspace follows **Cargo semver**: `0.Y.Z` until 1.0.
- `0.Y` increments are **minor releases** (new features, breaking changes allowed
  with migration notes in this file). `Z` increments are patches (bug fixes only,
  no behavior changes unless the fix restores documented behavior).
- **The engine version is stamped into every `experiment.json`** — a result is
  only reproducible with the exact engine version that produced it. Upgrades
  must re-run stratzs before comparing result hashes.

## What counts as public API

| Layer | Stability commitment |
|---|---|
| `stratz` CLI (command/flag surface) | stable within a minor release; deprecations announced one minor ahead |
| Output file schemas (`summary.json`, `metrics.json`, CSVs, `experiment.json`) | additive only within a minor release; renames/removals bump `Y` |
| `experiment_id` / `result_hash` formulas | frozen within a minor release; changes bump `Y` and invalidate old hashes |
| Registry schema (`registry.db`) | migrated forward automatically; never destroyed |
| Kernel crate public Rust APIs (`bt-*`) | best-effort stability; 0.x minor bumps may change signatures (this is a library-internal contract, the CLI is the product surface) |
| AI audit ledger (`.stratz/ai/ledger.jsonl`) | append-only, additive fields only |
| Strategy spec grammar | additive only; breaking grammar changes bump `Y` and are validated loudly by the compiler |

## Non-goals (never guaranteed)

- Byte-identity of AI model outputs (models are non-deterministic; the ledger
  records what happened, determinism applies to the simulation kernel).
- Registry file format across major rewrites (auto-migration only).

## Changelog

### 0.1.0 — deterministic kernel
- Event-driven OHLCV stratzer: Decimal accounting, netting positions,
  intrabar ambiguity policies, cost stack, risk engine, full metrics,
  Monte Carlo, experiment identity hashes, JSONL audit trail.

### 0.2.0 — enterprise harness + research suite (Phase 1)
- Folder-native project contract (`init`/`doctor`), SQLite experiment registry
  with lineage, parameter sweeps (rayon), walk-forward analysis, Deflated
  Sharpe + PBO/CSCV, read-only SQL query engine, self-contained HTML reports.

### 0.3.0 — stress engine (Phase 2)
- Deterministic scenario transforms (gap shock, volatility regime, liquidity
  drought, drift), cost overlays, historical stress presets, scenario×
  perturbation stress matrix with worst-case aggregation.

### 0.4.0 — AI at the boundaries (Phase 3)
- `bt-ai` gateway (Z.ai/GLM + OpenAI-compatible), `ai compile` with
  engine-validated repair loop, `ai review`/`explain`/`ask` with quoted-number
  verification, token budgets, prompt-hash cache, append-only AI audit ledger.
- Key handling: `--api-key` / `STRATZ_API_KEY` / OS keyring.

### 0.5.0 — data & execution realism (Phase 4)
- Limit/Stop/StopLimit **entry** orders with expression prices; square-root
  impact slippage; participation-capped partial fills; corporate actions
  (cash dividends + splits); cross-symbol price fields; financing term curve;
  Parquet ingestion.

### Next (candidates)
- Portfolio-level exposure constraints; multi-currency accounting;
  indicators on higher timeframes across symbols; docs site build in CI.
