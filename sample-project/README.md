# Stratz sample enterprise project

This folder IS the product demo: a complete, runnable Stratz harness project.
Copy it anywhere, then run the commands below (paths assume the built binary
is on PATH as `stratz`).

## Layout (the folder contract)

```
Strategy/
  v1/   eurusd_trend_rider   — strategy.md (human) + strategy.yaml (compiled) + config
  v2/   eurusd_trend_rider_fast — faster variant (10-bar SMA, hard TP)
Data/
  EURUSD_1h.csv              — 300 hourly bars, OHLCV
Trades/
  signals_macro.csv          — timestamped external signal (`risk_on`)
Notes/
  market_playbook.md         — researcher context for AI review/explain
scenarios/                   — stress scenario definitions
.stratz/                      — generated state (registry, results, cache, AI ledger)
```

The harness picks the LATEST strategy version automatically; override with
`--strategy-version v1`.

## Walkthrough

```bash
stratz doctor                          # verify the folder contract
stratz run                             # v1, artifacts + registry row
stratz run --strategy-version v2       # the faster variant
stratz registry list                   # both runs recorded
stratz compare .stratz/results/<v1-id> .stratz/results/<v2-id>
stratz sweep --grid sweep_grid.yaml    # parameter sweep (rayon)
stratz walk-forward --window 30d --oos 10d
stratz stress --scenario-dir scenarios # stress matrix
stratz robust --registry-tag trials    # deflated Sharpe + PBO (tag 2+ runs first)
stratz query registry "SELECT kind, label, return_pct FROM runs ORDER BY id"
stratz report .stratz/results/<id> --html
stratz ai compile --dry-run            # offline prompt preview (needs key to go live)
```

The AI commands require an API key: `--api-key`, the `STRATZ_API_KEY`
environment variable, or `stratz ai login --api-key <key>` (OS keyring).
`scripts/demo.sh` runs this whole walkthrough on a scratch copy.
