//! Risk engine (spec §11). Runs BEFORE order acceptance: computes position
//! size from the strategy's sizing rule and validates every risk limit.
//! Rejections are explicit, reasoned, and logged as events by the engine.

use bt_core::error::CoreError;
use bt_core::instrument::Instrument;
use bt_core::{CoreResult, D};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Position sizing modes. Values are percentages unless noted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SizingMode {
    FixedQuantity {
        qty: D,
    },
    /// Fixed notional in account currency.
    FixedAmount {
        amount: D,
    },
    /// Notional as % of equity (100 = full equity at leverage 1).
    PercentEquity {
        value: D,
    },
    /// Risk per trade as % of equity; requires a stop definition.
    PercentRisk {
        value: D,
    },
    /// Fixed monetary risk per trade; requires a stop definition.
    RiskAmount {
        amount: D,
    },
    /// Risk % of equity with distance = ATR × multiple.
    AtrBased {
        value: D,
        atr_period: u32,
        multiple: D,
    },
}

impl Default for SizingMode {
    fn default() -> Self {
        SizingMode::PercentEquity { value: dec!(100) }
    }
}

/// Pre-trade risk limits. Every field is explicit configuration (spec §42).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    /// Max total notional / equity (1.0 = no leverage).
    #[serde(default = "default_max_leverage")]
    pub max_leverage: D,
    /// Max single-position notional as % of equity.
    #[serde(default = "default_pos_pct")]
    pub max_position_notional_pct: D,
    #[serde(default = "default_concurrent")]
    pub max_concurrent_positions: usize,
    /// Daily loss limit as % of day-start equity (blocks NEW entries only).
    #[serde(default)]
    pub daily_loss_limit_pct: Option<D>,
    /// Total drawdown limit as % from peak equity (blocks NEW entries only).
    #[serde(default)]
    pub max_drawdown_limit_pct: Option<D>,
    #[serde(default)]
    pub max_entry_fills_per_day: Option<u64>,
    /// Pyramiding cap per position.
    #[serde(default = "default_entries")]
    pub max_entries_per_position: u32,
}

fn default_max_leverage() -> D {
    dec!(1)
}
fn default_pos_pct() -> D {
    dec!(100)
}
fn default_concurrent() -> usize {
    10
}
fn default_entries() -> u32 {
    1
}

impl Default for RiskConfig {
    fn default() -> Self {
        RiskConfig {
            max_leverage: default_max_leverage(),
            max_position_notional_pct: default_pos_pct(),
            max_concurrent_positions: default_concurrent(),
            daily_loss_limit_pct: None,
            max_drawdown_limit_pct: None,
            max_entry_fills_per_day: None,
            max_entries_per_position: default_entries(),
        }
    }
}

/// Everything the risk engine needs to know about the current moment.
#[derive(Debug, Clone, Copy)]
pub struct RiskContext {
    pub equity: D,
    pub peak_equity: D,
    pub day_start_equity: D,
    pub open_position_count: usize,
    /// Existing position qty for the target symbol (0 = flat).
    pub symbol_qty: D,
    /// Existing notional for the target symbol (counted into position cap).
    pub symbol_notional: D,
    /// Total notional across ALL open positions (portfolio leverage check).
    pub total_notional: D,
    /// True when this entry will first close an opposing position.
    pub is_reversal: bool,
    pub entries_in_position: u32,
    pub entry_fills_today: u64,
    /// Reference price for sizing (decision-time close).
    pub price: D,
    pub contract_size: D,
    pub leverage: D,
}

pub enum RiskDecision {
    Approved(D),
    Rejected(String),
}

pub struct RiskEngine {
    pub cfg: RiskConfig,
}

impl RiskEngine {
    pub fn new(cfg: RiskConfig) -> Self {
        RiskEngine { cfg }
    }

