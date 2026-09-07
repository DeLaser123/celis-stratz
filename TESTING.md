# Testing Architecture

Hard requirement: the engine is complete when the tests say so, not when it compiles.

## Layers

1. **Unit tests** (per crate, inline `#[cfg(test)]`): every financial formula has a
   reference test with a hand-computed expected value.
2. **Integration tests** (`bt-simulation/tests/`): end-to-end runs over synthetic CSVs
   where the correct answer is known exactly.
3. **Property tests** (`proptest`): accounting invariants over randomized price/qty
   sequences (bounded, seeded — property tests are themselves deterministic).
4. **Golden tests** (`tests/golden/`): dataset + strategy + config + expected outputs
   (trades/equity/metrics) + hashes. A golden failure = behavioral change.
5. **Determinism tests**: run twice ⇒ identical `result_hash`; shuffled input ⇒
   identical normalized data hash and result hash.
6. **Look-ahead tests**: (a) trades completed within a data prefix are identical whether
   later bars exist or not; (b) HTF values invisible before HTF close; (c) external
   signals with later timestamps never affect earlier decisions.
7. **Reference-vs-optimized (differential)**: indicator precompute path vs streaming
   path must produce identical runs (`indicator_precompute: true|false`), exercised
   automatically in every simulation integration test.

## Synthetic fixtures (spec §31 mapping)

| # | Fixture | Asserts |
|---|---|---|
| 1 | buy@100 → sell@110, 0 costs | gross P&L = 10, equity = 100,010 |
| 2 | long@100, SL 95, TP 110 | R computed from initial risk 5×qty |
| 3 | short@100 → cover@90 | positive P&L |
| 4–6 | cost models | commission/spread/slippage charged and attributed separately |
| 7 | one bar touches SL 95 & TP 110 | each policy resolves exactly as specified |
| 8 | same inputs twice | identical result hashes |
| 9 | shuffled rows | deterministic normalized result |
| 10 | prefix vs full data | completed trades identical |
| 11 | 1h base + 1d HTF | daily close invisible before 00:00 UTC next day |
| 12 | partial exit | avg entry unchanged, realized P&L proportional |
| 13 | reversal | old position realized, new opens with fresh avg/stop |
| 14 | two symbols | independent positions, combined equity |
| 15 | percent_risk sizing | qty = floor(risk/(distance×cs), qty_step) |
| 16 | hand-built equity path | max DD exact |
| 17 | hand-built returns | Sharpe/Sortino vs manual reference |

## Gates (acceptance criteria)

`cargo build --release`, `cargo test`, `cargo fmt --check`,
`cargo clippy --all-targets --all-features -- -D warnings`, golden + determinism +
differential suites all pass. CI-able as a single script: `cargo fmt --check && cargo
clippy --all-targets --all-features -- -D warnings && cargo test --release`.
