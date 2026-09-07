//! Ledger and trade records (spec §19). Every account mutation is traceable
//! to a ledger entry which references the event that caused it.

use crate::money::D;
use crate::time::Ts;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerEntryType {
    Trade,
    Dividend,
    Commission,
    SpreadCost,
    SlippageCost,
    Financing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerEntry {
    pub seq: u64,
    pub ts: Ts,
    pub entry_type: LedgerEntryType,
    /// Signed amount applied to balance (negative = cost).
    pub amount: D,
    pub balance_after: D,
    /// Human-readable reference into the event log (e.g. "order_id=12").
    pub reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradeRecord {
    pub trade_id: u64,
    pub symbol: String,
    /// "long" | "short"
    pub direction: String,
    pub entry_timestamp: Ts,
    pub entry_price: D,
    pub exit_timestamp: Ts,
    pub exit_price: D,
    pub quantity: D,
    pub gross_pnl: D,
    pub commission: D,
    pub spread_cost: D,
    pub slippage_cost: D,
    pub financing: D,
    /// Cash dividends received (long) or paid (short, negative) during the trade.
    pub dividends: D,
    pub net_pnl: D,
    pub initial_risk: Option<D>,
    pub r_multiple: Option<D>,
    pub holding_bars: u64,
    pub holding_time_secs: i64,
    /// Maximum adverse excursion (price distance against the trade).
    pub mae: D,
    /// Maximum favorable excursion (price distance with the trade).
    pub mfe: D,
    pub entry_reason: String,
    pub exit_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FillRecord {
    pub fill_id: u64,
    pub order_id: u64,
    pub ts: Ts,
    pub symbol: String,
    pub side: String,
    pub quantity: D,
    /// Market trigger price before costs.
    pub raw_price: D,
    /// Price after spread+slippage (what the account actually transacts at).
    pub fill_price: D,
    pub commission: D,
    pub spread_cost: D,
    pub slippage_cost: D,
    pub notional: D,
    pub reason: String,
}