    /// Compute the position quantity for an entry (floored to qty_step —
    /// sizing must never round risk up).
    pub fn size_position(
        &self,
        mode: &SizingMode,
        ctx: &RiskContext,
        stop_distance: Option<D>,
        atr_value: Option<D>,
        instrument: &Instrument,
    ) -> CoreResult<D> {
        use rust_decimal_macros::dec;
        let price = ctx.price;
        if price <= dec!(0) {
            return Err(CoreError::InvalidOrder(format!(
                "sizing price must be > 0, got {price}"
            )));
        }
        let qty_from_notional = |notional: D| -> CoreResult<D> {
            let units = notional / (price * ctx.contract_size);
            bt_core::money::floor_to_step(units, instrument.qty_step)
        };
        let qty_from_risk = |risk_amount: D, distance: D| -> CoreResult<D> {
            if distance <= dec!(0) {
                return Err(CoreError::RiskViolation(
                    "risk-based sizing requires a positive stop distance".into(),
                ));
            }
            let units = risk_amount / (distance * ctx.contract_size);
            bt_core::money::floor_to_step(units, instrument.qty_step)
        };
        let qty = match mode {
            SizingMode::FixedQuantity { qty } => {
                bt_core::money::floor_to_step(*qty, instrument.qty_step)?
            }
            SizingMode::FixedAmount { amount } => qty_from_notional(*amount)?,
            SizingMode::PercentEquity { value } => {
                let notional = ctx.equity * value / dec!(100);
                qty_from_notional(notional)?
            }
            SizingMode::PercentRisk { value } => {
                let dist = stop_distance.ok_or_else(|| {
                    CoreError::RiskViolation("percent_risk sizing requires a stop".into())
                })?;
                let risk_amt = ctx.equity * value / dec!(100);
                qty_from_risk(risk_amt, dist)?
            }
            SizingMode::RiskAmount { amount } => {
                let dist = stop_distance.ok_or_else(|| {
                    CoreError::RiskViolation("risk_amount sizing requires a stop".into())
                })?;
                qty_from_risk(*amount, dist)?
            }
            SizingMode::AtrBased {
                value, multiple, ..
            } => {
                let atr = atr_value.ok_or_else(|| {
                    CoreError::RiskViolation("atr-based sizing requires ATR data".into())
                })?;
                let dist = atr * multiple;
                let risk_amt = ctx.equity * value / dec!(100);
                qty_from_risk(risk_amt, dist)?
            }
        };
        Ok(qty)
    }

