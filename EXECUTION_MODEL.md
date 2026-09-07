# Execution Model

## Time semantics

| Term | Meaning |
|---|---|
| bar open time | `timestamp` column (per convention) |
| bar close time | open time + interval (exclusive boundary) |
| decision timestamp | close time of the just-closed bar; strategy runs here |
| order submission timestamp | decision timestamp of the bar that produced the signal |
| execution timestamp | when a fill occurs (open of next bar, or close of current bar) |
| settlement | immediate (cash/margin accounting, no T+1) — see LIMITATIONS |

**No look-ahead**: the strategy sees bar *N* (and its indicators) only at bar *N*'s
close time. Higher-timeframe values are visible only for HTF bars whose close time
≤ current decision time. A completed trade can never be affected by later data.

## Execution models (`execution.model`)

| Model | Market order fills at | Fill timestamp |
|---|---|---|
| `next_open` (default) | next bar's open price | next bar open time |
| `current_close` | current bar's close price | current bar close time |

## Intrabar trigger rules (OHLC)

A resting order is evaluated against each bar after activation:

| Order | Trigger (long side) | Fill price (gap-aware) |
|---|---|---|
| Stop-loss (sell) | `low <= stop` | `min(stop, open)` if opened below stop else `stop` |
| Take-profit (sell limit) | `high >= target` | `max(target, open)` if opened above target else `target` |
| Buy stop | `high >= stop` | `max(stop, open)`… uses worse of open/stop |
| Sell limit | `low <= limit` | `min(limit, open)`… |

Short sides are mirrored. `execution.strict_trigger = true` requires strict inequality
(price must trade *through* the level; conservative).

Trailing stops are updated **at bar close** from `trail_on: close` (default,
conservative) or `high_low` (uses bar extreme). A trailing stop that moved on bar *N*
can trigger on bar *N+1* onward (never within the bar that moved it).

## Intrabar ambiguity policy (`execution.intrabar_policy`)

When a bar's range touches both the stop-loss and the take-profit of the same position,
the true path is unknowable from OHLC. Policies:

| Policy | Resolution |
|---|---|
| `conservative` (default) | stop-loss assumed first (worst case) |
| `optimistic` | take-profit assumed first |
| `ohlc_path` | `close >= open` (up bar) ⇒ TP first, else SL first; doji ⇒ conservative |
| `reject` | **no guess**: exit deferred; event `AmbiguityDeferred`; position resolves on a later unambiguous bar (or at data end with reason `EndOfData`) |
| `explicit` | user-specified priority list, e.g. `[take_profit, stop_loss]` |

The chosen policy and any deferrals are printed in the report and stored in
`experiment.json`.

## Cost stack (applied in this order, all adverse-side)

1. **Spread** (`costs.spread`): buy at `price + spread/2`, sell at `price − spread/2`.
   Models: `zero`, `fixed {spread}`.
2. **Slippage** (`costs.slippage`): `zero`, `fixed {value}` (price units),
   `percentage {bps}` of price.
3. **Commission** (`costs.commission`): `zero`, `fixed {per_order, per_unit?}`,
   `percentage {rate}` of notional.
4. **Financing** (`costs.financing`): `zero`, `daily_rate {long, short}` — applied
   per bar at bar close as `notional × daily_rate / bars_per_day` (long pays `long`
   rate, short pays `short` rate; negative rates pay the holder).

Cost attribution is decomposed per fill: `raw_price` (market trigger price),
`fill_price` (after spread+slippage), `spread_cost`, `slippage_cost`, `commission`.
Gross P&L uses raw prices; net P&L subtracts every component separately.
Nothing is ever combined silently.

## Order lifecycle

`Created → Accepted → Active → (PartiallyFilled →) Filled | Rejected | Cancelled | Expired`.
All transitions are events; history is immutable (orders.csv is append-only).
