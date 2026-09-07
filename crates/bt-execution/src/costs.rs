//! Cost models (spec §12). Pluggable, explicitly configured, never silently
//! combined. Every fill decomposes into raw price + spread + slippage +
//! commission so gross/net P&L attribution is exact.

use bt_core::D;
use serde::{Deserialize, Serialize};

/// Spread quoted in price units; half is paid per side (buy at ask = mid + s/2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpreadModel {
    #[default]
    Zero,
    Fixed {
        spread: D,
    },
}

impl SpreadModel {
    pub fn half_spread(&self) -> D {
        match self {
            SpreadModel::Zero => rust_decimal_macros::dec!(0),
            SpreadModel::Fixed { spread } => *spread / rust_decimal_macros::dec!(2),
        }
    }
}

/// Slippage is an adverse price adjustment applied after the spread.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SlippageModel {
    #[default]
    Zero,
    /// Absolute price units.
    Fixed { value: D },
    /// Basis points of price (1 bps = 0.01%).
    Percentage { bps: D },
    /// Square-root market-impact law: adverse move (as a fraction of price)
    /// = `coefficient` x sqrt(qty / bar_volume). Falls back to zero slippage
    /// when the bar's volume is absent (documented).
    SquareRootImpact { coefficient: D },
}

impl SlippageModel {
    /// Adverse price adjustment for one fill. `qty` is the fill quantity and
    /// `bar_volume` the volume of the bar being filled (when known).
    pub fn adverse(&self, price: D, qty: D, bar_volume: Option<D>) -> D {
        match self {
            SlippageModel::Zero => rust_decimal_macros::dec!(0),
            SlippageModel::Fixed { value } => *value,
            SlippageModel::Percentage { bps } => price * bps / rust_decimal_macros::dec!(10000),
            SlippageModel::SquareRootImpact { coefficient } => match bar_volume {
                Some(v)
                    if v > rust_decimal_macros::dec!(0) && qty > rust_decimal_macros::dec!(0) =>
                {
                    let participation = qty / v;
                    // sqrt via the documented f64 statistics boundary.
                    let root =
                        bt_core::money::d_from_f64(bt_core::money::to_f64(participation).sqrt())
                            .unwrap_or(rust_decimal_macros::dec!(0));
                    price * coefficient * root
                }
                _ => rust_decimal_macros::dec!(0),
            },
        }
    }
}

/// Commission charged on each fill. `Fixed.per_unit` is per unit of quantity;
/// `Fixed.per_order` is a flat fee. `Percentage.rate` applies to notional
/// (rate 0.0002 = 0.02%).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommissionModel {
    #[default]
    Zero,
    Fixed {
        per_order: D,
        per_unit: Option<D>,
    },
    Percentage {
        rate: D,
    },
}

impl CommissionModel {
    pub fn charge(&self, notional: D, qty: D) -> D {
        match self {
            CommissionModel::Zero => rust_decimal_macros::dec!(0),
            CommissionModel::Fixed {
                per_order,
                per_unit,
            } => {
                let mut c = *per_order;
                if let Some(pu) = per_unit {
                    c += pu * qty;
                }
                c
            }
            CommissionModel::Percentage { rate } => notional * rate,
        }
    }
}

/// Financing (swap) for positions held across bars. Rates are daily fractions
/// of notional (0.00001 = 0.001%/day). Long pays `long` rate, short pays
/// `short` rate (a negative rate pays the holder). Applied per bar, pro rata.
/// `TermCurve` varies the rate by holding duration: the segment with the
/// largest `after_days <= held_days` applies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinancingModel {
    #[default]
    Zero,
    DailyRate {
        long: D,
        short: D,
    },
    TermCurve {
        segments: Vec<TermSegment>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermSegment {
    /// Holding duration (in days) from which this segment applies.
    pub after_days: u32,
    pub long: D,
    pub short: D,
}

impl FinancingModel {
    /// Signed cost for the given signed quantity (positive = charge).
    /// `held_days` is the position's holding duration in days (floor).
    pub fn per_bar(
        &self,
        signed_qty: D,
        price: D,
        contract_size: D,
        bars_per_day: D,
        held_days: u64,
    ) -> D {
        let zero = rust_decimal_macros::dec!(0);
        let rate: D = match self {
            FinancingModel::Zero => return zero,
            FinancingModel::DailyRate { long, short } => {
                if signed_qty >= zero {
                    *long
                } else {
                    *short
                }
            }
            FinancingModel::TermCurve { segments } => {
                let mut rate = None;
                let mut best: i64 = -1;
                for seg in segments {
                    let after = i64::from(seg.after_days);
                    if after <= i64::try_from(held_days).unwrap_or(i64::MAX) && after >= best {
                        best = after;
                        rate = Some(if signed_qty >= zero {
                            seg.long
                        } else {
                            seg.short
                        });
                    }
                }
                match rate {
                    Some(r) => r,
                    None => return zero,
                }
            }
        };
        let notional = signed_qty.abs() * price * contract_size;
        if bars_per_day.is_zero() {
            return zero;
        }
        notional * rate / bars_per_day
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostModels {
    #[serde(default)]
    pub spread: SpreadModel,
    #[serde(default)]
    pub slippage: SlippageModel,
    #[serde(default)]
    pub commission: CommissionModel,
    #[serde(default)]
    pub financing: FinancingModel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn spread_halves() {
        assert_eq!(
            SpreadModel::Fixed { spread: dec!(2) }.half_spread(),
            dec!(1)
        );
        assert_eq!(SpreadModel::Zero.half_spread(), dec!(0));
    }

    #[test]
    fn slippage_bps() {
        let m = SlippageModel::Percentage { bps: dec!(1) };
        assert_eq!(m.adverse(dec!(100), dec!(1), None), dec!(0.01));
    }

    #[test]
    fn commission_models() {
        let fixed = CommissionModel::Fixed {
            per_order: dec!(1),
            per_unit: Some(dec!(0.01)),
        };
        assert_eq!(fixed.charge(dec!(0), dec!(100)), dec!(2));
        let pct = CommissionModel::Percentage { rate: dec!(0.0002) };
        assert_eq!(pct.charge(dec!(10000), dec!(1)), dec!(2));
    }

    #[test]
    fn financing_signs() {
        let f = FinancingModel::DailyRate {
            long: dec!(0.0001),
            short: dec!(-0.0001),
        };
        let bpd = dec!(24);
        // long pays
        assert_eq!(
            f.per_bar(dec!(10000), dec!(1), dec!(1), bpd, 0),
            dec!(10000) * dec!(0.0001) / bpd
        );
        // short receives (negative cost)
        assert_eq!(
            f.per_bar(dec!(-10000), dec!(1), dec!(1), bpd, 0),
            -(dec!(10000) * dec!(0.0001) / bpd)
        );
    }
}
