//! Margin account (spec §10). The ONLY place balance/positions mutate.
//!
//! Invariants:
//! - `equity = balance + unrealized` (by construction; asserted in tests);
//! - `balance = initial_capital + Σ ledger.amounts` (conservation; every
//!   balance change writes exactly one ledger entry);
//! - position quantities can only reach zero through an explicit closing fill;
//! - all arithmetic is checked; overflow is a typed error, never a wrap.

use crate::position::{FeeAccumulator, Position};
use bt_core::error::{CoreError, CoreResult};
use bt_core::instrument::Instrument;
use bt_core::ledger::{LedgerEntry, LedgerEntryType};
use bt_core::time::Ts;
use bt_core::{Side, D};
use rust_decimal_macros::dec;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct AccountSnapshot {
    pub balance: D,
    pub unrealized: D,
    pub equity: D,
    pub margin_used: D,
    pub free_margin: D,
}

/// Outcome of applying a fill (engine turns this into events).
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    Opened,
    Increased,
    Reduced { realized: D, fully_closed: bool },
    Closed { realized: D },
    Reversed { realized: D, new_qty: D },
}

pub struct Account {
    pub initial_capital: D,
    pub balance: D,
    pub leverage: D,
    pub positions: BTreeMap<String, Position>,
    pub ledger: Vec<LedgerEntry>,
    /// Totals for the report (exact attribution).
    pub total_commission: D,
    pub total_spread: D,
    pub total_slippage: D,
    pub total_financing: D,
    pub total_realized: D,
    seq: bt_core::event::Sequencer,
}

impl Account {
    pub fn new(initial_capital: D, leverage: D) -> Self {
        Account {
            initial_capital,
            balance: initial_capital,
            leverage,
            positions: BTreeMap::new(),
            ledger: Vec::new(),
            total_commission: dec!(0),
            total_spread: dec!(0),
            total_slippage: dec!(0),
            total_financing: dec!(0),
            total_realized: dec!(0),
            seq: bt_core::event::Sequencer::new(),
        }
    }

    fn push_entry(
        &mut self,
        ts: Ts,
        entry_type: LedgerEntryType,
        amount: D,
        reference: String,
    ) -> CoreResult<()> {
        let new_balance = self
            .balance
            .checked_add(amount)
            .ok_or(CoreError::DecimalOverflow {
                context: "balance".into(),
            })?;
        self.balance = new_balance;
        let seq = self.seq.next_seq();
        self.ledger.push(LedgerEntry {
            seq,
            ts,
            entry_type,
            amount,
            balance_after: new_balance,
            reference,
            // Accounting ledger entries are not AI calls.
            max_tokens: None,
        });
        Ok(())
    }

    pub fn unrealized(
        &self,
        marks: &BTreeMap<String, D>,
        instruments: &BTreeMap<String, Instrument>,
    ) -> CoreResult<D> {
        let mut u = dec!(0);
        for (symbol, pos) in &self.positions {
            let mark = marks
                .get(symbol)
                .ok_or_else(|| CoreError::UnknownSymbol(symbol.clone()))?;
            let cs = instruments
                .get(symbol)
                .map(|i| i.contract_size)
                .unwrap_or(dec!(1));
            u = bt_core::money::d_add(u, pos.unrealized(*mark, cs), "unrealized")?;
        }
        Ok(u)
    }

    pub fn snapshot(
        &self,
        marks: &BTreeMap<String, D>,
        instruments: &BTreeMap<String, Instrument>,
    ) -> CoreResult<AccountSnapshot> {
        let unrealized = self.unrealized(marks, instruments)?;
        let equity = bt_core::money::d_add(self.balance, unrealized, "equity")?;
        let margin_used = self.margin_used(marks, instruments)?;
        let free_margin = bt_core::money::d_sub(equity, margin_used, "free_margin")?;
        Ok(AccountSnapshot {
            balance: self.balance,
            unrealized,
            equity,
            margin_used,
            free_margin,
        })
    }

