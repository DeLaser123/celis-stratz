# Limitations (v0.1 — honest list)

The engine is a faithful OHLC-bar stratzer under the assumptions below. It is NOT
yet a tick-level or multi-asset-corporate-actions engine. Deferred items are deliberate
scope decisions, not omissions by accident.

## Data
- OHLCV bars from CSV or Parquet. Corporate actions (cash dividends + splits) are
  supported via a side-file; funding and L1/L2 remain future work. No survivorship
  handling (universe management is future work).

## Execution
- OHLC intrabar uncertainty is irreducible; the configured ambiguity *policy* decides,
  and ambiguity can be surfaced (`reject` policy) rather than guessed.
- Trailing stops move only at bar close; they cannot trigger inside the bar that moves them.
- Limit/Stop/StopLimit entry orders are strategy-facing (v0.1) and trigger intrabar
  with gap-aware fills, but there is no order queue position modeling.
- Partial fills exist for entries via `execution.participation_cap` (fraction of bar
  volume); remainders keep working at later bars. Exits and stop-loss fills are
  never capped (risk exits must execute).
- Settlement is immediate; no T+1 cash settlement, no trade-date vs settle-date split.
- Margin violations are reported but there is no automatic liquidation cascade.
- Cross-symbol expressions support plain price fields only (no indicators on other
  symbols yet); financing supports flat daily rates and a term curve, not rate curves
  by currency or tenor.
- Splits assume UNADJUSTED price data; combining split actions with adjusted
  prices double-counts the split (documented in DATA_MODEL.md).

## Accounting
- Quote currency == account currency. No FX conversion, no multi-currency books.
- Netting per symbol (no hedging mode).
- Financing: flat daily rate model only; no curve, no swap triple-point days.
- Borrow cost for shorts is expressible only via the financing rate (not a separate
  availability-driven model).

## Strategy
- Expressions are bar-based; no order-matrix/option payoff language.
- Position sizing executes at an *estimated* price (decision-time close); realized risk
  may differ from intended risk when the next open gaps (recorded in the ledger).

## Research workflow
- Optimization (grid/random search, walk-forward, OOS splitting) is intentionally not
  implemented in v0.1 (spec §23: correct engine first). The run API is designed so a
  sweep is a loop over parameterized specs; rayon is available for it.
- Anti-bias system v0.1: look-ahead, duplicate, ordering, timezone, OHLC, and
  intrabar-ambiguity protections are implemented; survivorship/selection/snooping
  warnings are procedural (docs), not yet automated.
- Reference-vs-optimized differential testing covers the indicator computation layer
  (the only optimized layer in v0.1); a full second engine implementation is future work.

## Statistics
- Sharpe assumes i.i.d. returns (no autocorrelation correction); annualization uses
  the documented `periods_per_year` model (24h market). Risk of ruin is Monte-Carlo
  based on historical trade sequences, not a closed-form model.
