//! Fill construction: turns a raw market trigger price into the price the
//! account actually transacts at, with fully attributed costs.

use crate::costs::CostModels;
use bt_core::error::{CoreError, CoreResult};
use bt_core::{Side, D};

#[derive(Debug, Clone)]
pub struct CostBreakdown {
    /// Raw trigger price (open/close/stop/limit level before costs).
    pub raw_price: D,
    /// Price after adverse spread + slippage adjustments.
    pub fill_price: D,
    /// Money cost of the half-spread on this fill (>= 0).
    pub spread_cost: D,
    /// Money cost of slippage on this fill (>= 0).
    pub slippage_cost: D,
    /// Commission charged on this fill.
    pub commission: D,
    /// Notional at raw price (qty * raw_price * contract_size).
    pub notional: D,
}

/// Build a fill for `qty` units (always positive; `side` gives direction).
pub fn build_fill(
    side: Side,
    raw_price: D,
    qty: D,
    contract_size: D,
    models: &CostModels,
    bar_volume: Option<D>,
) -> CoreResult<CostBreakdown> {
    use rust_decimal_macros::dec;
    if qty <= dec!(0) {
        return Err(CoreError::InvalidOrder(format!(
            "fill quantity must be positive, got {qty}"
        )));
    }
    if raw_price <= dec!(0) {
        return Err(CoreError::InvalidOrder(format!(
            "fill price must be positive, got {raw_price}"
        )));
    }
    let hs = models.spread.half_spread();
    let slip = models.slippage.adverse(raw_price, qty, bar_volume);
    let adjustment = match side {
        Side::Buy => hs + slip,
        Side::Sell => -(hs + slip),
    };
    let fill_price = raw_price
        .checked_add(adjustment)
        .ok_or(CoreError::DecimalOverflow {
            context: "fill price".into(),
        })?;
    let spread_cost = hs * qty * contract_size;
    let slippage_cost = slip * qty * contract_size;
    let notional = qty * raw_price * contract_size;
    let commission = models.commission.charge(notional, qty);
    Ok(CostBreakdown {
        raw_price,
        fill_price,
        spread_cost,
        slippage_cost,
        commission,
        notional,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::{CommissionModel, SlippageModel, SpreadModel};
    use rust_decimal_macros::dec;

    fn models() -> CostModels {
        CostModels {
            spread: SpreadModel::Fixed {
                spread: dec!(0.0002),
            },
            slippage: SlippageModel::Percentage { bps: dec!(1) },
            commission: CommissionModel::Percentage { rate: dec!(0.0002) },
            financing: Default::default(),
        }
    }

    #[test]
    fn buy_fills_at_ask_plus_slippage() {
        let b = build_fill(Side::Buy, dec!(100), dec!(10), dec!(1), &models(), None).unwrap();
        // spread/2 = 0.0001, slippage = 100*0.0001 = 0.01
        assert_eq!(b.fill_price, dec!(100.0101));
        assert_eq!(b.spread_cost, dec!(0.001));
        assert_eq!(b.slippage_cost, dec!(0.1));
        assert_eq!(b.commission, dec!(0.2)); // 0.02% of 1000
    }

    #[test]
    fn sell_fills_at_bid_minus_slippage() {
        let b = build_fill(Side::Sell, dec!(100), dec!(10), dec!(1), &models(), None).unwrap();
        assert_eq!(b.fill_price, dec!(99.9899));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(build_fill(Side::Buy, dec!(100), dec!(0), dec!(1), &models(), None).is_err());
        assert!(build_fill(Side::Buy, dec!(-1), dec!(1), dec!(1), &models(), None).is_err());
    }
}