    pub fn margin_used(
        &self,
        marks: &BTreeMap<String, D>,
        instruments: &BTreeMap<String, Instrument>,
    ) -> CoreResult<D> {
        let mut m = dec!(0);
        for (symbol, pos) in &self.positions {
            let mark = marks
                .get(symbol)
                .ok_or_else(|| CoreError::UnknownSymbol(symbol.clone()))?;
            let cs = instruments
                .get(symbol)
                .map(|i| i.contract_size)
                .unwrap_or(dec!(1));
            let lev = instruments
                .get(symbol)
                .map(|i| i.leverage)
                .unwrap_or(dec!(1));
            let notional = pos.notional(*mark, cs);
            m = bt_core::money::d_add(
                m,
                bt_core::money::d_div(notional, lev, "margin")?,
                "margin_used",
            )?;
        }
        Ok(m)
    }

    /// Required margin for a prospective order (notional / leverage).
    pub fn required_margin(notional: D, leverage: D) -> CoreResult<D> {
        bt_core::money::d_div(notional, leverage, "required_margin")
    }

    /// Apply a fill to the netting position engine.
    ///
    /// `qty` is always positive; `side` gives direction. Costs (commission,
    /// spread, slippage) are booked to balance with individual ledger entries.
    /// Realized P&L is booked on reducing/closing fills.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_fill(
        &mut self,
        order_id: u64,
        symbol: &str,
        side: Side,
        qty: D,
        raw_price: D,
        _fill_price: D,
        commission: D,
        spread_cost: D,
        slippage_cost: D,
        contract_size: D,
        ts: Ts,
        direction_of_trade: &str, // "long"|"short" for a NEW position (side==Buy => long)
        initial_stop: Option<D>,
        stop: Option<D>,
        target: Option<D>,
        reason: &str,
    ) -> CoreResult<ApplyOutcome> {
        if qty <= dec!(0) {
            return Err(CoreError::InvalidOrder(format!(
                "fill qty must be > 0, got {qty}"
            )));
        }
        // P&L convention (ACCOUNTING.md): positions and realized P&L are
        // computed at RAW market prices; spread/slippage are booked as
        // separate cost entries so gross vs net attribution never double
        // counts. `fill_price` stays informational (the transacted price).
        let signed = match side {
            Side::Buy => qty,
            Side::Sell => -qty,
        };
        let reference = format!("order_id={order_id}");

        // Book costs first (they always reduce balance).
        if commission != dec!(0) {
            self.total_commission += commission;
            self.push_entry(
                ts,
                LedgerEntryType::Commission,
                -commission,
                reference.clone(),
            )?;
        }
        if spread_cost != dec!(0) {
            self.total_spread += spread_cost;
            self.push_entry(
                ts,
                LedgerEntryType::SpreadCost,
                -spread_cost,
                reference.clone(),
            )?;
        }
        if slippage_cost != dec!(0) {
            self.total_slippage += slippage_cost;
            self.push_entry(
                ts,
                LedgerEntryType::SlippageCost,
                -slippage_cost,
                reference.clone(),
            )?;
        }

        let existing = self.positions.get(symbol);
        let outcome = match existing {
            None => {
                let pos = Position {
                    symbol: symbol.to_string(),
                    qty: signed,
                    avg_entry: raw_price,
                    open_ts: ts,
                    stop,
                    target,
                    trailing_stop: None,
                    direction: direction_of_trade.to_string(),
                    initial_stop,
                    initial_risk: initial_stop.map(|s| (raw_price - s).abs() * qty * contract_size),
                    entries: 1,
                    entry_reason: reason.to_string(),
                    entry_fees: {
                        let mut f = FeeAccumulator::default();
                        f.add(commission, spread_cost, slippage_cost);
                        f
                    },
                    financing_acc: dec!(0),
                    mae: dec!(0),
                    mfe: dec!(0),
                    bars_held: 0,
                    entry_order_ids: vec![order_id],
                };
                self.positions.insert(symbol.to_string(), pos);
                ApplyOutcome::Opened
            }
            Some(pos) => {
                let pos_qty = pos.qty;
                if pos_qty == dec!(0) {
                    return Err(CoreError::AccountingInvariantViolation(
                        "position with zero quantity must be removed".into(),
                    ));
                }
                let new_qty = bt_core::money::d_add(pos_qty, signed, "position qty")?;
                let fill_with_position = (pos_qty > dec!(0) && signed > dec!(0))
                    || (pos_qty < dec!(0) && signed < dec!(0));
                if fill_with_position {
                    // Increasing: weighted average entry, no P&L (no double count).
                    let total_abs = pos_qty.abs() + signed.abs();
                    let new_avg =
                        (pos_qty.abs() * pos.avg_entry + signed.abs() * raw_price) / total_abs;
                    let pos = self.positions.get_mut(symbol).unwrap();
                    pos.qty = new_qty;
                    pos.avg_entry = new_avg;
                    pos.entries += 1;
                    pos.entry_fees.add(commission, spread_cost, slippage_cost);
                    pos.entry_order_ids.push(order_id);
                    if pos.initial_stop.is_none() {
                        pos.initial_stop = initial_stop;
                        pos.initial_risk =
                            initial_stop.map(|s| (raw_price - s).abs() * qty * contract_size);
                    }
                    ApplyOutcome::Increased
                } else {
                    // Fill opposes the position: reduce | close | reverse.
                    let closed = signed.abs().min(pos_qty.abs());
                    let realized = if pos_qty > dec!(0) {
                        (raw_price - pos.avg_entry) * closed * contract_size
                    } else {
                        (pos.avg_entry - raw_price) * closed * contract_size
                    };
                    self.total_realized += realized;
                    self.push_entry(ts, LedgerEntryType::Trade, realized, reference.clone())?;
                    if new_qty == dec!(0) {
                        self.positions.remove(symbol);
                        ApplyOutcome::Closed { realized }
                    } else if signed.abs() < pos_qty.abs() {
                        // Partial reduction (did not cross zero): avg entry unchanged.
                        let pos = self.positions.get_mut(symbol).unwrap();
                        pos.qty = new_qty;
                        ApplyOutcome::Reduced {
                            realized,
                            fully_closed: false,
                        }
                    } else {
                        // Reversal: position flipped through zero.
                        let remainder = new_qty.abs();
                        let pos = Position {
                            symbol: symbol.to_string(),
                            qty: if new_qty > dec!(0) {
                                remainder
                            } else {
                                -remainder
                            },
                            avg_entry: raw_price,
                            open_ts: ts,
                            stop,
                            target,
                            trailing_stop: None,
                            direction: direction_of_trade.to_string(),
                            initial_stop,
                            initial_risk: initial_stop
                                .map(|s| (raw_price - s).abs() * remainder * contract_size),
                            entries: 1,
                            entry_reason: reason.to_string(),
                            entry_fees: {
                                let mut f = FeeAccumulator::default();
                                f.add(commission, spread_cost, slippage_cost);
                                f
                            },
                            financing_acc: dec!(0),
                            mae: dec!(0),
                            mfe: dec!(0),
                            bars_held: 0,
                            entry_order_ids: vec![order_id],
                        };
                        self.positions.insert(symbol.to_string(), pos);
                        ApplyOutcome::Reversed { realized, new_qty }
                    }
                }
            }
        };

        // Invariant: balance equals initial + ledger sum (self-check).
        //
        // Decimal keeps 28 SIGNIFICANT digits, so accumulating the balance
        // truncates ~1e-22 of dust per addition relative to the exact ledger
        // sum. Conservation is therefore asserted at a 1e-12 tolerance, which
        // is many orders of magnitude below any real accounting error (fees,
        // double-charges etc. are >= cent-scale) while remaining deterministic.
        let ledger_sum: D = self.ledger.iter().map(|e| e.amount).sum();
        let diff = (self.balance - self.initial_capital - ledger_sum).abs();
        if diff > D::new(1, 12) {
            return Err(CoreError::AccountingInvariantViolation(format!(
                "ledger conservation violated: balance={} initial={} ledger_sum={} diff={}",
                self.balance, self.initial_capital, ledger_sum, diff
            )));
        }
        Ok(outcome)
    }

    /// Apply a cash dividend on the ex-date bar for `symbol`.
    /// Long positions receive `amount x qty x contract_size`;
    /// short positions pay the same on |qty|. Returns the cash applied.
    pub fn apply_dividend(
        &mut self,
        symbol: &str,
        amount: D,
        contract_size: D,
        ts: Ts,
    ) -> CoreResult<D> {
        use rust_decimal_macros::dec as dm;
        let Some(pos) = self.positions.get(symbol) else {
            return Ok(dm!(0));
        };
        // signed cash: long (+qty) receives, short (|qty|) pays
        let cash = amount * pos.qty * contract_size;
        if cash == dm!(0) {
            return Ok(dm!(0));
        }
        let reference = format!("dividend symbol={symbol} per_unit={amount}");
        self.push_entry(ts, LedgerEntryType::Dividend, cash, reference)?;
        Ok(cash)
    }

    /// Adjust an open position for a split (new units per old unit). Prices
    /// divide by the ratio, quantities multiply; economics are unchanged.
    /// Protective price levels and the trade builder are adjusted by the
    /// engine (it owns them).
    pub fn apply_split(&mut self, symbol: &str, ratio: D) -> CoreResult<bool> {
        if ratio <= dec!(0) {
            return Err(CoreError::InvalidData(format!(
                "split ratio must be positive, got {ratio}"
            )));
        }
        let Some(pos) = self.positions.get_mut(symbol) else {
            return Ok(false);
        };
        pos.qty *= ratio;
        pos.avg_entry /= ratio;
        if let Some(s) = pos.stop.as_mut() {
            *s /= ratio;
        }
        if let Some(t) = pos.target.as_mut() {
            *t /= ratio;
        }
        if let Some(t) = pos.trailing_stop.as_mut() {
            *t /= ratio;
        }
        if let Some(s) = pos.initial_stop.as_mut() {
            *s /= ratio;
        }
        // initial_risk = |entry - stop| x qty x cs is invariant under a split
        // (prices divide, quantity multiplies) — left untouched.
        Ok(true)
    }

    /// Apply per-bar financing to one position. `amount` is signed from the
    /// holder's perspective (positive = cost).
    pub fn apply_financing(&mut self, symbol: &str, amount: D, ts: Ts) -> CoreResult<bool> {
        if amount == dec!(0) {
            return Ok(false);
        }
        let reference = format!("position={symbol}");
        self.total_financing += amount;
        self.push_entry(ts, LedgerEntryType::Financing, -amount, reference)?;
        if let Some(pos) = self.positions.get_mut(symbol) {
            pos.financing_acc += amount;
        }
        Ok(true)
    }

    /// Ledger seq peek (engine shares sequencing with the event log by
    /// convention: ledger has its own monotonic sequence).
    pub fn ledger_len(&self) -> usize {
        self.ledger.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_core::time::parse_timestamp;

    fn ts(s: &str) -> Ts {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        parse_timestamp(s, utc, "t").unwrap()
    }

    fn acc() -> Account {
        Account::new(dec!(100000), dec!(1))
    }

    #[test]
    fn buy_sell_roundtrip_exact_pnl() {
        // Spec test 1: buy 1 @100, sell @110 => gross 10, equity 100010.
        let mut a = acc();
        let o1 = a
            .apply_fill(
                1,
                "X",
                Side::Buy,
                dec!(1),
                dec!(100),
                dec!(100),
                dec!(0),
                dec!(0),
                dec!(0),
                dec!(1),
                ts("2024-01-01T00:00:00Z"),
                "long",
                None,
                None,
                None,
                "entry",
            )
            .unwrap();
        assert!(matches!(o1, ApplyOutcome::Opened));
        let o2 = a
            .apply_fill(
                2,
                "X",
                Side::Sell,
                dec!(1),
                dec!(110),
                dec!(110),
                dec!(0),
                dec!(0),
                dec!(0),
                dec!(1),
                ts("2024-01-02T00:00:00Z"),
                "long",
                None,
                None,
                None,
                "exit",
            )
            .unwrap();
        assert!(matches!(o2, ApplyOutcome::Closed { realized } if realized == dec!(10)));
        assert_eq!(a.balance, dec!(100010));
        assert!(a.positions.is_empty());
    }

    #[test]
    fn short_roundtrip_positive_pnl() {
        // Spec test 3: short @100, cover @90 => +10.
        let mut a = acc();
        a.apply_fill(
            1,
            "X",
            Side::Sell,
            dec!(1),
            dec!(100),
            dec!(100),
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "short",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        let o = a
            .apply_fill(
                2,
                "X",
                Side::Buy,
                dec!(1),
                dec!(90),
                dec!(90),
                dec!(0),
                dec!(0),
                dec!(0),
                dec!(1),
                ts("2024-01-02T00:00:00Z"),
                "short",
                None,
                None,
                None,
                "exit",
            )
            .unwrap();
        assert!(matches!(o, ApplyOutcome::Closed { realized } if realized == dec!(10)));
        assert_eq!(a.balance, dec!(100010));
    }

    #[test]
    fn costs_are_booked_and_attributed() {
        // Spec tests 4-6: commission/spread/slippage reduce balance and are tracked.
        let mut a = acc();
        a.apply_fill(
            1,
            "X",
            Side::Buy,
            dec!(10),
            dec!(100),
            dec!(100.5),
            dec!(2),
            dec!(0.3),
            dec!(0.2),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        assert_eq!(a.total_commission, dec!(2));
        assert_eq!(a.total_spread, dec!(0.3));
        assert_eq!(a.total_slippage, dec!(0.2));
        // balance reduced by total costs
        assert_eq!(a.balance, dec!(100000) - dec!(2.5));
        let entries: Vec<_> = a.ledger.iter().map(|e| e.entry_type).collect();
        assert!(entries.contains(&LedgerEntryType::Commission));
        assert!(entries.contains(&LedgerEntryType::SpreadCost));
        assert!(entries.contains(&LedgerEntryType::SlippageCost));
    }

    #[test]
    fn partial_exit_keeps_avg_entry() {
        let mut a = acc();
        a.apply_fill(
            1,
            "X",
            Side::Buy,
            dec!(10),
            dec!(100),
            dec!(100),
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        let o = a
            .apply_fill(
                2,
                "X",
                Side::Sell,
                dec!(4),
                dec!(110),
                dec!(110),
                dec!(0),
                dec!(0),
                dec!(0),
                dec!(1),
                ts("2024-01-02T00:00:00Z"),
                "long",
                None,
                None,
                None,
                "exit",
            )
            .unwrap();
        match o {
            ApplyOutcome::Reduced {
                realized,
                fully_closed,
            } => {
                assert_eq!(realized, dec!(40));
                assert!(!fully_closed);
            }
            other => panic!("expected reduced, got {other:?}"),
        }
        let p = &a.positions["X"];
        assert_eq!(p.qty, dec!(6));
        assert_eq!(p.avg_entry, dec!(100));
        assert_eq!(a.balance, dec!(100000) + dec!(40));
    }

    #[test]
    fn reversal_resets_entry() {
        let mut a = acc();
        a.apply_fill(
            1,
            "X",
            Side::Buy,
            dec!(10),
            dec!(100),
            dec!(100),
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        let o = a
            .apply_fill(
                2,
                "X",
                Side::Sell,
                dec!(15),
                dec!(110),
                dec!(110),
                dec!(0),
                dec!(0),
                dec!(0),
                dec!(1),
                ts("2024-01-02T00:00:00Z"),
                "short",
                Some(dec!(115)),
                None,
                None,
                "entry",
            )
            .unwrap();
        match o {
            ApplyOutcome::Reversed { realized, new_qty } => {
                assert_eq!(realized, dec!(100)); // 10 * (110-100)
                assert_eq!(new_qty, dec!(-5));
            }
            other => panic!("expected reversed, got {other:?}"),
        }
        let p = &a.positions["X"];
        assert_eq!(p.qty, dec!(-5));
        assert_eq!(p.avg_entry, dec!(110));
        assert_eq!(p.direction, "short");
        assert_eq!(p.initial_stop, Some(dec!(115)));
        assert_eq!(p.initial_risk, Some(dec!(25))); // |110-115| * 5
    }

    #[test]
    fn averaging_increases_entries() {
        let mut a = acc();
        a.apply_fill(
            1,
            "X",
            Side::Buy,
            dec!(10),
            dec!(100),
            dec!(100),
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        let o = a
            .apply_fill(
                2,
                "X",
                Side::Buy,
                dec!(10),
                dec!(110),
                dec!(110),
                dec!(0),
                dec!(0),
                dec!(0),
                dec!(1),
                ts("2024-01-02T00:00:00Z"),
                "long",
                None,
                None,
                None,
                "entry",
            )
            .unwrap();
        assert!(matches!(o, ApplyOutcome::Increased));
        let p = &a.positions["X"];
        assert_eq!(p.qty, dec!(20));
        assert_eq!(p.avg_entry, dec!(105));
        assert_eq!(a.balance, dec!(100000)); // no P&L from adding
    }

    #[test]
    fn ledger_conservation_holds() {
        let mut a = acc();
        a.apply_fill(
            1,
            "X",
            Side::Buy,
            dec!(10),
            dec!(100),
            dec!(100.5),
            dec!(2),
            dec!(0.3),
            dec!(0.2),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        a.apply_financing("X", dec!(1), ts("2024-01-02T00:00:00Z"))
            .unwrap();
        a.apply_fill(
            3,
            "X",
            Side::Sell,
            dec!(10),
            dec!(110),
            dec!(109.5),
            dec!(2),
            dec!(0.3),
            dec!(0.2),
            dec!(1),
            ts("2024-01-03T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "exit",
        )
        .unwrap();
        let ledger_sum: D = a.ledger.iter().map(|e| e.amount).sum();
        assert!((a.balance - a.initial_capital - ledger_sum).abs() <= D::new(1, 12));
    }

    #[test]
    fn unrealized_and_margin() {
        let mut a = acc();
        let mut instruments = BTreeMap::new();
        let mut inst = Instrument::new("X");
        inst.leverage = dec!(10);
        instruments.insert("X".to_string(), inst);
        a.apply_fill(
            1,
            "X",
            Side::Buy,
            dec!(10),
            dec!(100),
            dec!(100),
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(1),
            ts("2024-01-01T00:00:00Z"),
            "long",
            None,
            None,
            None,
            "entry",
        )
        .unwrap();
        let mut marks = BTreeMap::new();
        marks.insert("X".to_string(), dec!(105));
        let snap = a.snapshot(&marks, &instruments).unwrap();
        assert_eq!(snap.unrealized, dec!(50));
        assert_eq!(snap.equity, dec!(100050));
        assert_eq!(snap.margin_used, dec!(1050) / dec!(10));
        assert_eq!(snap.free_margin, snap.equity - snap.margin_used);
    }
}
