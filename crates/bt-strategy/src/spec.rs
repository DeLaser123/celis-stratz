//! Machine-readable strategy specification (spec §14, §44). Strict
//! deserialization: unknown fields and ambiguous shapes are errors, so an AI
//! compiling natural language into this schema gets exact feedback.

use crate::expr::{parse_expr, BoolOp, Expr};
use bt_core::D;
use bt_risk::SizingMode;
use serde::{Deserialize, Serialize};

pub use crate::expr::BoolOp as SpecBoolOp;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySpec {
    pub name: String,
    pub symbols: Vec<String>,
    /// Additional (higher) timeframes referenced by expressions, e.g. ["1D"].
    #[serde(default)]
    pub timeframes: Vec<String>,
    pub entry: EntryRule,
    #[serde(default)]
    pub entry_short: Option<EntryRule>,
    #[serde(default)]
    pub exit: Option<ExitRule>,
    #[serde(default)]
    pub orders: Orders,
    #[serde(default)]
    pub risk: RiskBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryRule {
    pub direction: Direction,
    pub when: When,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitRule {
    pub when: When,
}

/// `when: {all: [...]}` | `when: {any: [...]}` | a single expression.
#[derive(Debug, Clone)]
pub struct When {
    pub op: BoolOp,
    pub exprs: Vec<Expr>,
}

impl<'de> Deserialize<'de> for When {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_yaml::Value::deserialize(deserializer)?;
        parse_when(&v).map_err(serde::de::Error::custom)
    }
}

fn parse_when(v: &serde_yaml::Value) -> Result<When, String> {
    if let Some(map) = v.as_mapping() {
        if map.len() == 1 {
            if let Some(all) = get(map, "all") {
                return list_when(all, BoolOp::All, "when.all");
            }
            if let Some(any) = get(map, "any") {
                return list_when(any, BoolOp::Any, "when.any");
            }
        }
    }
    // Single expression form.
    let e = parse_expr(v, None, "when").map_err(|e| format!("when: {e}"))?;
    Ok(When {
        op: BoolOp::All,
        exprs: vec![e],
    })
}

fn list_when(v: &serde_yaml::Value, op: BoolOp, path: &str) -> Result<When, String> {
    let seq = v
        .as_sequence()
        .ok_or_else(|| format!("{path}: expected a list of conditions"))?;
    if seq.is_empty() {
        return Err(format!("{path}: condition list is empty"));
    }
    let exprs = seq
        .iter()
        .enumerate()
        .map(|(i, c)| parse_expr(c, None, &format!("{path}[{i}]")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(When { op, exprs })
}

fn get<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    map.get(serde_yaml::Value::String(key.to_string()))
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Orders {
    #[serde(default)]
    pub entry: EntryOrderDef,
    #[serde(default)]
    pub stop_loss: StopLossDef,
    #[serde(default)]
    pub take_profit: TakeProfitDef,
    #[serde(default)]
    pub trailing_stop: Option<TrailingDef>,
}

/// Entry order type. `market` fills at next open / current close; `limit`,
/// `stop` and `stop_limit` rest and trigger intrabar on later bars.
/// Prices may be numbers or expressions (evaluated at decision time).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryOrderDef {
    #[default]
    Market,
    Limit {
        price: PriceSpec,
    },
    Stop {
        price: PriceSpec,
    },
    StopLimit {
        stop_price: PriceSpec,
        limit_price: PriceSpec,
    },
}

/// Entry trigger price: a fixed number or an expression evaluated at the
/// decision timestamp (e.g. the prior bar's high).
#[derive(Debug, Clone)]
pub enum PriceSpec {
    Fixed(D),
    Expr {
        expr: crate::expr::Expr,
        raw: serde_yaml::Value,
    },
}

impl<'de> serde::Deserialize<'de> for PriceSpec {
    fn deserialize<De>(deserializer: De) -> Result<Self, De::Error>
    where
        De: serde::Deserializer<'de>,
    {
        let v = serde_yaml::Value::deserialize(deserializer)?;
        match &v {
            serde_yaml::Value::Number(_) | serde_yaml::Value::String(_) => {
                let num = crate::expr::yaml_num(&v, "orders.entry.price")
                    .map_err(serde::de::Error::custom)?;
                Ok(PriceSpec::Fixed(num))
            }
            serde_yaml::Value::Mapping(_) => {
                let expr = crate::expr::parse_expr(&v, None, "orders.entry.price")
                    .map_err(serde::de::Error::custom)?;
                Ok(PriceSpec::Expr { expr, raw: v })
            }
            other => Err(serde::de::Error::custom(format!(
                "orders.entry price must be a number or an expression mapping, got {other:?}"
            ))),
        }
    }
}

impl Serialize for PriceSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            PriceSpec::Fixed(d) => serializer.serialize_str(&d.to_string()),
            PriceSpec::Expr { raw, .. } => raw.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopLossDef {
    #[default]
    None,
    FixedDistance {
        value: D,
    },
    AtrMultiple {
        period: u32,
        multiple: D,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TakeProfitDef {
    #[default]
    None,
    FixedDistance {
        value: D,
    },
    RiskMultiple {
        value: D,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrailingDef {
    FixedDistance { value: D },
    AtrMultiple { period: u32, multiple: D },
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RiskBlock {
    #[serde(default = "default_sizing")]
    pub sizing: SizingMode,
}

fn default_sizing() -> SizingMode {
    SizingMode::default()
}

/// Document wrapper: `{strategy: {...}}` (canonical) or the bare mapping.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrategyDoc {
    strategy: StrategySpec,
}

/// Parse a strategy specification from YAML text. Accepts both the canonical
/// `{strategy: ...}` wrapper and the bare field mapping. Error precedence:
/// a real error inside the wrapper is reported as-is; only a *missing*
/// wrapper falls back to the bare form (so exact reasons are never masked).
pub fn parse_spec(text: &str) -> Result<StrategySpec, String> {
    match serde_yaml::from_str::<StrategyDoc>(text) {
        Ok(doc) => Ok(doc.strategy),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("missing field `strategy`") {
                serde_yaml::from_str::<StrategySpec>(text).map_err(|e2| e2.to_string())
            } else {
                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
strategy:
  name: sma_cross
  symbols: [EURUSD]
  entry:
    direction: long
    when:
      all:
        - cross_above: [{field: close}, {sma: {source: {field: close}, period: 20}}]
        - gt: [{field: close}, 1.0]
  orders:
    stop_loss: {type: fixed_distance, value: 0.0050}
    take_profit: {type: risk_multiple, value: 3.0}
  risk:
    sizing: {mode: percent_risk, value: 1.0}
"#;

    #[test]
    fn parses_valid_spec() {
        let spec: StrategySpec = parse_spec(VALID).unwrap();
        assert_eq!(spec.name, "sma_cross");
        assert_eq!(spec.entry.direction, Direction::Long);
        assert_eq!(spec.entry.when.op, BoolOp::All);
        assert_eq!(spec.entry.when.exprs.len(), 2);
        assert!(matches!(
            spec.orders.stop_loss,
            StopLossDef::FixedDistance { .. }
        ));
        assert!(matches!(spec.risk.sizing, SizingMode::PercentRisk { .. }));
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = VALID.replace("name: sma_cross", "name: sma_cross\n  typo_field: 1");
        assert!(parse_spec(&bad).is_err());
    }

    #[test]
    fn rejects_bad_condition_shape() {
        let bad = VALID.replace("- gt: [{field: close}, 1.0]", "- gt: [{field: close}]");
        let err = parse_spec(&bad).unwrap_err();
        assert!(err.contains("exactly 2 operands"), "got {err}");
    }

    #[test]
    fn single_expression_when() {
        let s = VALID.replace(
            "      all:\n        - cross_above: [{field: close}, {sma: {source: {field: close}, period: 20}}]\n        - gt: [{field: close}, 1.0]",
            "      gt: [{field: close}, 1.0]",
        );
        let spec: StrategySpec = parse_spec(&s).unwrap();
        assert_eq!(spec.entry.when.exprs.len(), 1);
    }

    #[test]
    fn defaults_are_explicit() {
        let spec: StrategySpec = parse_spec(VALID).unwrap();
        assert!(matches!(
            spec.orders.take_profit,
            TakeProfitDef::RiskMultiple { .. }
        ));
        assert!(spec.orders.trailing_stop.is_none());
        assert!(matches!(spec.risk.sizing, SizingMode::PercentRisk { .. }));
    }
}
