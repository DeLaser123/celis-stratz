# Strategy Specification (machine-readable)

The strategy spec is a declarative YAML/JSON document. It is designed so an AI can
compile a natural-language strategy into it, and the engine can **prove** the result
is unambiguous (`stratz validate-strategy` → `VALID` or `INVALID` + exact reasons).
Unknown fields are rejected (`deny_unknown_fields`) to catch typos. Ambiguity is an
error — the engine never invents missing logic.

## Full schema

```yaml
strategy:
  name: sma_cross              # required
  symbols: [EURUSD]            # required; must exist in data
  timeframes: ["1H"]           # optional extra HTFs, e.g. ["1D"]; base TF from config
  entry:                       # required
    direction: long            # long | short
    when:                      # boolean condition tree
      all:
        - cross_above: [{field: close}, {sma: {source: {field: close}, period: 20}}]
        - gt: [{field: close}, 1.0]
  entry_short:                 # optional (separate short-side entry rule)
    direction: short
    when: {any: [ ... ]}
  exit:                        # optional; evaluated every bar for open positions
    when: {any: [ ... ]}
  orders:
    entry: {type: market}      # v0.1: market entries
    stop_loss:                 # none (default) | fixed_distance {value} | atr_multiple {period, multiple}
      type: fixed_distance
      value: 0.0050
    take_profit:               # none | fixed_distance | risk_multiple {value}
      type: risk_multiple
      value: 3.0
    trailing_stop: null        # none | atr_multiple | fixed_distance {value}
  risk:
    sizing:                    # mode + value
      mode: percent_risk       # fixed_quantity | fixed_amount | percent_equity | percent_risk | risk_amount | atr_based
      value: 1.0
```

## Expression language

Every node is one of: a scalar literal, `{field: X}`, `{timeframe: "1D", of: <expr>}`
(HTF), or a single-key operator mapping. Types: `Num(Decimal)` / `Bool`. A condition
must be `Bool` — an indicator value used directly as a condition is a validation error.

| Category | Operators |
|---|---|
| comparison | `gt gte lt lte eq ne` |
| boolean | `and` (list) · `or` (list) · `not` |
| arithmetic | `add sub mul div abs` |
| cross | `cross_above cross_below` (previous vs current evaluated value) |
| range | `between {value, min, max}` |
| momentum | `pct_change {of, bars}` |
| rolling | `rolling_high rolling_low {of, period}` |
| indicators | `sma ema wma rsi atr stddev roc highest lowest` `{source, period}` · `macd {source, fast, slow, signal, component: macd|signal|hist}` · `bollinger {source, period, k, component: upper|middle|lower}` |
| time | `time_of_day {from: "HH:MM", to, tz}` · `day_of_week: [mon..sun]` |
| position | `position_direction (long|short|flat)` · `bars_in_position` · `unrealized_pnl` · `unrealized_pnl_pct` |
| account | `equity` · `balance` · `drawdown_pct` |
| external | `signal {key}` (timestamped external input; value visible only after its timestamp) |

`{field: X}` supports `open high low close volume hl2 hlc3 ohlc4`; with
`{timeframe: "1D", of: {field: close}}` it reads the daily series (closed bars only).

## Indicator conventions (causal, documented)

- **SMA**: arithmetic mean of last `period` values; defined after `period` bars.
- **EMA**: seeded with SMA of first `period` values; defined from bar `period`.
- **WMA**: linearly weighted, most recent has weight `period`.
- **RSI**: Wilder's smoothing; `period` bars to first value.
- **ATR**: Wilder-smoothed true range; first value = SMA of TR over `period`.
- **MACD**: EMA(fast) − EMA(slow); signal = EMA(signal) of MACD.
- **Bollinger**: SMA ± `k × population stddev` (documented choice).
- **stddev**: population stddev over window (consistent with Bollinger).
- **roc**: `(cur / prev@period − 1) × 100`.
- `highest`/`lowest`: extremes of last `period` bars **including current**.

Indicators are computed per `(symbol, timeframe, canonical-key)`; identical nodes share
one series. Precompute (vectorized) and streaming paths must produce identical values
(differential-tested; see TESTING.md §reference-vs-optimized).

## External / macro inputs

- **Static**: `config.macro_inputs: {key: value}` — experiment-level constants.
- **Time-varying**: CSV `timestamp,key,value` supplied via `--signals`; `{signal: {key}}`
  resolves to the last value with `timestamp ≤ decision time` (information availability
  enforced; later observations can never influence earlier decisions).

## Evaluation order & entry semantics

Each bar's decision evaluates, in order:
1. **exit** conditions (if a position is open) → close request;
2. **entry** conditions — always evaluated. Against an *open opposite-side*
   position the opposite rule is checked FIRST (a reversal request); against a
   same-side position a matching rule requests a pyramiding add (bounded by
   `risk.max_entries_per_position`, default 1 = ignore repeat signals). The
   risk engine decides what actually happens.

`NA` (undefined) semantics: indicator warmup, missing volume, division by zero
and not-yet-visible timeframes evaluate to `NA`; any `NA` inside a condition
makes the condition FALSE (documented, conservative: undefined data never
generates a signal). A missing source value also RESETS the indicator window —
averages are never computed across data gaps.

**Timeframe references must be declared**: any `{timeframe: "1D", ...}` (or an
indicator sourced from one) requires `strategy.timeframes: [1D]`; an undeclared
reference is a validation error, never a silent runtime `NA`.

## Rejection philosophy

Examples of INVALID specs: condition on an undefined timeframe; `cross_above` against a
`Bool`; sizing `percent_risk` without any stop definition; unknown indicator or field;
entry direction `both` (ambiguous — declare `entry` and `entry_short` separately).
Each failure returns the exact reason and YAML path.