    /// Validate an ENTRY (new or increasing). Exits are never blocked by risk
    /// limits (risk limits protect capital; blocking exits would do the opposite).
    pub fn check_entry(&self, ctx: &RiskContext, qty: D) -> RiskDecision {
        use rust_decimal_macros::dec;
        let reject = |reason: String| RiskDecision::Rejected(reason);
        if qty <= dec!(0) {
            return reject("sized quantity is zero (risk amount too small for qty step)".into());
        }
        if ctx.entries_in_position >= self.cfg.max_entries_per_position {
            return reject(format!(
                "max entries per position reached ({})",
                self.cfg.max_entries_per_position
            ));
        }
        if !ctx.is_reversal
            && ctx.symbol_qty == dec!(0)
            && ctx.open_position_count >= self.cfg.max_concurrent_positions
        {
            return reject(format!(
                "max concurrent positions reached ({})",
                self.cfg.max_concurrent_positions
            ));
        }
        // Daily loss limit
        if let Some(limit) = self.cfg.daily_loss_limit_pct {
            if ctx.day_start_equity > dec!(0) {
                let floor = ctx.day_start_equity - ctx.day_start_equity * limit / dec!(100);
                if ctx.equity <= floor {
                    return reject(format!(
                        "daily loss limit hit ({limit}% below day start {})",
                        ctx.day_start_equity
                    ));
                }
            }
        }
        // Total drawdown limit
        if let Some(limit) = self.cfg.max_drawdown_limit_pct {
            if ctx.peak_equity > dec!(0) {
                let floor = ctx.peak_equity - ctx.peak_equity * limit / dec!(100);
                if ctx.equity <= floor {
                    return reject(format!(
                        "max drawdown limit hit ({limit}% below peak {})",
                        ctx.peak_equity
                    ));
                }
            }
        }
        // Trades per day
        if let Some(max) = self.cfg.max_entry_fills_per_day {
            if ctx.entry_fills_today >= max {
                return reject(format!("max entry fills per day reached ({max})"));
            }
        }
        // Notional/leverage checks
        let notional = qty * ctx.price * ctx.contract_size;
        let pos_cap = ctx.equity * self.cfg.max_position_notional_pct / dec!(100);
        let symbol_notional_after = if ctx.is_reversal {
            notional
        } else {
            notional + ctx.symbol_notional
        };
        if symbol_notional_after > pos_cap {
            return reject(format!(
                "position notional {symbol_notional_after} exceeds cap {pos_cap} ({}% of equity)",
                self.cfg.max_position_notional_pct
            ));
        }
        let lev = ctx.leverage.max(dec!(1));
        let total_after = if ctx.is_reversal {
            ctx.total_notional
        } else {
            ctx.total_notional + notional
        };
        if total_after / lev > ctx.equity * self.cfg.max_leverage {
            return reject(format!(
                "portfolio notional {total_after} at leverage {lev} would exceed max_leverage ({})",
                self.cfg.max_leverage
            ));
        }
        RiskDecision::Approved(qty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(equity: D) -> RiskContext {
        RiskContext {
            equity,
            peak_equity: equity,
            day_start_equity: equity,
            open_position_count: 0,
            symbol_qty: dec!(0),
            symbol_notional: dec!(0),
            total_notional: dec!(0),
            is_reversal: false,
            entries_in_position: 0,
            entry_fills_today: 0,
            price: dec!(100),
            contract_size: dec!(1),
            leverage: dec!(1),
        }
    }

    fn inst() -> Instrument {
        let mut i = Instrument::new("X");
        i.qty_step = dec!(0.01);
        i
    }

    #[test]
    fn percent_risk_sizing_floors_to_step() {
        // Spec test 15: equity 100k, risk 1% = 1000, distance 5 => 200 units.
        let e = RiskEngine::new(RiskConfig::default());
        let qty = e
            .size_position(
                &SizingMode::PercentRisk { value: dec!(1) },
                &ctx(dec!(100000)),
                Some(dec!(5)),
                None,
                &inst(),
            )
            .unwrap();
        assert_eq!(qty, dec!(200));
        // Non-representable: 1000 / 3 = 333.33 floor
        let qty2 = e
            .size_position(
                &SizingMode::PercentRisk { value: dec!(1) },
                &ctx(dec!(100000)),
                Some(dec!(3)),
                None,
                &inst(),
            )
            .unwrap();
        assert_eq!(qty2, dec!(333.33));
    }

    #[test]
    fn percent_risk_requires_stop() {
        let e = RiskEngine::new(RiskConfig::default());
        assert!(e
            .size_position(
                &SizingMode::PercentRisk { value: dec!(1) },
                &ctx(dec!(100000)),
                None,
                None,
                &inst()
            )
            .is_err());
    }

    #[test]
    fn concurrent_position_limit() {
        let e = RiskEngine::new(RiskConfig {
            max_concurrent_positions: 2,
            ..Default::default()
        });
        let mut c = ctx(dec!(100000));
        c.open_position_count = 2;
        match e.check_entry(&c, dec!(1)) {
            RiskDecision::Rejected(r) => assert!(r.contains("concurrent")),
            _ => panic!("expected rejection"),
        }
        // Adding to an existing symbol position is not a new concurrent position.
        c.symbol_qty = dec!(5);
        c.symbol_notional = dec!(500);
        assert!(matches!(
            e.check_entry(&c, dec!(1)),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn daily_loss_limit_blocks_entries() {
        let e = RiskEngine::new(RiskConfig {
            daily_loss_limit_pct: Some(dec!(5)),
            ..Default::default()
        });
        let mut c = ctx(dec!(94999)); // > 5% below day start of 100000
        c.day_start_equity = dec!(100000);
        assert!(matches!(
            e.check_entry(&c, dec!(1)),
            RiskDecision::Rejected(_)
        ));
        let mut c2 = ctx(dec!(95100));
        c2.day_start_equity = dec!(100000);
        assert!(matches!(
            e.check_entry(&c2, dec!(1)),
            RiskDecision::Approved(_)
        ));
    }

    #[test]
    fn drawdown_limit_blocks_entries() {
        let e = RiskEngine::new(RiskConfig {
            max_drawdown_limit_pct: Some(dec!(20)),
            ..Default::default()
        });
        let mut c = ctx(dec!(79000));
        c.peak_equity = dec!(100000);
        assert!(matches!(
            e.check_entry(&c, dec!(1)),
            RiskDecision::Rejected(_)
        ));
    }

    #[test]
    fn notional_cap_and_leverage() {
        let e = RiskEngine::new(RiskConfig::default());
        // 100% cap: notional 200*100 = 20000 <= 100000 ok
        assert!(matches!(
            e.check_entry(&ctx(dec!(100000)), dec!(200)),
            RiskDecision::Approved(_)
        ));
        // 1500 units * 100 = 150000 > 100000 => rejected
        assert!(matches!(
            e.check_entry(&ctx(dec!(100000)), dec!(1500)),
            RiskDecision::Rejected(_)
        ));
        // Existing symbol notional counts into the position cap.
        let mut c = ctx(dec!(100000));
        c.symbol_qty = dec!(600);
        c.symbol_notional = dec!(60000);
        // order 500 units => 60000+50000=110000 > 100000 => rejected
        assert!(matches!(
            e.check_entry(&c, dec!(500)),
            RiskDecision::Rejected(_)
        ));
        // Reversal replaces the position: only the new notional counts.
        c.is_reversal = true;
        assert!(matches!(
            e.check_entry(&c, dec!(500)),
            RiskDecision::Approved(_)
        ));
        // Portfolio leverage: existing 60000 elsewhere + 50000 > equity*1 => rejected.
        let mut c2 = ctx(dec!(100000));
        c2.total_notional = dec!(60000);
        assert!(matches!(
            e.check_entry(&c2, dec!(500)),
            RiskDecision::Rejected(_)
        ));
    }

    #[test]
    fn fixed_quantity_sizing() {
        let e = RiskEngine::new(RiskConfig::default());
        let qty = e
            .size_position(
                &SizingMode::FixedQuantity {
                    qty: dec!(1.234567),
                },
                &ctx(dec!(100000)),
                None,
                None,
                &inst(),
            )
            .unwrap();
        assert_eq!(qty, dec!(1.23));
    }
}
