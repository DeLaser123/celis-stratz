# Metrics Definitions

All statistics consume the **equity curve** (per-bar Decimal equity) and the **trade
ledger**. f64 is used only after the explicit Decimal→f64 conversion (documented; both
are finite and the conversion is total for our ranges).

## Conventions (configurable, printed in every report)

| Setting | Default | Alternatives |
|---|---|---|
| returns frequency | `daily` (resampled last-equity-per-UTC-day) | `per_bar` |
| annualization factor | auto = `periods_per_year(freq)` from timeframe × `trading_days_per_year` (252) | explicit number |
| risk-free rate | 0.0 (annual) | any; per-period rf = `(1+rf)^(1/ppy) − 1` |
| mean/std | arithmetic mean, **sample** stddev (n−1) | population |
| zero-return periods | included | `exclude_zero_returns: true` |
| R convention | `net` (net P&L / initial risk) | `gross` |

`periods_per_year`: daily → `trading_days_per_year`; per-bar → `trading_days_per_year × bars_per_day`
where `bars_per_day = 86400 / bar_interval_seconds` (24h market assumption, documented).

## Definitions

- **Total Return** = final_equity/initial_capital − 1. **CAGR** = (final/initial)^(1/years) − 1,
  `years = (t_end − t_start) / 365.25 days`.
- **Sharpe** = `(mean(r) − rf_period) / stddev(r) × sqrt(ppy)`. Zero dispersion ⇒ `null` (never fake a number).
- **Sortino** = `(mean(r) − rf_period) / downside_dev × sqrt(ppy)`; downside_dev = `sqrt(mean(min(r−rf_period,0)²))`.
- **Calmar** = CAGR / |maxDD%|.
- **Max Drawdown** = max over bars of `(peak − equity)` (Decimal) and `(peak − equity)/peak`.
- **Average Drawdown** = mean of drawdown magnitude over points where in drawdown (dd > 0).
- **Ulcer Index** = `sqrt(mean(dd_pct²))` over all bars.
- **Profit Factor** = Σ wins_gross_net / |Σ losses_net| (net convention configurable `net|gross`).
- **Win/Loss/Breakeven**: net P&L > 0 / < 0 / = 0.
- **Expectancy (R)** = mean(R). **Avg R** = mean(R). **Median/Max/Min R** from trade ledger.
- **Payoff Ratio** = avg win / |avg loss|. **Recovery Factor** = total_return_amount / maxDD_amount.
- **Risk of Ruin**: Monte Carlo (see below), fraction of paths whose equity ever ≤ `ruin_threshold_pct` of initial.
- **Exposure / Time in Market** = fraction of bars with any open position.
- **Streaks**: max consecutive wins/losses in exit-ordered trade sequence.
- **Skewness/Kurtosis**: sample skewness g1 and excess kurtosis g2 of daily returns.
- **MAE/MFE**: per trade, max adverse/favorable **price excursion** vs entry, using bar
  extremes from the entry bar (post-fill) to the exit bar.
- **R-multiple**: `R = convention_PnL / initial_risk`; `initial_risk = |entry − initial_stop| × qty × contract_size`;
  trades without a stop have `R = null` and are excluded from R statistics (counted separately).
- **Monthly/yearly returns**: chained from last-equity-per-month boundaries (month start
  basis = previous month's final equity, or initial capital for the first month).
- **Rolling Sharpe**: rolling window of daily returns (default 126 days) for visualization.

Every metric's intermediate series (returns, drawdowns, R distribution) is exported so
each number can be independently recomputed.

## Monte Carlo

Method `reshuffle` (default) or `bootstrap`, over the sequence of trade net P&Ls.
PRNG: SplitMix64 (explicit `seed`, stored in experiment metadata). Outputs
`monte_carlo.json`: distribution percentiles (p5/p25/p50/p75/p95) of final equity and
max drawdown, and risk-of-ruin. Determinism: same seed ⇒ identical output.
