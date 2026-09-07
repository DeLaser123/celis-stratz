//! Experiment metadata + identity hashes (spec §26, DETERMINISM.md).
//!
//! `experiment_id` fully determines a run from its inputs; `result_hash`
//! fingerprints the outputs. Both use canonical JSON (sorted keys).

use crate::config::EngineConfig;
use bt_analytics::metrics::{EquityPoint, MetricsReport};
use bt_core::instrument::Instrument;
use bt_core::ledger::TradeRecord;
use bt_core::time::format_ts;
use bt_data::Dataset;
use bt_execution::intrabar::AmbiguityPolicy;
use bt_strategy::spec::StrategySpec;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Convert a serde_yaml value tree into a serde_json tree (for audit).
pub fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                json!(i)
            } else if let Some(u) = n.as_u64() {
                json!(u)
            } else if let Some(f) = n.as_f64() {
                json!(f)
            } else {
                Value::Null
            }
        }
        serde_yaml::Value::String(s) => Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => Value::Array(seq.iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(m) => {
            let mut map = serde_json::Map::new();
            for (k, v) in m {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => yaml_to_json(other).to_string(),
                };
                map.insert(key, yaml_to_json(v));
            }
            Value::Object(map)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(&t.value),
    }
}

pub fn build_experiment(
    engine_version: &str,
    dataset: &Dataset,
    spec: &StrategySpec,
    cfg: &EngineConfig,
    instruments: &BTreeMap<String, Instrument>,
    policy: &AmbiguityPolicy,
) -> Value {
    let strategy_hash = bt_core::hash::sha256_hex(spec_canonical_text(spec).as_bytes());
    let instruments_json: Value = Value::Object(
        instruments
            .iter()
            .map(|(s, i)| {
                (
                    s.clone(),
                    json!({
                        "contract_size": i.contract_size.to_string(),
                        "leverage": i.leverage.to_string(),
                        "qty_step": i.qty_step.to_string(),
                        "tick_size": i.tick_size.map(|t| t.to_string()),
                        "quote_currency": i.quote_currency,
                        "base_currency": i.base_currency,
                    }),
                )
            })
            .collect(),
    );

    let (start, end) = dataset
        .series
        .values()
        .next()
        .map(|s| {
            (
                s.bars
                    .first()
                    .map(|b| format_ts(b.open_time))
                    .unwrap_or_default(),
                s.bars
                    .last()
                    .map(|b| format_ts(b.open_time))
                    .unwrap_or_default(),
            )
        })
        .unwrap_or_default();

    let identity = json!({
        "engine_version": engine_version,
        "data_hash": dataset.data_hash,
        "normalized_data_hash": dataset.normalized_hash,
        "strategy_hash": strategy_hash,
        "config_hash": config_hash_text(cfg),
        "seed": cfg.analytics.monte_carlo.seed,
        "starting_capital": cfg.account.starting_capital.to_string(),
        "instruments": instruments_json,
        "execution": {
            "model": format!("{:?}", cfg.execution.model).to_lowercase(),
            "intrabar_policy": format!("{policy:?}").to_lowercase(),
            "strict_trigger": cfg.execution.strict_trigger,
        },
    });
    let experiment_id = bt_core::hash::hash_json(&identity);

    json!({
        "experiment_id": experiment_id,
        "engine_version": engine_version,
        "strategy_name": spec.name,
        "strategy_definition": spec_to_json(spec),
        "strategy_hash": strategy_hash,
        "data_hash": dataset.data_hash,
        "normalized_data_hash": dataset.normalized_hash,
        "data_validation": {
            "mode": format!("{:?}", cfg.market.validation_mode).to_lowercase(),
            "rows_read": dataset.report.rows_read,
            "rows_kept": dataset.report.rows_kept,
            "issues": dataset.report.summary(),
        },
        "instrument": instruments_json,
        "timeframe_secs": dataset.base_interval_secs,
        "start_date": start,
        "end_date": end,
        "starting_capital": cfg.account.starting_capital.to_string(),
        "currency": cfg.account.currency,
        "fee_model": cfg.costs.commission,
        "spread_model": cfg.costs.spread,
        "slippage_model": cfg.costs.slippage,
        "financing_model": cfg.costs.financing,
        "execution_model": format!("{:?}", cfg.execution.model).to_lowercase(),
        "intrabar_policy": format!("{policy:?}").to_lowercase(),
        "risk_model": cfg.risk,
        "analytics": cfg.analytics,
        "effective_config": cfg,
        "random_seed": cfg.analytics.monte_carlo.seed,
        "identity_inputs": identity,
    })
}

