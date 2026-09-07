# Determinism Strategy

Hard requirement: same inputs ⇒ byte-identical outputs.

## Inputs that fully determine a run

data file bytes, strategy spec, config, starting capital, seed, engine version,
instrument configuration, execution model. All are hashed.

## Sources of nondeterminism and their elimination

| Source | Elimination |
|---|---|
| system clock / locale | no wall-clock reads in the engine; output timestamps derive from data; fixed formatting (`%Y-%m-%dT%H:%M:%SZ` UTC) |
| float iteration order | accounting/ledger in `Decimal`; stats on ordered arrays |
| hash-map ordering | `BTreeMap` everywhere state is iterated; strategies iterate explicit ordered lists |
| thread scheduling | single-run simulation is single-threaded; `rayon` only across *independent* runs (sweep); each run's inputs are cloned, never shared |
| nondeterministic RNG | only Monte Carlo randomizes; SplitMix64 with explicit seed; no other randomness exists in the engine |
| filesystem ordering | outputs written in fixed order; data explicitly sorted in normalization |
| undefined timestamp ordering | strict validation; permissive mode sorts (stable) with reported fixes |
| floating-point assoc. order | summations in fixed (chronological) order only |

## Experiment identity

```
experiment_id  = SHA-256(canonical_json{engine_version, data_hash, strategy_hash,
                                     config_hash, seed, starting_capital, instruments,
                                     execution})
result_hash    = SHA-256(canonical_json{trades, fills, equity_curve, metrics})
```

`canonical_json` = `serde_json::Value` serialization (BTreeMap ⇒ sorted keys; Decimals
as exact strings). Recorded in `experiment.json` together with engine version, all
hashes, seed, effective config **including every default that was applied** (§43:
no hidden assumptions — the effective config is complete and explicit).

## Reproduction procedure

`stratz run --data ... --strategy ... --config ... --out out2` then compare
`experiment.json.experiment_id` and `result_hash` (or `diff -r`). The determinism test
suite does exactly this in-process (run twice, compare hashes; shuffle input rows,
expect identical normalized-data hash and identical result hash).
