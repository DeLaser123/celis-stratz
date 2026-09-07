//! Exact decimal arithmetic helpers (spec §4).
//!
//! Rules:
//! - accounting uses `Decimal` with **checked** arithmetic everywhere; overflow
//!   is a typed error, never a wrapped value;
//! - **no implicit rounding**: internal values keep full Decimal precision;
//! - explicit rounding happens only through the helpers below.

use crate::error::{CoreError, CoreResult};
use rust_decimal::{Decimal, RoundingStrategy};

pub type D = Decimal;

pub const ZERO: D = Decimal::ZERO;
pub const ONE: D = Decimal::ONE;

/// Checked add; overflow/NaN-free by construction.
pub fn d_add(a: D, b: D, ctx: &str) -> CoreResult<D> {
    a.checked_add(b).ok_or_else(|| CoreError::DecimalOverflow {
        context: ctx.into(),
    })
}

pub fn d_sub(a: D, b: D, ctx: &str) -> CoreResult<D> {
    a.checked_sub(b).ok_or_else(|| CoreError::DecimalOverflow {
        context: ctx.into(),
    })
}

pub fn d_mul(a: D, b: D, ctx: &str) -> CoreResult<D> {
    a.checked_mul(b).ok_or_else(|| CoreError::DecimalOverflow {
        context: ctx.into(),
    })
}

/// Checked division. Division by zero is an accounting error, not a panic.
pub fn d_div(a: D, b: D, ctx: &str) -> CoreResult<D> {
    if b.is_zero() {
        return Err(CoreError::AccountingInvariantViolation(format!(
            "division by zero in {ctx}"
        )));
    }
    a.checked_div(b).ok_or_else(|| CoreError::DecimalOverflow {
        context: ctx.into(),
    })
}

pub fn d_abs(a: D) -> D {
    a.abs()
}

/// Floor a value to a step (used for quantity sizing: **never** rounds risk up).
pub fn floor_to_step(v: D, step: D) -> CoreResult<D> {
    if step <= ZERO {
        return Ok(v);
    }
    let units = d_div(v, step, "floor_to_step")?;
    Ok(units.floor() * step)
}

/// Round to `dp` decimal places with midpoint-away-from-zero (explicit rule).
pub fn round_dp(v: D, dp: u32) -> D {
    v.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero)
}

/// Round-half-away formatting used by all CSV/JSON output writers.
pub fn fmt_money(v: D, dp: u32) -> String {
    round_dp(v, dp).normalize().to_string()
}

pub fn fmt_price(v: D) -> String {
    v.normalize().to_string()
}

/// Decimal from f64 (statistics boundary only — never used for accounting).
pub fn d_from_f64(v: f64) -> CoreResult<D> {
    Decimal::from_f64_retain(v)
        .ok_or_else(|| CoreError::InvalidData(format!("cannot represent {v} as decimal")))
}

/// f64 from Decimal (statistics boundary; documented conversion).
pub fn to_f64(v: D) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    v.to_f64().unwrap_or_else(|| {
        if v.is_sign_negative() {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }
    })
}

/// Exponentiation for CAGR-type math (f64 domain, documented).
pub fn pow_f64(base: f64, exp: f64) -> f64 {
    base.powf(exp)
}

/// d^n for non-negative integer n, in exact Decimal via checked squaring.
pub fn d_powi(base: D, n: u64, ctx: &str) -> CoreResult<D> {
    let mut acc = ONE;
    let mut b = base;
    let mut e = n;
    while e > 0 {
        if e & 1 == 1 {
            acc = d_mul(acc, b, ctx)?;
        }
        e >>= 1;
        if e > 0 {
            b = d_mul(b, b, ctx)?;
        }
    }
    Ok(acc)
}

/// sqrt in f64 (statistics boundary).
pub fn sqrt_f64(v: f64) -> f64 {
    v.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn checked_ops() {
        assert_eq!(d_add(dec!(1.5), dec!(2.25), "t").unwrap(), dec!(3.75));
        assert!(d_div(dec!(1), ZERO, "t").is_err());
        let big = Decimal::MAX;
        assert!(d_add(big, ONE, "t").is_err());
    }

    #[test]
    fn floor_step_never_rounds_risk_up() {
        // risk per unit 0.005, risk amount 100 -> 20000 units exactly
        assert_eq!(floor_to_step(dec!(20000), dec!(0.01)).unwrap(), dec!(20000));
        // 19999.999 units of step 0.01 floors down
        assert_eq!(
            floor_to_step(dec!(0.999999), dec!(0.01)).unwrap(),
            dec!(0.99)
        );
    }

    #[test]
    fn rounding_is_half_away() {
        assert_eq!(round_dp(dec!(0.125), 2), dec!(0.13));
        assert_eq!(round_dp(dec!(0.135), 2), dec!(0.14));
        assert_eq!(round_dp(dec!(-0.125), 2), dec!(-0.13));
    }

    #[test]
    fn money_formatting() {
        assert_eq!(fmt_money(dec!(100000.10), 2), "100000.1");
        assert_eq!(fmt_money(dec!(0.0050000), 4), "0.005");
    }
}