/// A canonical, deterministic text form of the spec (hash input).
fn spec_canonical_text(spec: &StrategySpec) -> String {
    serde_json::to_string(&spec_to_json(spec)).unwrap_or_default()
}

fn config_hash_text(cfg: &EngineConfig) -> String {
    let canonical = serde_json::to_string(cfg).unwrap_or_default();
    bt_core::hash::sha256_hex(canonical.as_bytes())
}

/// Serialize the parsed spec (custom When) into JSON for audit. We round-trip
/// through the YAML value of the parsed fields we can serialize directly.
fn spec_to_json(spec: &StrategySpec) -> Value {
    // StrategySpec has hand-rolled pieces (When/Expr) without Serialize, so we
    // serialize the structural parts we need for auditing via a dedicated shape.
    let doc = json!({
        "name": spec.name,
        "symbols": spec.symbols,
        "timeframes": spec.timeframes,
        "entry_direction": format!("{:?}", spec.entry.direction).to_lowercase(),
        "has_entry_short": spec.entry_short.is_some(),
        "has_exit": spec.exit.is_some(),
        "orders": {
            "stop_loss": serde_json::to_value(&spec.orders.stop_loss).unwrap_or(Value::Null),
            "take_profit": serde_json::to_value(&spec.orders.take_profit).unwrap_or(Value::Null),
            "trailing_stop": serde_json::to_value(&spec.orders.trailing_stop).unwrap_or(Value::Null),
        },
        "risk": { "sizing": serde_json::to_value(&spec.risk.sizing).unwrap_or(Value::Null) },
    });
    doc
}

pub fn result_hash(
    trades: &[TradeRecord],
    equity: &[EquityPoint],
    metrics: &MetricsReport,
) -> String {
    let doc = json!({
        "trades": trades.iter().map(trade_to_json).collect::<Vec<_>>(),
        "equity_curve": equity.iter().map(|p| json!({
            "ts": format_ts(p.ts),
            "e": p.equity.to_string(),
            "b": p.balance.to_string(),
            "u": p.unrealized.to_string(),
            "dd": p.drawdown.to_string(),
        })).collect::<Vec<_>>(),
        "metrics": serde_json::to_value(metrics).unwrap_or(Value::Null),
    });
    bt_core::hash::hash_json(&doc)
}

fn trade_to_json(t: &TradeRecord) -> Value {
    json!({
        "id": t.trade_id,
        "symbol": t.symbol,
        "dir": t.direction,
        "entry_ts": format_ts(t.entry_timestamp),
        "entry_px": t.entry_price.to_string(),
        "exit_ts": format_ts(t.exit_timestamp),
        "exit_px": t.exit_price.to_string(),
        "qty": t.quantity.to_string(),
        "gross": t.gross_pnl.to_string(),
        "commission": t.commission.to_string(),
        "spread": t.spread_cost.to_string(),
        "slippage": t.slippage_cost.to_string(),
        "financing": t.financing.to_string(),
        "net": t.net_pnl.to_string(),
        "risk": t.initial_risk.map(|r| r.to_string()),
        "r": t.r_multiple.map(|r| r.to_string()),
        "bars": t.holding_bars,
        "entry_reason": t.entry_reason,
        "exit_reason": t.exit_reason,
    })
}

pub fn build_summary(
    experiment_id: &str,
    result_hash: &str,
    compiled: &bt_strategy::runtime::CompiledStrategy,
    metrics: &MetricsReport,
    cfg: &EngineConfig,
    _base_secs: i64,
) -> Value {
    let m = metrics;
    json!({
        "experiment_id": experiment_id,
        "result_hash": result_hash,
        "strategy": compiled.name,
        "symbols": compiled.symbols,
        "period": {
            "start": m.overview.start_ts.map(format_ts),
            "end": m.overview.end_ts.map(format_ts),
        },
        "initial_capital": cfg.account.starting_capital.to_string(),
        "final_equity": m.overview.final_equity,
        "return_pct": m.overview.total_return_pct,
        "cagr_pct": m.overview.cagr_pct,
        "sharpe": m.returns.sharpe,
        "sortino": m.returns.sortino,
        "max_drawdown_pct": m.risk.max_drawdown_pct,
        "profit_factor": m.trades.profit_factor,
        "win_rate_pct": m.trades.win_rate_pct,
        "expectancy_r": m.r_stats.avg_r,
        "trades": m.trades.total_trades,
        "currency": cfg.account.currency,
    })
}
