# Stratz Backtest Engine — Architecture

Version: 0.1.0 (engine version recorded in every `experiment.json`).

## 1. Purpose

A deterministic, auditable, event-driven backtesting engine for mechanically defined
trading strategies. It is a **library first**; the CLI is only one consumer.

Design priority order (hard): `CORRECTNESS > DETERMINISM > AUDITABILITY > TESTABILITY > EXTENSIBILITY > PERFORMANCE`.

## 2. Component diagram

```
                        ┌────────────────────────────────────────────┐
                        │                 bt-cli                     │
                        │  run / validate-data / validate-strategy   │
                        │  report / compare / monte-carlo / version  │
                        └────────────────────┬───────────────────────┘
                                             │
                        ┌────────────────────▼───────────────────────┐
                        │              bt-simulation                 │
                        │   (event loop / reference engine, outputs) │
                        └─┬──────┬───────┬───────┬───────┬────────────┘
                          │      │       │       │       │
              ┌───────────▼┐ ┌───▼────┐ ┌▼──────┐ ┌────▼───┐ ┌────▼─────────┐
              │  bt-data   │ │bt-strat│ │bt-exec│ │bt-acct │ │ bt-analytics │
              │ csv/valid/ │ │egy     │ │ution  │ │        │ │ metrics, MC  │
              │ resample   │ │indicators│ │costs │ └────┬───┘ └──────┬───────┘
              └─────┬──────┘ │spec/expr│ └──┬───┘      │            │
                    │        └───┬────┘    │      ┌───▼────┐  ┌────▼─────┐
              ┌─────▼────────────▼─────────▼──────│bt-risk │  │  bt-core │
              │        bt-core (shared domain)    └────────┘  └──────────┘
              └───────────────────────────────────────────────────────────┘
```

Crates (each with one responsibility):

| Crate | Responsibility | Depends on |
|---|---|---|
| `bt-core` | Domain types: decimal helpers, time model, instruments, orders, events, ledger, hashing, PRNG, errors | — |
| `bt-data` | Bar model, CSV ingestion, validation (strict/permissive), time-frame resampling | core |
| `bt-strategy` | Indicator engine, typed expression system, machine-readable strategy spec + validator | core, data |
| `bt-execution` | Cost models, order lifecycle, fill simulator, intrabar trigger/ambiguity logic | core |
| `bt-accounting` | Netting position engine, margin account, immutable ledger | core |
| `bt-risk` | Position sizing + pre-trade risk validation | core |
| `bt-analytics` | Performance metrics, distributions, Monte Carlo | core |
| `bt-simulation` | The deterministic event loop that wires everything; output writers | all |
| `bt-cli` | clap CLI binary `stratz` | simulation |

## 3. Data flow (per bar)

```
CSV ──► validate ──► normalized bars ──► (resample to higher timeframes)
   per bar N:
     T0 open_time:      execute pending market orders at open (+costs)
                        create SL/TP orders anchored to fill price
     intrabar:          evaluate stop/limit/trailing triggers on OHLC
                        ambiguity policy decides SL-vs-TP ordering
                        update MAE/MFE; apply financing at close
     T1 close_time:     update indicators with bar N (causal only)
                        strategy.on_bar → intents
                        risk engine validates + sizes → orders
                        (current_close model: fill at close now)
                        record equity snapshot
```

Every transition emits `Event`s to an append-only JSONL event log. The strategy
only ever *requests*; execution/accounting *decide*.

## 4. Domain model (summary; details in DATA_MODEL/ACCOUNTING/EXECUTION docs)

- **Money/prices/quantities**: `rust_decimal::Decimal` everywhere in the ledger.
  f64 only inside statistics, after explicit documented conversions.
- **Time**: `chrono::DateTime<Utc>` internally; bar timestamps per configured
  convention (`open` default); decision timestamp = bar close time.
- **Position**: netting per symbol (weighted-average entry, FIFO-free);
  partial exits, reversals, pyramiding (config-capped) supported.
- **Account**: margin account — `balance` (realized), `equity = balance + unrealized`,
  `margin_used = Σ|notional|/leverage`, `free_margin = equity − margin_used`.
- **Orders**: explicit lifecycle `Created → Accepted → Active → PartiallyFilled → Filled |
  Rejected | Cancelled | Expired`; history is append-only.
- **Ledger**: every balance/equity change has a `LedgerEntry` referencing an event.

## 5. Event model

Ordered, strictly increasing `seq` (u64) + timestamp. Types: `RunStarted,
DataWarning, MarketEvent, IndicatorUpdate*, StrategySignal, OrderIntent, OrderCreated,
OrderAccepted, OrderRejected, OrderActivated, OrderFilled, OrderCancelled, OrderExpired,
PositionOpened, PositionIncreased, PositionReduced, PositionReversed, PositionClosed,
AmbiguityDeferred, FinancingApplied, AccountUpdated, RiskRejected, MarginViolation,
EquitySnapshot, RunFinished`. See `event.rs` in bt-core. Event log is JSONL:
`{"seq":..,"ts":"...","event":"ORDER_FILLED","payload":{...}}`.

## 6. Separation of powers

- Strategy (compiled from spec): read-only view (`StrategyContext`), returns `Intent`s.
- Risk engine: may **reject** intents; computes position size.
- Execution simulator: owns fill logic and costs; produces `Fill`s.
- Accounting: owns balances/positions/ledger; the only place account state mutates.
- Nothing else may mutate account state. Violations are compile-time module boundaries.

## 7. Extension points

- Cost models: implement `CostModel` enums (extensible via new variants).
- Indicators: add to registry + expression parser (no engine changes).
- Data: new `MarketDataSource` implementations (tick/L2 later).
- Engines: `SimulationEngine` trait — the current implementation is the *reference*
  engine; any future optimized engine must be differential-tested against it.
