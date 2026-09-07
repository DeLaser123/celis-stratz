//! Shared run helpers for the harness: execute one backtest and summarize it
//! into registry rows. Every harness feature (sweep, walk-forward, stress)
//! goes through here so registry records stay uniform.

use crate::registry::{RegistryRun, RunKind};
use bt_core::error::CoreResult;
use bt_core::time::format_ts;
use bt_data::Dataset;
use bt_simulation::config::EngineConfig;
use bt_simulation::engine::{RunResult, SimulationEngine};
use bt_strategy::spec::StrategySpec;

/// Execute one backtest in-process (events collected, not persisted unless
/// the caller writes them).
pub fn execute(
    dataset: &Dataset,
    spec: &StrategySpec,
    cfg: &EngineConfig,
) -> CoreResult<RunResult> {
    let mut sink = bt_core::event::VecSink::new();
    let signals = std::collections::BTreeMap::new();
    SimulationEngine::run(dataset, spec, cfg, &signals, &mut sink)
}

/// Build a registry row from a completed run.
#[allow(clippy::too_many_arguments)]
pub fn registry_row(
    run: &RunResult,
    kind: RunKind,
    parent_id: Option<i64>,
    label: &str,
    strategy_version: &str,
    out_dir: Option<&std::path::Path>,
    params_json: &str,
) -> RegistryRun {
    let m = &run.metrics;
    let symbols = run
        .summary
        .get("symbols")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    RegistryRun {
        id: 0,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        experiment_id: run.experiment_id.clone(),
        result_hash: run.result_hash.clone(),
        kind,
        parent_id,
        label: label.to_string(),
        strategy_name: run
            .summary
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        strategy_version: strategy_version.to_string(),
        symbols,
        timeframe_secs: run
            .experiment
            .get("timeframe_secs")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        data_hash: run
            .experiment
            .get("data_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        start_ts: m.overview.start_ts.map(format_ts).unwrap_or_default(),
        end_ts: m.overview.end_ts.map(format_ts).unwrap_or_default(),
        initial_capital: format!("{:.2}", m.overview.initial_capital),
        final_equity: m.overview.final_equity,
        return_pct: m.overview.total_return_pct.unwrap_or(0.0),
        sharpe: m.returns.sharpe,
        sortino: m.returns.sortino,
        max_dd_pct: m.risk.max_drawdown_pct,
        profit_factor: m.trades.profit_factor,
        win_rate_pct: m.trades.win_rate_pct,
        trades: m.trades.total_trades as i64,
        out_dir: out_dir.map(|p| p.display().to_string()).unwrap_or_default(),
        params_json: params_json.to_string(),
    }
}
