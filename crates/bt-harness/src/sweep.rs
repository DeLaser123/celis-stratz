//! Parameter sweeps (spec §23): deterministic grid / seeded random search
//! over strategy-spec overrides, executed in parallel with rayon across
//! *independent* runs (never inside one simulation).
//!
//! Overrides are YAML paths into the strategy document, e.g.
//! `risk.sizing.value: [0.5, 1.0, 2.0]` or `orders.stop_loss.value: [30, 50]`.
//! The kernel never changes: every combination is just a different spec.

use crate::registry::{Registry, RegistryRun, RunKind};
use crate::runner;
use bt_analytics::MetricsReport;
use bt_core::error::{CoreError, CoreResult};
use bt_core::prng::SplitMix64;
use bt_data::Dataset;
use bt_simulation::config::EngineConfig;
use bt_simulation::engine::RunResult;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as Y;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    #[default]
    Sharpe,
    Sortino,
    ReturnPct,
    Calmar,
    ProfitFactor,
    ExpectancyR,
    /// Ranked by SMALLEST magnitude (lower drawdown is better).
    MaxDdPct,
}

impl Objective {
    pub fn parse_objective(s: &str) -> CoreResult<Objective> {
        match s {
            "sharpe" => Ok(Objective::Sharpe),
            "sortino" => Ok(Objective::Sortino),
            "return_pct" => Ok(Objective::ReturnPct),
            "calmar" => Ok(Objective::Calmar),
            "profit_factor" => Ok(Objective::ProfitFactor),
            "expectancy_r" => Ok(Objective::ExpectancyR),
            "max_dd_pct" => Ok(Objective::MaxDdPct),
            other => Err(CoreError::ConfigError(format!(
                "unknown objective '{other}' (sharpe|sortino|return_pct|calmar|profit_factor|expectancy_r|max_dd_pct)"
            ))),
        }
    }

