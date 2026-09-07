//! Open position state (netting model). One position per symbol with signed
//! quantity, weighted-average entry, protective levels, excursion tracking
//! and accumulated entry-side costs for exact trade attribution.

use bt_core::time::Ts;
use bt_core::D;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeAccumulator {
    pub commission: D,
    pub spread_cost: D,
    pub slippage_cost: D,
}

impl FeeAccumulator {
    pub fn add(&mut self, commission: D, spread: D, slippage: D) {
        self.commission += commission;
        self.spread_cost += spread;
        self.slippage_cost += slippage;
    }
    pub fn total(&self) -> D {
        self.commission + self.spread_cost + self.slippage_cost
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    /// Signed: > 0 long, < 0 short, 0 = flat (never stored).
    pub qty: D,
    pub avg_entry: D,
    pub open_ts: Ts,
    /// Fixed stop level (may be None if strategy defined no stop).
    pub stop: Option<D>,
    pub target: Option<D>,
    /// Trailing stop level (updated at bar close by the engine).
    pub trailing_stop: Option<D>,
    /// Direction of the first entry of the current trade ("long"/"short").
    pub direction: String,
    /// Stop level of the FIRST entry — anchors the initial risk (1R).
    pub initial_stop: Option<D>,
    /// |entry − initial_stop| × qty_at_first_entry × contract_size.
    pub initial_risk: Option<D>,
    pub entries: u32,
    pub entry_reason: String,
    pub entry_fees: FeeAccumulator,
    pub financing_acc: D,
    /// Maximum adverse / favorable excursion, as positive price distances.
    pub mae: D,
    pub mfe: D,
    /// Bars elapsed since first entry (incremented once per base bar).
    pub bars_held: u64,
    /// Order ids that built this position (audit).
    pub entry_order_ids: Vec<u64>,
}

impl Position {
    pub fn is_long(&self) -> bool {
        self.qty > dec!(0)
    }

    /// Unrealized P&L at `mark` in quote currency (sign-correct for shorts).
    pub fn unrealized(&self, mark: D, contract_size: D) -> D {
        (mark - self.avg_entry) * self.qty * contract_size
    }

    /// Notional at `mark` (absolute).
    pub fn notional(&self, mark: D, contract_size: D) -> D {
        self.qty.abs() * mark * contract_size
    }

    /// Update MAE/MFE with one bar's extremes (post-fill convention: the
    /// entry bar's full range is included — documented, conservative).
    pub fn update_excursions(&mut self, high: D, low: D) {
        if self.is_long() {
            let fav = high - self.avg_entry;
            let adv = self.avg_entry - low;
            if fav > self.mfe {
                self.mfe = fav;
            }
            if adv > self.mae {
                self.mae = adv;
            }
        } else {
            let fav = self.avg_entry - low;
            let adv = high - self.avg_entry;
            if fav > self.mfe {
                self.mfe = fav;
            }
            if adv > self.mae {
                self.mae = adv;
            }
        }
    }
}
