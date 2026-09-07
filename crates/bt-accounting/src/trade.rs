//! Trade ledger construction: aggregates one flat→flat round trip into a
//! `TradeRecord` (spec §19). The engine drives it from `ApplyOutcome`s.

use bt_core::error::CoreError;
use bt_core::ledger::TradeRecord;
use bt_core::time::Ts;
use bt_core::D;
use rust_decimal_macros::dec;

pub struct TradeBuilder {
    pub trade_id: u64,
    pub symbol: String,
    pub direction: String,
    pub entry_ts: Ts,
    pub entry_price: D,
    pub exit_ts: Option<Ts>,
    pub exit_price: Option<D>,
    /// Total quantity closed across all exit fills (== total entered on finish).
    pub closed_qty: D,
    /// Peak absolute position size held during the trade.
    pub peak_qty: D,
    pub gross_pnl: D,
    pub commission: D,
    pub spread_cost: D,
    pub slippage_cost: D,
    pub financing: D,
    pub dividends: D,
    pub initial_risk: Option<D>,
    pub holding_bars: u64,
    pub mae: D,
    pub mfe: D,
    pub entry_reason: String,
}

impl TradeBuilder {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        trade_id: u64,
        symbol: &str,
        direction: &str,
        ts: Ts,
        entry_price: D,
        qty: D,
        initial_risk: Option<D>,
        entry_reason: &str,
        entry_commission: D,
        entry_spread: D,
        entry_slippage: D,
    ) -> Self {
        TradeBuilder {
            trade_id,
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_ts: ts,
            entry_price,
            exit_ts: None,
            exit_price: None,
            closed_qty: dec!(0),
            peak_qty: qty,
            gross_pnl: dec!(0),
            commission: entry_commission,
            spread_cost: entry_spread,
            slippage_cost: entry_slippage,
            financing: dec!(0),
            dividends: dec!(0),
            initial_risk,
            holding_bars: 0,
            mae: dec!(0),
            mfe: dec!(0),
            entry_reason: entry_reason.to_string(),
        }
    }

    /// Record a partial or full closing fill.
    pub fn record_exit(&mut self, ts: Ts, price: D, qty: D, realized_gross: D) {
        self.exit_ts = Some(ts);
        self.exit_price = Some(price); // final closing fill price (documented)
        self.closed_qty += qty;
        self.gross_pnl += realized_gross;
    }

    pub fn add_exit_fees(&mut self, commission: D, spread: D, slippage: D) {
        self.commission += commission;
        self.spread_cost += spread;
        self.slippage_cost += slippage;
    }

    pub fn add_financing(&mut self, amount: D) {
        self.financing += amount;
    }

    pub fn add_dividend(&mut self, amount: D) {
        self.dividends += amount;
    }

    /// Adjust an open trade for a split: entry price divides, quantities
    /// multiply, price-distance excursions divide. Economics unchanged.
    pub fn apply_split(&mut self, ratio: D) {
        self.entry_price /= ratio;
        self.closed_qty *= ratio;
        self.peak_qty *= ratio;
        self.mae /= ratio;
        self.mfe /= ratio;
    }

    pub fn observe_excursions(&mut self, mae: D, mfe: D, bars_held: u64) {
        if mae > self.mae {
            self.mae = mae;
        }
        if mfe > self.mfe {
            self.mfe = mfe;
        }
        self.holding_bars = bars_held;
    }

    pub fn grow_peak(&mut self, qty: D) {
        if qty > self.peak_qty {
            self.peak_qty = qty;
        }
    }

    /// Finish the trade. `r_convention_net` selects the P&L used for R.
    pub fn finish(
        self,
        exit_reason: &str,
        r_convention_net: bool,
    ) -> Result<TradeRecord, CoreError> {
        let (exit_ts, exit_price) = match (self.exit_ts, self.exit_price) {
            (Some(t), Some(p)) => (t, p),
            _ => {
                return Err(CoreError::AccountingInvariantViolation(format!(
                    "trade {} finished without an exit fill",
                    self.trade_id
                )))
            }
        };
        let net_pnl = self.gross_pnl
            - self.commission
            - self.spread_cost
            - self.slippage_cost
            - self.financing
            + self.dividends;
        let r_multiple = self.initial_risk.filter(|r| *r != dec!(0)).map(|r| {
            let pnl = if r_convention_net {
                net_pnl
            } else {
                self.gross_pnl
            };
            pnl / r
        });
        Ok(TradeRecord {
            trade_id: self.trade_id,
            symbol: self.symbol,
            direction: self.direction,
            entry_timestamp: self.entry_ts,
            entry_price: self.entry_price,
            exit_timestamp: exit_ts,
            exit_price,
            quantity: self.closed_qty,
            gross_pnl: self.gross_pnl,
            commission: self.commission,
            spread_cost: self.spread_cost,
            slippage_cost: self.slippage_cost,
            financing: self.financing,
            dividends: self.dividends,
            net_pnl,
            initial_risk: self.initial_risk,
            r_multiple,
            holding_bars: self.holding_bars,
            holding_time_secs: (exit_ts - self.entry_ts).num_seconds(),
            mae: self.mae,
            mfe: self.mfe,
            entry_reason: self.entry_reason,
            exit_reason: exit_reason.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn ts(s: &str) -> Ts {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        bt_core::time::parse_timestamp(s, utc, "t").unwrap()
    }

    #[test]
    fn r_multiple_from_initial_risk() {
        // Spec test 2: long entry 100, stop 95, qty 2 => initial risk 10.
        // Exit at 110 => gross 20 => R = 2.0 (net, zero costs).
        let mut b = TradeBuilder::start(
            1,
            "X",
            "long",
            ts("2024-01-01T00:00:00Z"),
            dec!(100),
            dec!(2),
            Some(dec!(10)),
            "entry",
            dec!(0),
            dec!(0),
            dec!(0),
        );
        b.record_exit(ts("2024-01-02T00:00:00Z"), dec!(110), dec!(2), dec!(20));
        // Observed: low touched 95 (MAE = 100-95), high touched 110 (MFE = 110-100).
        b.observe_excursions(dec!(5), dec!(10), 24);
        let t = b.finish("take_profit", true).unwrap();
        assert_eq!(t.net_pnl, dec!(20));
        assert_eq!(t.r_multiple, Some(dec!(2)));
        assert_eq!(t.mfe, dec!(10));
        assert_eq!(t.mae, dec!(5));
        assert_eq!(t.holding_bars, 24);
        assert_eq!(t.holding_time_secs, 86_400);
    }

    #[test]
    fn costs_reduce_net() {
        let mut b = TradeBuilder::start(
            1,
            "X",
            "long",
            ts("2024-01-01T00:00:00Z"),
            dec!(100),
            dec!(2),
            None,
            "entry",
            dec!(1),
            dec!(0.5),
            dec!(0.5),
        );
        b.record_exit(ts("2024-01-02T00:00:00Z"), dec!(110), dec!(2), dec!(20));
        b.add_exit_fees(dec!(1), dec!(0.5), dec!(0.5));
        b.add_financing(dec!(0.25));
        let t = b.finish("stop_loss", true).unwrap();
        assert_eq!(t.gross_pnl, dec!(20));
        assert_eq!(t.net_pnl, dec!(20) - dec!(4) - dec!(0.25));
        assert_eq!(t.r_multiple, None, "no stop configured => no R");
    }
}
