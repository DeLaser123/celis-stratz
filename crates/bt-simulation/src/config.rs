//! Engine configuration (spec §42-43). Every field is explicit; missing
//! fields take documented defaults, and the FULLY resolved configuration is
//! stored in `experiment.json` so no assumption is ever hidden.

use bt_analytics::AnalyticsConfig;
use bt_core::instrument::Instrument;
use bt_core::D;
use bt_data::{LoadLimits, TimestampConvention, ValidationMode};
use bt_execution::{AmbiguityPolicy, CostModels, TriggerPriority};
use bt_risk::RiskConfig;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketConfig {
    /// Base timeframe (e.g. "1h"). If omitted it is inferred from the data
    /// and the inferred value is recorded in experiment.json.
    #[serde(default)]
    pub timeframe: Option<String>,
    #[serde(default)]
    pub timestamp_convention: TimestampConvention,
    /// IANA timezone for naive CSV timestamps.
    #[serde(default = "default_tz")]
    pub timezone: String,
    #[serde(default)]
    pub validation_mode: ValidationMode,
}

fn default_tz() -> String {
    "UTC".into()
}

impl Default for MarketConfig {
    fn default() -> Self {
        MarketConfig {
            timeframe: None,
            timestamp_convention: TimestampConvention::Open,
            timezone: default_tz(),
            validation_mode: ValidationMode::Strict,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    #[serde(default = "default_capital")]
    pub starting_capital: D,
    #[serde(default = "default_currency")]
    pub currency: String,
    /// Fallback leverage for instruments without an override.
    #[serde(default = "default_leverage")]
    pub leverage: D,
}

fn default_capital() -> D {
    dec!(100000)
}
fn default_currency() -> String {
    "USD".into()
}
fn default_leverage() -> D {
    dec!(1)
}

impl Default for AccountConfig {
    fn default() -> Self {
        AccountConfig {
            starting_capital: default_capital(),
            currency: default_currency(),
            leverage: default_leverage(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionModel {
    #[default]
    NextOpen,
    CurrentClose,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    #[serde(default)]
    pub model: ExecutionModel,
    #[serde(default)]
    pub intrabar_policy: AmbiguityPolicy,
    /// Required when intrabar_policy = explicit.
    #[serde(default)]
    pub explicit_priority: Option<Vec<TriggerPriority>>,
    /// Require price to trade THROUGH a level (strict) rather than touch it.
    #[serde(default)]
    pub strict_trigger: bool,
    /// Max participation as a fraction of bar volume for ENTRY fills
    /// (e.g. 0.1 = fill at most 10% of the bar's volume per bar).
    /// Protective/exit and end-of-data fills are never capped.
    #[serde(default)]
    pub participation_cap: Option<D>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        ExecutionConfig {
            model: ExecutionModel::NextOpen,
            intrabar_policy: AmbiguityPolicy::default(),
            explicit_priority: None,
            strict_trigger: false,
            participation_cap: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EndPolicy {
    #[default]
    CloseAll,
    MarkToMarket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IndicatorModeConfig {
    #[default]
    Precompute,
    Streaming,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub end_policy: EndPolicy,
    /// Decimal places used ONLY in the terminal report display.
    #[serde(default = "default_dp")]
    pub display_dp: u32,
    /// Indicator computation path (both are differential-tested; results
    /// must be identical).
    #[serde(default)]
    pub indicator_mode: IndicatorModeConfig,
}

fn default_dp() -> u32 {
    2
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            end_policy: EndPolicy::CloseAll,
            display_dp: default_dp(),
            indicator_mode: IndicatorModeConfig::Precompute,
        }
    }
}

/// Per-symbol instrument overrides, merged over documented defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct InstrumentPatch {
    pub contract_size: Option<D>,
    pub leverage: Option<D>,
    pub qty_step: Option<D>,
    pub tick_size: Option<D>,
    pub quote_currency: Option<String>,
    pub base_currency: Option<String>,
}

impl InstrumentPatch {
    pub fn apply(&self, symbol: &str, fallback_leverage: D) -> Instrument {
        let mut inst = Instrument::new(symbol);
        inst.leverage = fallback_leverage;
        if let Some(v) = self.contract_size {
            inst.contract_size = v;
        }
        if let Some(v) = self.leverage {
            inst.leverage = v;
        }
        if let Some(v) = self.qty_step {
            inst.qty_step = v;
        }
        inst.tick_size = self.tick_size.or(inst.tick_size);
        if let Some(v) = &self.quote_currency {
            inst.quote_currency = v.clone();
        }
        if let Some(v) = &self.base_currency {
            inst.base_currency = v.clone();
        }
        inst
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    #[serde(default)]
    pub market: MarketConfig,
    #[serde(default)]
    pub account: AccountConfig,
    #[serde(default)]
    pub costs: CostModels,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub risk: RiskConfig,
    #[serde(default)]
    pub analytics: AnalyticsConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub limits: LoadLimits,
    #[serde(default)]
    pub instruments: BTreeMap<String, InstrumentPatch>,
    /// Static experiment-level macro inputs: key -> value.
    #[serde(default)]
    pub macro_inputs: BTreeMap<String, D>,
}

impl EngineConfig {
    pub fn parse(text: &str) -> CoreResult<EngineConfig> {
        serde_yaml::from_str(text)
            .map_err(|e| bt_core::CoreError::ConfigError(format!("config parse error: {e}")))
    }

    pub fn resolve_instruments(&self, symbols: &[String]) -> BTreeMap<String, Instrument> {
        let mut out = BTreeMap::new();
        for s in symbols {
            let patch = self.instruments.get(s).cloned().unwrap_or_default();
            out.insert(s.clone(), patch.apply(s, self.account.leverage));
        }
        out
    }
}

use bt_core::CoreResult;
