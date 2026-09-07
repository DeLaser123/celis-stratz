//! Instrument contract specification. Every value is explicit configuration —
//! the engine applies documented defaults and prints them in `experiment.json`.

use crate::money::D;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instrument {
    pub symbol: String,
    /// Units of the quote currency per 1 unit of quantity (e.g. FX lot = 100_000).
    #[serde(default = "default_contract_size")]
    pub contract_size: D,
    /// Account leverage used for margin computation.
    #[serde(default = "default_leverage")]
    pub leverage: D,
    /// Quantity granularity; sizing floors to this step (conservative).
    #[serde(default = "default_qty_step")]
    pub qty_step: D,
    /// Optional price tick rounding for generated stop/target levels.
    #[serde(default)]
    pub tick_size: Option<D>,
    #[serde(default = "default_quote")]
    pub quote_currency: String,
    #[serde(default = "default_base")]
    pub base_currency: String,
}

fn default_contract_size() -> D {
    rust_decimal_macros::dec!(1)
}
fn default_leverage() -> D {
    rust_decimal_macros::dec!(1)
}
fn default_qty_step() -> D {
    rust_decimal_macros::dec!(0.000001)
}
fn default_quote() -> String {
    "USD".into()
}
fn default_base() -> String {
    "".into()
}

impl Instrument {
    pub fn new(symbol: &str) -> Self {
        Instrument {
            symbol: symbol.to_string(),
            contract_size: default_contract_size(),
            leverage: default_leverage(),
            qty_step: default_qty_step(),
            tick_size: None,
            quote_currency: default_quote(),
            base_currency: default_base(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_explicit() {
        let i = Instrument::new("EURUSD");
        assert_eq!(i.contract_size.to_string(), "1");
        assert_eq!(i.leverage.to_string(), "1");
        assert_eq!(i.qty_step.to_string(), "0.000001");
        assert_eq!(i.quote_currency, "USD");
    }
}
