# Assumptions — strategy version v1

Compiled from `strategy.md`. Every assumption the compiler (AI or human)
made while mechanizing the description:

- "Trend rider" with no explicit take-profit: implemented as
  `take_profit: none` — exits ride the SMA cross.
- Risk sizing interpreted as stop-distance risk sizing
  (`percent_risk 1.0`), not notional allocation.
- No session filter requested: all hours traded.
- Stop anchored to the ATR value observed at decision time.
