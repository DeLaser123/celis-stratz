# Strategy: EURUSD Trend Rider (v1)

Long-only trend-following on EURUSD 1h bars.

## Entry
Buy when the 1h close crosses above the 20-bar SMA **and** the RSI(14) is
above 50.

## Exit
Close the position when the close crosses back below the 20-bar SMA.

## Risk
- Stop loss: 2 × ATR(10) below entry.
- Take profit: none (let the trend run; the SMA cross exits).
- Position sizing: risk 1% of equity per trade (stop-distance sizing).

## Sessions
Trade all hours — EURUSD trades nearly 24/5.
