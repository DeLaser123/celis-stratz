# Accounting Model

## Account (margin/CFD style — the general case)

```
balance      : realized equity (Decimal, full precision internally)
unrealized   : Σ position unrealized P&L (marked at current bar close)
equity       : balance + unrealized
margin_used  : Σ |qty × price × contract_size| / leverage   (per position, at entry notional? → marked at current price)
free_margin  : equity − margin_used
```

Invariant (checked continuously, panic-free typed error on violation):
`equity = balance + unrealized`. Property tests enforce it.

## Position engine (netting per symbol)

- One net position per symbol: signed quantity, weighted-average entry price.
- **Increase**: new avg entry = weighted mean; P&L unchanged (no double counting).
- **Reduce**: realizes `qty_closed × (mark − avg_entry) × contract_size` (sign per side);
  avg entry unchanged.
- **Reverse** (crossing zero): close the old qty fully (realize), open remainder as new
  position with fresh avg entry/stop/target.
- Flat = quantity zero → position removed; a `TradeRecord` is emitted
  (flat→flat round trip, aggregating all entries/exits of the trip).

## P&L formulas (quote currency == account currency; see LIMITATIONS)

```
long  pnl = (exit − entry) × qty × contract_size
short pnl = (entry − exit) × qty × contract_size
notional  = |qty| × price × contract_size
```

**Raw-price convention (important):** positions store the RAW market price as
average entry, and realized/unrealized P&L is computed against raw market
prices (the account is marked at bar closes). Spread and slippage are NOT
embedded in P&L — they are booked as separate negative ledger entries. This
makes `gross P&L − commission − spread − slippage − financing = net P&L` exact
and prevents double counting. `fill_price` (raw ± half-spread ± slippage) is
the transacted price recorded in fills.csv; stop/target levels anchor to the
raw entry price so risk distance is cost-free.

**Decimal precision note:** `rust_decimal` keeps 28 significant digits, so
accumulating the balance truncates ~1e-22 of dust per addition relative to the
exact ledger sum. The conservation invariant
`balance = initial_capital + Σ ledger.amounts` is asserted at a 1e-12
tolerance (many orders of magnitude below any real accounting error, and
deterministic). The ledger remains the authoritative replay source.

## Money rules

- All accounting in `Decimal` (checked arithmetic; overflow → `AccountingInvariantViolation`).
- **No implicit rounding**: internal values keep full Decimal precision.
- Explicit rounding only at: (a) output formatting (`rounding.money_dp`, default 2),
  (b) quantity sizing floors to `instrument.qty_step` (conservative, never rounds up risk),
  (c) price levels to `instrument.tick_size` when configured.
- **No event creates money from nothing**: every balance change is a `LedgerEntry`
  referencing an event id; fees/slippage/financing reduce balance and are attributed.

## Ledger

`LedgerEntry { ts, seq, entry_type: Trade | Commission | SpreadCost | SlippageCost |
Financing | Adjustment, amount, balance_after, reference }`.
Answering "why did equity change at T?" = replay ledger entries with `reference`
→ event log entries. The audit trail is complete by construction: fills and financing
are the *only* mutation sources.

## Margin handling

Pre-trade: required margin must fit free margin, else `OrderRejected(MarginViolation)`.
In-trade: if `free_margin < 0` an `AccountUpdated(margin_violation=true)` event is
emitted each bar (no auto-liquidation in v0.1 — documented in LIMITATIONS).

## End-of-data policy (`runtime.end_policy`)

`close_all` (default): open positions are closed at the final bar's close with reason
`EndOfData` (marked in trade ledger). `mark_to_market`: left open, unrealized included
in final equity (final equity identical, but last trade not realized).
