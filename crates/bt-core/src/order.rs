//! Order model and lifecycle (spec §8). History is never mutated: transitions
//! are recorded as events; `orders.csv` carries the final state of each order.

use crate::money::D;
use crate::time::Ts;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
    Stop,
    StopLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Created,
    Accepted,
    Active,
    PartiallyFilled,
    Filled,
    Rejected,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderReason {
    StrategyEntry,
    StrategyExit,
    StopLoss,
    TakeProfit,
    TrailingStop,
    EndOfData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionEffect {
    Open,
    Close,
    Reverse,
}

/// A full order record. Quantities are positive; `side` carries direction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Order {
    pub order_id: u64,
    pub strategy_id: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: D,
    pub limit_price: Option<D>,
    pub stop_price: Option<D>,
    pub creation_ts: Ts,
    pub activation_ts: Option<Ts>,
    pub expiration_ts: Option<Ts>,
    pub status: OrderStatus,
    pub filled_qty: D,
    pub remaining_qty: D,
    pub avg_fill_price: Option<D>,
    pub commission: D,
    pub slippage: D,
    pub parent_order_id: Option<u64>,
    pub position_effect: PositionEffect,
    pub reason: OrderReason,
}

impl Order {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        order_id: u64,
        strategy_id: &str,
        symbol: &str,
        side: Side,
        order_type: OrderType,
        quantity: D,
        limit_price: Option<D>,
        stop_price: Option<D>,
        creation_ts: Ts,
        reason: OrderReason,
        position_effect: PositionEffect,
        parent_order_id: Option<u64>,
    ) -> Self {
        Order {
            order_id,
            strategy_id: strategy_id.to_string(),
            symbol: symbol.to_string(),
            side,
            order_type,
            quantity,
            limit_price,
            stop_price,
            creation_ts,
            activation_ts: None,
            expiration_ts: None,
            status: OrderStatus::Created,
            filled_qty: rust_decimal_macros::dec!(0),
            remaining_qty: quantity,
            avg_fill_price: None,
            commission: rust_decimal_macros::dec!(0),
            slippage: rust_decimal_macros::dec!(0),
            parent_order_id,
            position_effect,
            reason,
        }
    }
}

/// Monotonic id generator (deterministic: ids depend only on event order).
#[derive(Debug, Clone)]
pub struct IdGenerator {
    next: u64,
}

impl IdGenerator {
    pub fn new() -> Self {
        IdGenerator { next: 1 }
    }
    pub fn next_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_increase() {
        let mut g = IdGenerator::new();
        assert_eq!(g.next_id(), 1);
        assert_eq!(g.next_id(), 2);
    }
}
