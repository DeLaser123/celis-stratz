# Market playbook (researcher notes)

- EURUSD hourly data from Provider X; timestamps are bar **open** times in UTC.
- Volatility clusters: expect ATR spikes around London/NY overlap (13:00–17:00 UTC).
- Risk-off days: treat signals only when `risk_on == 1` from the macro feed
  (see `Trades/signals_macro.csv`; referenced by AI review, not by the v1 spec).
- Known data gaps: none in this sample.