    fn value(&self, m: &MetricsReport) -> Option<f64> {
        match self {
            Objective::Sharpe => m.returns.sharpe,
            Objective::Sortino => m.returns.sortino,
            Objective::ReturnPct => m.overview.total_return_pct,
            Objective::Calmar => m.returns.calmar,
            Objective::ProfitFactor => m.trades.profit_factor,
            Objective::ExpectancyR => m.r_stats.avg_r,
            Objective::MaxDdPct => Some(-m.risk.max_drawdown_pct.abs()),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Objective::Sharpe => "sharpe",
            Objective::Sortino => "sortino",
            Objective::ReturnPct => "return_pct",
            Objective::Calmar => "calmar",
            Objective::ProfitFactor => "profit_factor",
            Objective::ExpectancyR => "expectancy_r",
            Objective::MaxDdPct => "max_dd_pct",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepConfig {
    #[serde(default)]
    pub objective: Objective,
    #[serde(default = "default_mode")]
    pub mode: String, // "grid" | "random"
    #[serde(default = "default_samples")]
    pub samples: usize,
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Path -> list of override values (cartesian product in grid mode).
    pub grid: BTreeMap<String, Vec<Y>>,
}

fn default_mode() -> String {
    "grid".into()
}
fn default_samples() -> usize {
    20
}
fn default_seed() -> u64 {
    42
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepRow {
    pub label: String,
    pub overrides: BTreeMap<String, Y>,
    pub objective_value: Option<f64>,
    pub final_equity: f64,
    pub return_pct: f64,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub max_dd_pct: f64,
    pub profit_factor: Option<f64>,
    pub trades: usize,
    pub error: Option<String>,
    pub out_dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepReport {
    pub objective: Objective,
    pub mode: String,
    pub total_combinations: usize,
    pub evaluated: usize,
    pub failed: usize,
    pub rows: Vec<SweepRow>,
    pub best: Option<SweepRow>,
    /// Set when a 1-D numeric sweep shows a non-monotonic objective sequence.
    pub non_monotonic: Option<String>,
}

/// Apply a dotted YAML path override (a.b.c = v; a.list.0 = v). Intermediate
/// mapping keys must exist; the leaf is replaced. Never guessed, never merged.
pub fn apply_override(root: &mut Y, path: &str, value: Y) -> CoreResult<()> {
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (path, None),
    };
    match rest {
        None => set_leaf(root, head, value, path),
        Some(rest) => {
            let child = descend_mut(root, head, path)?;
            apply_override(child, rest, value)
        }
    }
}

fn descend_mut<'a>(cur: &'a mut Y, seg: &str, path: &str) -> CoreResult<&'a mut Y> {
    if let Ok(idx) = seg.parse::<usize>() {
        let seq = cur.as_sequence_mut().ok_or_else(|| {
            CoreError::ConfigError(format!("path '{path}': '{seg}' is not a sequence"))
        })?;
        let n = seq.len();
        seq.get_mut(idx).ok_or_else(|| {
            CoreError::ConfigError(format!(
                "path '{path}': index {idx} out of bounds (len {n})"
            ))
        })
    } else {
        let map = cur.as_mapping_mut().ok_or_else(|| {
            CoreError::ConfigError(format!("path '{path}': '{seg}' is not a mapping"))
        })?;
        let key = Y::String(seg.to_string());
        map.get_mut(&key).ok_or_else(|| {
            CoreError::ConfigError(format!(
                "path '{path}': key '{seg}' does not exist in the strategy document"
            ))
        })
    }
}

fn set_leaf(cur: &mut Y, seg: &str, value: Y, path: &str) -> CoreResult<()> {
    if let Ok(idx) = seg.parse::<usize>() {
        let seq = cur.as_sequence_mut().ok_or_else(|| {
            CoreError::ConfigError(format!("path '{path}': '{seg}' is not a sequence"))
        })?;
        let n = seq.len();
        let slot = seq.get_mut(idx).ok_or_else(|| {
            CoreError::ConfigError(format!(
                "path '{path}': index {idx} out of bounds (len {n})"
            ))
        })?;
        *slot = value;
        Ok(())
    } else {
        let map = cur.as_mapping_mut().ok_or_else(|| {
            CoreError::ConfigError(format!("path '{path}': '{seg}' is not a mapping"))
        })?;
        map.insert(Y::String(seg.to_string()), value);
        Ok(())
    }
}

/// Cartesian product of the grid (deterministic order: params sorted, values
/// in listed order).
pub fn combinations(config: &SweepConfig) -> Vec<BTreeMap<String, Y>> {
    let params: Vec<(&String, &Vec<Y>)> = config.grid.iter().collect();
    let mut combos: Vec<BTreeMap<String, Y>> = vec![BTreeMap::new()];
    for (path, values) in params {
        let mut next = Vec::new();
        for combo in &combos {
            for v in values {
                let mut c = combo.clone();
                c.insert((*path).clone(), v.clone());
                next.push(c);
            }
        }
        combos = next;
    }
    if config.mode == "random" && config.samples < combos.len() {
        let mut rng = SplitMix64::new(config.seed);
        let mut idx: Vec<usize> = (0..combos.len()).collect();
        rng.shuffle(&mut idx);
        idx.truncate(config.samples);
        idx.sort_unstable(); // deterministic reporting order
        idx.into_iter().map(|i| combos[i].clone()).collect()
    } else {
        combos
    }
}

fn label_for(combo: &BTreeMap<String, Y>) -> String {
    combo
        .iter()
        .map(|(k, v)| {
            let vs = match v {
                Y::String(s) => s.clone(),
                other => serde_yaml::to_string(other)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            };
            format!("{k}={vs}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn overrides_to_json(combo: &BTreeMap<String, Y>) -> String {
    let map: serde_json::Map<String, serde_json::Value> = combo
        .iter()
        .map(|(k, v)| (k.clone(), crate::yaml_to_json_value(v)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn run_sweep(
    dataset: &Dataset,
    strategy_text: &str,
    cfg: &EngineConfig,
    sweep: &SweepConfig,
    registry: Option<(&Registry, i64, &str)>, // (registry, parent manifest id, strategy version)
) -> CoreResult<SweepReport> {
    let combos = combinations(sweep);
    if combos.is_empty() {
        return Err(CoreError::ConfigError("sweep grid is empty".into()));
    }
    let dataset = dataset.clone();
    let cfg = cfg.clone();
    let strategy_text = strategy_text.to_string();
    let sweep = sweep.clone();
    let failures = AtomicUsize::new(0);
    let (reg_ref, sweep_parent, sweep_version) = registry
        .map(|(r, p, v)| (Some(r), p, v.to_string()))
        .unwrap_or((None, 0i64, String::new()));

    // Base strategy tree for override application. Override paths are
    // relative to the strategy root, so unwrap the {strategy: ...} wrapper.
    let full_tree: Y = serde_yaml::from_str(&strategy_text)
        .map_err(|e| CoreError::StrategyError(format!("strategy yaml: {e}")))?;
    let base_tree: Y = match &full_tree {
        Y::Mapping(m) if m.contains_key(Y::String("strategy".into())) => full_tree
            .get(Y::String("strategy".into()))
            .cloned()
            .unwrap_or(Y::Mapping(Default::default())),
        _ => full_tree.clone(),
    };

    let results: Vec<(SweepRow, Option<RegistryRun>)> = combos
        .into_par_iter()
        .map(|combo| {
            let label = label_for(&combo);
            let mut tree = base_tree.clone();
            for (path, v) in &combo {
                if let Err(e) = apply_override(&mut tree, path, v.clone()) {
                    failures.fetch_add(1, Ordering::Relaxed);
                    return (
                        SweepRow {
                            label,
                            overrides: combo,
                            objective_value: None,
                            final_equity: 0.0,
                            return_pct: 0.0,
                            sharpe: None,
                            sortino: None,
                            max_dd_pct: 0.0,
                            profit_factor: None,
                            trades: 0,
                            error: Some(e.to_string()),
                            out_dir: String::new(),
                        },
                        None,
                    );
                }
            }
            let text = serde_yaml::to_string(&Y::Mapping(
                [(Y::String("strategy".into()), tree)].into_iter().collect(),
            ))
            .unwrap_or_default();
            let run = match bt_strategy::spec::parse_spec(&text)
                .map_err(CoreError::StrategyError)
                .and_then(|spec| runner::execute(&dataset, &spec, &cfg))
            {
                Ok(run) => run,
                Err(e) => {
                    failures.fetch_add(1, Ordering::Relaxed);
                    return (
                        SweepRow {
                            label,
                            overrides: combo,
                            objective_value: None,
                            final_equity: 0.0,
                            return_pct: 0.0,
                            sharpe: None,
                            sortino: None,
                            max_dd_pct: 0.0,
                            profit_factor: None,
                            trades: 0,
                            error: Some(e.to_string()),
                            out_dir: String::new(),
                        },
                        None,
                    );
                }
            };
            let m = &run.metrics;
            let row = SweepRow {
                label: label.clone(),
                overrides: combo.clone(),
                objective_value: sweep.objective.value(m),
                final_equity: m.overview.final_equity,
                return_pct: m.overview.total_return_pct.unwrap_or(0.0),
                sharpe: m.returns.sharpe,
                sortino: m.returns.sortino,
                max_dd_pct: m.risk.max_drawdown_pct,
                profit_factor: m.trades.profit_factor,
                trades: m.trades.total_trades,
                error: None,
                out_dir: String::new(),
            };
            let reg_run = if sweep_parent > 0 {
                Some(RegistryRun {
                    id: 0,
                    created_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    experiment_id: run.experiment_id.clone(),
                    result_hash: run.result_hash.clone(),
                    kind: RunKind::SweepCell,
                    parent_id: Some(sweep_parent),
                    label: label.clone(),
                    strategy_name: run
                        .summary
                        .get("strategy")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .into(),
                    strategy_version: sweep_version.to_string(),
                    symbols: row_symbols(&run),
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
                    start_ts: String::new(),
                    end_ts: String::new(),
                    initial_capital: format!("{:.2}", m.overview.initial_capital),
                    final_equity: m.overview.final_equity,
                    return_pct: m.overview.total_return_pct.unwrap_or(0.0),
                    sharpe: m.returns.sharpe,
                    sortino: m.returns.sortino,
                    max_dd_pct: m.risk.max_drawdown_pct,
                    profit_factor: m.trades.profit_factor,
                    win_rate_pct: m.trades.win_rate_pct,
                    trades: m.trades.total_trades as i64,
                    out_dir: String::new(),
                    params_json: overrides_to_json(&combo),
                })
            } else {
                None
            };
            (row, reg_run)
        })
        .collect();

    let failed = failures.load(Ordering::Relaxed);
    if let Some(reg) = reg_ref {
        for (_, reg_run) in &results {
            if let Some(rr) = reg_run {
                let _ = reg.insert(rr);
            }
        }
    }
    let mut rows: Vec<SweepRow> = results.into_iter().map(|(r, _)| r).collect();
    rows.sort_by(|a, b| {
        let va = a.objective_value.unwrap_or(f64::NEG_INFINITY);
        let vb = b.objective_value.unwrap_or(f64::NEG_INFINITY);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    let best = rows.iter().find(|r| r.error.is_none()).cloned();

    // Non-monotonicity check for single-parameter numeric sweeps.
    let non_monotonic = check_monotonicity(sweep.objective, &rows);

    Ok(SweepReport {
        objective: sweep.objective,
        mode: sweep.mode.clone(),
        total_combinations: rows.len(),
        evaluated: rows.iter().filter(|r| r.error.is_none()).count(),
        failed,
        rows,
        best,
        non_monotonic,
    })
}

fn row_symbols(run: &RunResult) -> String {
    run.summary
        .get("symbols")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// For a single-parameter sweep with numeric values, detect >1 direction
/// change in the objective sequence (classic overfit smell).
fn check_monotonicity(objective: Objective, rows: &[SweepRow]) -> Option<String> {
    if rows.len() < 3 {
        return None;
    }
    let first_param = rows
        .iter()
        .filter(|r| r.error.is_none())
        .flat_map(|r| r.overrides.keys().next().cloned())
        .next()?;
    let varying = rows
        .iter()
        .all(|r| r.overrides.len() == 1 && r.overrides.contains_key(&first_param));
    if !varying {
        return None;
    }
    let mut points: Vec<(f64, f64)> = rows
        .iter()
        .filter_map(|r| {
            let x = match r.overrides.get(&first_param)? {
                Y::Number(n) => n.as_f64()?,
                Y::String(s) => s.trim().parse().ok()?,
                _ => return None,
            };
            let y = r.objective_value?;
            Some((x, y))
        })
        .collect();
    if points.len() < 3 {
        return None;
    }
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut direction = 0i8;
    let mut changes = 0usize;
    for w in points.windows(2) {
        let d = (w[1].1 - w[0].1).total_cmp(&0.0);
        if d == std::cmp::Ordering::Equal {
            continue;
        }
        let new_dir = if d == std::cmp::Ordering::Greater {
            1i8
        } else {
            -1i8
        };
        if direction != 0 && new_dir != direction {
            changes += 1;
        }
        direction = new_dir;
    }
    if changes > 1 {
        Some(format!(
            "objective '{}' is non-monotonic across '{first_param}' ({changes} direction changes) — \
             treat the best cell with suspicion and run `backtest robust` before trusting it",
            objective.name()
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = "timestamp,symbol,open,high,low,close,volume\n";
    const STRATEGY: &str = r#"
strategy:
  name: t
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, {sma: {source: {field: close}, period: 3}}]}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#;

    fn dataset(n: usize) -> bt_data::Dataset {
        let mut csv = String::from(DATA);
        let mut price = 100i64;
        for i in 0..n {
            price += if (i / 5) % 2 == 0 { 1 } else { -1 };
            let ts = format!("2024-01-{:02}T{:02}:00:00Z", 1 + i / 24, i % 24);
            csv.push_str(&format!(
                "{ts},X,{price},{p2},{p3},{price},10\n",
                price = price,
                p2 = price + 1,
                p3 = price - 1
            ));
        }
        let path = std::env::temp_dir().join(format!("bt_sweep_data_{}.csv", std::process::id()));
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
    fn combinations_cartesian_and_random() {
        let mut grid = BTreeMap::new();
        grid.insert(
            "risk.sizing.qty".to_string(),
            vec![Y::Number(1.into()), Y::Number(2.into())],
        );
        grid.insert(
            "orders.stop_loss.value".to_string(),
            vec![
                Y::Number(1.into()),
                Y::Number(2.into()),
                Y::Number(3.into()),
            ],
        );
        let cfg = SweepConfig {
            mode: "grid".into(),
            grid,
            ..Default::default()
        };
        assert_eq!(combinations(&cfg).len(), 6);

        let mut grid2 = BTreeMap::new();
        grid2.insert(
            "a".to_string(),
            (0..10).map(|i| Y::Number(i.into())).collect(),
        );
        let cfg2 = SweepConfig {
            mode: "random".into(),
            samples: 4,
            seed: 7,
            grid: grid2,
            ..Default::default()
        };
        let c1 = combinations(&cfg2);
        let c2 = combinations(&cfg2);
        assert_eq!(c1.len(), 4);
        assert_eq!(c1, c2, "random sweep sampling must be seeded/deterministic");
    }

    #[test]
    fn apply_override_paths() {
        let mut tree: Y = serde_yaml::from_str("a: {b: {c: 1}, list: [10, 20]}").unwrap();
        apply_override(&mut tree, "a.b.c", Y::Number(9.into())).unwrap();
        apply_override(&mut tree, "a.list.1", Y::Number(99.into())).unwrap();
        let s = serde_yaml::to_string(&tree).unwrap();
        assert!(s.contains("c: 9"));
        assert!(s.contains("- 99"));
        assert!(apply_override(&mut tree, "a.b.x.y", Y::Number(1.into())).is_err());
    }

    #[test]
    fn sweep_runs_and_ranks() {
        let ds = dataset(80);
        let cfg = EngineConfig::default();
        let sweep = SweepConfig {
            objective: Objective::ReturnPct,
            mode: "grid".into(),
            samples: 0,
            seed: 1,
            grid: {
                let mut g = BTreeMap::new();
                g.insert(
                    "risk.sizing.qty".to_string(),
                    vec![Y::Number(1.into()), Y::Number(2.into())],
                );
                g
            },
        };
        let report = run_sweep(&ds, STRATEGY, &cfg, &sweep, None).unwrap();
        assert_eq!(report.evaluated + report.failed, 2);
        assert!(report.best.is_some());
        let worst = report
            .rows
            .last()
            .unwrap()
            .objective_value
            .unwrap_or(f64::NEG_INFINITY);
        assert!(report.rows[0].objective_value.unwrap_or(f64::NEG_INFINITY) >= worst);
    }
}
