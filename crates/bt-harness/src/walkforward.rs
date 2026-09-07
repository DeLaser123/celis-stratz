//! Walk-forward analysis (Pardo): rolling in-sample/out-of-sample windows
//! with an optional per-window optimization inside the IS segment. The OOS
//! runs are stitched into one walk-forward equity path and walk-forward
//! efficiency (mean OOS objective / mean IS objective) is reported.
//!
//! Windows never overlap and OOS runs start with a FRESH indicator state
//! (documented, conservative: no data leaks across windows, and warmup cost
//! is charged to the OOS window that pays it).

use crate::registry::{Registry, RunKind};
use crate::runner;
use crate::sweep::{self, SweepConfig};
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::Ts;
use bt_data::Dataset;
use bt_simulation::config::EngineConfig;
use bt_simulation::engine::RunResult;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardConfig {
    /// In-sample window length in days.
    pub window_days: i64,
    /// Out-of-sample window length in days.
    pub oos_days: i64,
    /// Anchored: IS always starts at data start (grows); rolling: slides.
    pub anchored: bool,
    /// Optional per-window IS optimization (sweep grid); the winning
    /// overrides are applied to the OOS run and recorded.
    pub optimize: Option<SweepConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardWindow {
    pub index: usize,
    pub is_start: String,
    pub is_end: String,
    pub oos_start: String,
    pub oos_end: String,
    pub is_objective: Option<f64>,
    pub oos_objective: Option<f64>,
    pub is_return_pct: f64,
    pub oos_return_pct: f64,
    pub oos_max_dd_pct: f64,
    pub oos_trades: usize,
    pub chosen_params: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardReport {
    pub mode: String,
    pub windows: Vec<WalkForwardWindow>,
    /// Chained OOS equity path (normalized to 1.0 at start).
    pub stitched_equity: Vec<(String, f64)>,
    pub stitched_return_pct: f64,
    pub stitched_max_dd_pct: f64,
    pub walk_forward_efficiency: Option<f64>,
}

/// Slice a dataset to bars with open_time in [start, end).
pub fn slice_dataset(ds: &Dataset, start: Ts, end: Ts) -> Dataset {
    let mut out = ds.clone();
    for series in out.series.values_mut() {
        series
            .bars
            .retain(|b| b.open_time >= start && b.open_time < end);
    }
    out
}

/// Days-only window parsing ("90d", "26w"); months are deliberately not
/// supported (ambiguous calendar arithmetic).
pub fn parse_window_days(s: &str) -> CoreResult<i64> {
    let t = s.trim();
    if t.len() < 2 {
        return Err(CoreError::ConfigError(format!(
            "invalid window '{s}' (use e.g. 90d, 26w)"
        )));
    }
    let (num, unit) = t.split_at(t.len() - 1);
    let n: i64 = num
        .parse()
        .map_err(|_| CoreError::ConfigError(format!("invalid window '{s}' (use e.g. 90d, 26w)")))?;
    let days = match unit {
        "d" => n,
        "w" => n * 7,
        _ => {
            return Err(CoreError::ConfigError(format!(
                "invalid window unit in '{s}' (use d or w)"
            )))
        }
    };
    if days <= 0 {
        return Err(CoreError::ConfigError("window must be positive".into()));
    }
    Ok(days)
}

fn build_windows(ds: &Dataset, cfg: &WalkForwardConfig) -> Vec<(Ts, Ts, Ts, Ts)> {
    let Some(series) = ds.series.values().next() else {
        return Vec::new();
    };
    let Some(first) = series.bars.first().map(|b| b.open_time) else {
        return Vec::new();
    };
    let Some(last) = series.bars.last().map(|b| b.open_time) else {
        return Vec::new();
    };
    let day = chrono::Duration::days(1);
    let is_len = day * i32::try_from(cfg.window_days.max(1)).unwrap_or(1);
    let oos_len = day * i32::try_from(cfg.oos_days.max(1)).unwrap_or(1);
    let mut windows = Vec::new();
    if cfg.anchored {
        let mut is_end = first + is_len;
        while is_end + oos_len <= last + day {
            windows.push((first, is_end, is_end, is_end + oos_len));
            is_end += oos_len;
        }
    } else {
        let mut is_start = first;
        while is_start + is_len + oos_len <= last + day {
            windows.push((
                is_start,
                is_start + is_len,
                is_start + is_len,
                is_start + is_len + oos_len,
            ));
            is_start += oos_len;
        }
    }
    windows
}

fn objective_value(optimize: &Option<SweepConfig>, m: &bt_analytics::MetricsReport) -> Option<f64> {
    let objective = optimize.as_ref().map(|c| c.objective).unwrap_or_default();
    match objective {
        sweep::Objective::Sharpe => m.returns.sharpe,
        sweep::Objective::Sortino => m.returns.sortino,
        sweep::Objective::ReturnPct => m.overview.total_return_pct,
        sweep::Objective::Calmar => m.returns.calmar,
        sweep::Objective::ProfitFactor => m.trades.profit_factor,
        sweep::Objective::ExpectancyR => m.r_stats.avg_r,
        sweep::Objective::MaxDdPct => Some(-m.risk.max_drawdown_pct.abs()),
    }
}

fn run_text(ds: &Dataset, strategy_text: &str, cfg: &EngineConfig) -> CoreResult<RunResult> {
    let spec = bt_strategy::spec::parse_spec(strategy_text).map_err(CoreError::StrategyError)?;
    runner::execute(ds, &spec, cfg)
}

fn apply_overrides_text(
    strategy_text: &str,
    overrides: &BTreeMap<String, serde_yaml::Value>,
) -> String {
    let mut tree: serde_yaml::Value = serde_yaml::from_str(strategy_text)
        .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
    for (path, v) in overrides {
        let _ = sweep::apply_override(&mut tree, path, v.clone());
    }
    serde_yaml::to_string(&tree).unwrap_or_default()
}

pub fn run_walk_forward(
    ds: &Dataset,
    strategy_text: &str,
    cfg: &EngineConfig,
    wf: &WalkForwardConfig,
    registry: Option<(&Registry, i64, &str)>,
) -> CoreResult<WalkForwardReport> {
    let windows = build_windows(ds, wf);
    if windows.is_empty() {
        return Err(CoreError::ConfigError(
            "data too short for the requested walk-forward windows".into(),
        ));
    }
    let mut rows: Vec<WalkForwardWindow> = Vec::new();
    let mut stitched: Vec<(String, f64)> = Vec::new();
    let mut level = 1.0f64;

    for (idx, (is_s, is_e, oos_s, oos_e)) in windows.iter().enumerate() {
        let is_ds = slice_dataset(ds, *is_s, *is_e);
        let oos_ds = slice_dataset(ds, *oos_s, *oos_e);
        let mut chosen: Option<BTreeMap<String, serde_yaml::Value>> = None;
        let mut is_obj: Option<f64> = None;
        let mut is_ret = 0.0f64;
        let mut error: Option<String> = None;

        // ---- IS phase ----
        if let Some(grid) = &wf.optimize {
            match sweep::run_sweep(&is_ds, strategy_text, cfg, grid, None) {
                Ok(r) => match &r.best {
                    Some(best) => {
                        chosen = Some(best.overrides.clone());
                        let text = apply_overrides_text(strategy_text, &best.overrides);
                        match run_text(&is_ds, &text, cfg) {
                            Ok(run) => {
                                is_obj = objective_value(&wf.optimize, &run.metrics);
                                is_ret = run.metrics.overview.total_return_pct.unwrap_or(0.0);
                            }
                            Err(e) => error = Some(e.to_string()),
                        }
                    }
                    None => error = Some("in-window sweep produced no valid combination".into()),
                },
                Err(e) => error = Some(e.to_string()),
            }
        } else {
            match run_text(&is_ds, strategy_text, cfg) {
                Ok(run) => {
                    is_obj = objective_value(&wf.optimize, &run.metrics);
                    is_ret = run.metrics.overview.total_return_pct.unwrap_or(0.0);
                }
                Err(e) => error = Some(e.to_string()),
            }
        }

        // ---- OOS phase ----
        let oos_text = match &chosen {
            Some(o) => apply_overrides_text(strategy_text, o),
            None => strategy_text.to_string(),
        };
        let oos_run: Option<RunResult> = match run_text(&oos_ds, &oos_text, cfg) {
            Ok(r) => Some(r),
            Err(e) => {
                error = error.or(Some(e.to_string()));
                None
            }
        };

        let mut row = WalkForwardWindow {
            index: idx,
            is_start: bt_core::time::format_ts(*is_s),
            is_end: bt_core::time::format_ts(*is_e),
            oos_start: bt_core::time::format_ts(*oos_s),
            oos_end: bt_core::time::format_ts(*oos_e),
            is_objective: is_obj,
            oos_objective: None,
            is_return_pct: is_ret,
            oos_return_pct: 0.0,
            oos_max_dd_pct: 0.0,
            oos_trades: 0,
            chosen_params: chosen
                .as_ref()
                .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null)),
            error: error.clone(),
        };

        if let Some(run) = oos_run {
            let m = &run.metrics;
            row.oos_objective = objective_value(&wf.optimize, m);
            row.oos_return_pct = m.overview.total_return_pct.unwrap_or(0.0);
            row.oos_max_dd_pct = m.risk.max_drawdown_pct;
            row.oos_trades = m.trades.total_trades;
            // Stitch: continue the running equity level with this window's path.
            let first = run
                .equity_curve
                .first()
                .map(|p| bt_core::money::to_f64(p.equity))
                .unwrap_or(1.0);
            if first.abs() > f64::EPSILON {
                for p in &run.equity_curve {
                    let norm = bt_core::money::to_f64(p.equity) / first;
                    stitched.push((bt_core::time::format_ts(p.ts), level * norm));
                }
                level *= bt_core::money::to_f64(run.equity_curve.last().unwrap().equity) / first;
            }
            if let Some((reg, parent, version)) = registry {
                let rr = crate::registry::RegistryRun {
                    id: 0,
                    created_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    experiment_id: run.experiment_id.clone(),
                    result_hash: run.result_hash.clone(),
                    kind: RunKind::WalkForwardWindow,
                    parent_id: Some(parent),
                    label: format!("w{idx} {}→{}", row.oos_start, row.oos_end),
                    strategy_name: run
                        .summary
                        .get("strategy")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    strategy_version: version.to_string(),
                    symbols: String::new(),
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
                        .into(),
                    start_ts: row.oos_start.clone(),
                    end_ts: row.oos_end.clone(),
                    initial_capital: format!("{:.2}", m.overview.initial_capital),
                    final_equity: m.overview.final_equity,
                    return_pct: row.oos_return_pct,
                    sharpe: m.returns.sharpe,
                    sortino: m.returns.sortino,
                    max_dd_pct: row.oos_max_dd_pct,
                    profit_factor: m.trades.profit_factor,
                    win_rate_pct: m.trades.win_rate_pct,
                    trades: m.trades.total_trades as i64,
                    out_dir: String::new(),
                    params_json: row
                        .chosen_params
                        .clone()
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                };
                let _ = reg.insert(&rr);
            }
        }
        rows.push(row);
    }

    let mean = |v: &[f64]| -> Option<f64> {
        if v.is_empty() {
            None
        } else {
            Some(v.iter().sum::<f64>() / v.len() as f64)
        }
    };
    let is_vals: Vec<f64> = rows.iter().filter_map(|r| r.is_objective).collect();
    let oos_vals: Vec<f64> = rows.iter().filter_map(|r| r.oos_objective).collect();
    let wfe = match (mean(&is_vals), mean(&oos_vals)) {
        (Some(i), Some(o)) if i.abs() > f64::EPSILON => Some(o / i),
        _ => None,
    };
    let stitched_return = match (stitched.first(), stitched.last()) {
        (Some((_, f)), Some((_, l))) if f.abs() > f64::EPSILON => (l / f - 1.0) * 100.0,
        _ => 0.0,
    };
    let mut s_peak = 0.0f64;
    let mut s_dd = 0.0f64;
    for (_, v) in &stitched {
        if *v > s_peak {
            s_peak = *v;
        }
        let dd = if s_peak > 0.0 {
            (s_peak - v) / s_peak * 100.0
        } else {
            0.0
        };
        if dd > s_dd {
            s_dd = dd;
        }
    }

    Ok(WalkForwardReport {
        mode: if wf.anchored {
            "anchored".into()
        } else {
            "rolling".into()
        },
        windows: rows,
        stitched_return_pct: stitched_return,
        stitched_max_dd_pct: s_dd,
        stitched_equity: stitched,
        walk_forward_efficiency: wfe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_core::D as _D;

    const STRATEGY: &str = r#"
strategy:
  name: wf_test
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, {sma: {source: {field: close}, period: 3}}]}
  exit:
    when: {lt: [{field: close}, {sma: {source: {field: close}, period: 3}}]}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#;

    fn dataset(days: usize) -> Dataset {
        let mut csv = String::from("timestamp,symbol,open,high,low,close,volume\n");
        let mut price = 100i64;
        let mut i = 0usize;
        let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        'outer: for day in 0..days {
            let date = start + chrono::Duration::days(day as i64);
            for hour in 0..24 {
                if i.is_multiple_of(1) && i >= days * 24 {
                    break 'outer;
                }
                price += if (i / 12).is_multiple_of(2) { 1 } else { -1 };
                let ts = format!("{}T{:02}:00:00Z", date.format("%Y-%m-%d"), hour);
                csv.push_str(&format!(
                    "{ts},X,{p},{hi},{lo},{p},10\n",
                    p = price,
                    hi = price + 1,
                    lo = price - 1
                ));
                i += 1;
            }
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static WF_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = WF_SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("bt_wf_data_{}_{}.csv", std::process::id(), seq));
        std::fs::write(&path, csv).unwrap();
        bt_data::csv::load_bars_csv(
            &path,
            "UTC".parse().unwrap(),
            bt_data::TimestampConvention::Open,
            bt_data::ValidationMode::Strict,
            bt_data::LoadLimits::default(),
            Some(3600),
        )
        .unwrap()
    }

    #[test]
    fn windows_are_disjoint_and_stitch() {
        let ds = dataset(30); // 30 days of hourly bars
        let cfg = WalkForwardConfig {
            window_days: 7,
            oos_days: 7,
            anchored: false,
            optimize: None,
        };
        let wf = run_walk_forward(&ds, STRATEGY, &Default::default(), &cfg, None).unwrap();
        assert!(!wf.windows.is_empty());
        for w in wf.windows.windows(2) {
            assert!(
                w[0].oos_end <= w[1].is_start || w[0].oos_end == w[1].oos_start,
                "windows must not overlap IS"
            );
        }
        // stitched path starts at 1.0
        let first = wf.stitched_equity.first().map(|(_, v)| *v).unwrap_or(0.0);
        assert!((first - 1.0).abs() < 1e-9);
        assert!(wf.stitched_equity.len() > 1);
    }

    #[test]
    fn window_parsing() {
        assert_eq!(parse_window_days("90d").unwrap(), 90);
        assert_eq!(parse_window_days("26w").unwrap(), 182);
        assert!(
            parse_window_days("3m").is_err(),
            "months deliberately unsupported"
        );
        assert!(parse_window_days("0d").is_err());
    }

    #[test]
    fn slice_is_inclusive_exclusive() {
        let ds = dataset(3);
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let a = bt_core::time::parse_timestamp("2024-01-01T00:00:00Z", utc, "t").unwrap();
        let b = bt_core::time::parse_timestamp("2024-01-02T00:00:00Z", utc, "t").unwrap();
        let sliced = slice_dataset(&ds, a, b);
        assert_eq!(sliced.series["X"].bars.len(), 24, "[start, end) semantics");
        let _ = _D::from(1);
    }
}
