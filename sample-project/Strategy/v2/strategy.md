# Strategy: EURUSD Trend Rider (v2 — faster)

v2 speeds the trend up: a 10-bar SMA instead of 20, with a hard take-profit
at 2x the stop distance so winners are banked.

## Entry
Buy when the 1h close crosses above the 10-bar SMA.

## Exit
Close when the close crosses back below the 10-bar SMA, or via the
ATR(10) x 2 stop loss, or the 2R take profit.

## Risk
- Sizing: risk 1% of equity per trade.

Everything else (sessions, costs) follows the v1 config.
