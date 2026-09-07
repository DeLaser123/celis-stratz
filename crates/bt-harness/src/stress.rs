//! Stress engine (spec §23's "correct engine first" — this is the stress
//! layer on top of it).
//!
//! A scenario is a **deterministic, versioned transform of the input data
//! plus an optional cost overlay**. The simulation engine is never modified:
//! every stressed run is just the kernel executed on transformed inputs, so
//! it inherits every kernel guarantee (determinism, Decimal ledger, audit
//! trail) and gets its own scenario hash recorded next to the run.
//!
//! Transforms:
//! - `gap_shock`  — revalues all bars from the window start by (1 + pct);
//!   an optional `drift_pct_per_bar` compounds on top per subsequent bar.
//! - `vol_regime` — expands each bar's range around its midpoint by `mult`
//!   (open/close preserved; rejects negative lows loudly).
//! - `liquidity_drought` — multiplies volume (thin-market proxy).
//! - `drift` — compounds a per-bar drift across the window.
//!
//! Cost overlays adjust the cost stack for the run: spread multiplier /
//! absolute spread, additive slippage (bps), financing replacement.

use crate::registry::{Registry, RunKind};
use crate::runner;
use crate::sweep::{self, SweepConfig};
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::{format_ts, parse_timestamp, Ts};
use bt_core::D;
use bt_data::{Bar, BarSeries, Dataset};
use bt_execution::costs::{CostModels, FinancingModel, SlippageModel, SpreadModel};
use bt_simulation::config::EngineConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Scenario specification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioSpec {
    #[serde(default = "default_scenario_name")]
    pub name: String,
    /// Time window the scenario applies to. Defaults to the whole dataset.
    #[serde(default)]
    pub window: Option<Window>,
    #[serde(default)]
    pub transforms: Vec<Transform>,
    #[serde(default)]
    pub costs: CostOverlay,
}

fn default_scenario_name() -> String {
    "scenario".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Window {
    /// Explicit UTC bounds (inclusive start, exclusive end).
    Range { start: String, end: String },
    /// Named historical stress window applied to the client's data.
    Preset(String),
}

/// Historical stress packs: fixed UTC date ranges applied to client data.
/// If the data does not cover a preset, the scenario fails loudly.
pub fn historical_preset(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "gfc_2008" => Some(("2008-09-01T00:00:00Z", "2009-03-31T23:59:59Z")),
        "flash_crash_2010" => Some(("2010-05-01T00:00:00Z", "2010-05-31T23:59:59Z")),
        "chf_depeg_2015" => Some(("2015-01-14T00:00:00Z", "2015-01-31T23:59:59Z")),
        "volmageddon_2018" => Some(("2018-02-01T00:00:00Z", "2018-02-28T23:59:59Z")),
        "covid_2020" => Some(("2020-02-15T00:00:00Z", "2020-04-30T23:59:59Z")),
        "rates_2022" => Some(("2022-01-01T00:00:00Z", "2022-10-31T23:59:59Z")),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transform {
    GapShock {
        /// Revaluation applied at the window start (+5 = +5%).
        pct: D,
        /// Optional compounding drift per bar after the gap.
        #[serde(default)]
        drift_pct_per_bar: D,
        /// Optional symbol filter; empty/absent = all symbols.
        #[serde(default)]
        symbols: Vec<String>,
    },
    VolRegime {
        /// Range expansion multiplier around each bar's midpoint (1.0 = unchanged).
        mult: D,
        #[serde(default)]
        symbols: Vec<String>,
    },
    LiquidityDrought {
        /// Volume multiplier (0.5 = half liquidity).
        volume_mult: D,
        #[serde(default)]
        symbols: Vec<String>,
    },
    Drift {
        /// Compounding per-bar drift across the window.
        pct_per_bar: D,
        #[serde(default)]
        symbols: Vec<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostOverlay {
    /// Multiply an existing fixed spread (error if spread model is zero).
    #[serde(default)]
    pub spread_mult: Option<D>,
    /// Set an absolute fixed spread (overrides).
    #[serde(default)]
    pub spread_fixed: Option<D>,
    /// Additive slippage in basis points (onto zero or percentage models).
    #[serde(default)]
    pub slippage_bps_add: Option<D>,
    /// Replace financing with a daily-rate model.
    #[serde(default)]
    pub financing_daily_rate: Option<FinancingRates>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancingRates {
    pub long: D,
    pub short: D,
}

impl ScenarioSpec {
    pub fn parse(text: &str) -> CoreResult<ScenarioSpec> {
        if let Ok(wrapped) = serde_yaml::from_str::<ScenarioWrapper>(text) {
            return Ok(wrapped.scenario);
        }
        serde_yaml::from_str(text)
            .map_err(|e| CoreError::ConfigError(format!("scenario parse: {e}")))
    }

    pub fn load(path: &Path) -> CoreResult<ScenarioSpec> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
        ScenarioSpec::parse(&text)
    }

    /// Deterministic scenario identity (canonical JSON hash).
    pub fn hash(&self) -> String {
        bt_core::hash::hash_json(&serde_json::to_value(self).unwrap_or_default())
    }
}

#[derive(serde::Deserialize)]
struct ScenarioWrapper {
    scenario: ScenarioSpec,
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

/// Resolve the scenario window to concrete [start, end) bounds against the
/// dataset. Presets that do not intersect the data fail loudly.
pub fn resolve_window(ds: &Dataset, window: &Option<Window>) -> CoreResult<(Ts, Ts)> {
    let data_start = ds
        .series
        .values()
        .filter_map(|s| s.bars.first().map(|b| b.open_time))
        .min()
        .unwrap_or(Ts::UNIX_EPOCH);
    let data_end = ds
        .series
        .values()
        .filter_map(|s| s.bars.last().map(|b| b.close_time(s.interval_secs)))
        .max()
        .unwrap_or(Ts::UNIX_EPOCH);

    let (raw_start, raw_end, label): (String, String, String) = match window {
        None => (
            data_start.to_rfc3339(),
            data_end.to_rfc3339(),
            "full dataset".into(),
        ),
        Some(Window::Range { start, end }) => {
            (start.clone(), end.clone(), format!("{start}..{end}"))
        }
        Some(Window::Preset(p)) => match historical_preset(p) {
            Some((s, e)) => (s.to_string(), e.to_string(), format!("preset {p}")),
            None => {
                return Err(CoreError::ConfigError(format!(
                    "unknown historical preset '{p}' (available: gfc_2008, flash_crash_2010, \
                     chf_depeg_2015, volmageddon_2018, covid_2020, rates_2022)"
                )))
            }
        },
    };

    let utc: chrono_tz::Tz = "UTC"
        .parse()
        .map_err(|_| CoreError::ConfigError("internal: UTC tz".into()))?;
    let start = parse_timestamp(&raw_start, utc, &format!("scenario window {label}"))?;
    let end = parse_timestamp(&raw_end, utc, &format!("scenario window {label}"))?;
    if end <= start {
        return Err(CoreError::ConfigError(format!(
            "scenario window {label}: end <= start"
        )));
    }
    // Clamp to data; fail when there is no intersection at all.
    let clamped_start = start.max(data_start);
    let clamped_end = end.min(data_end);
    if clamped_end <= clamped_start {
        return Err(CoreError::InvalidData(format!(
            "scenario window {label} does not intersect the dataset \
             (data spans {} .. {})",
            format_ts(data_start),
            format_ts(data_end)
        )));
    }
    Ok((clamped_start, clamped_end))
}

fn symbol_selected(symbols: &[String], symbol: &str) -> bool {
    symbols.is_empty() || symbols.iter().any(|s| s == symbol)
}

/// Apply all transforms + the cost overlay. Returns the transformed dataset
/// and the adjusted cost models. The original dataset is never mutated.
pub fn apply_scenario(
    ds: &Dataset,
    spec: &ScenarioSpec,
    base_costs: &CostModels,
) -> CoreResult<(Dataset, CostModels)> {
    let (start, end) = resolve_window(ds, &spec.window)?;
    let mut out = ds.clone();
    for t in &spec.transforms {
        match t {
            Transform::GapShock {
                pct,
                drift_pct_per_bar,
                symbols,
            } => {
                for series in out.series.values_mut() {
                    if !symbol_selected(symbols, &series.symbol) {
                        continue;
                    }
                    let symbol = series.symbol.clone();
                    apply_to_window(series, start, end, |i, bar| {
                        let gap = D::ONE + *pct / D::from(100);
                        let factor = if i == 0 {
                            gap
                        } else {
                            bt_core::money::d_mul(
                                gap,
                                bt_core::money::d_powi(
                                    D::ONE + *drift_pct_per_bar / D::from(100),
                                    i as u64,
                                    "gap drift",
                                )?,
                                "gap factor",
                            )?
                        };
                        scale_bar(bar, factor, &symbol)
                    })?;
                }
            }
            Transform::VolRegime { mult, symbols } => {
                for series in out.series.values_mut() {
                    if !symbol_selected(symbols, &series.symbol) {
                        continue;
                    }
                    let symbol = series.symbol.clone();
                    apply_to_window(series, start, end, |_i, bar| {
                        expand_range(bar, *mult, &symbol)
                    })?;
                }
            }
            Transform::LiquidityDrought {
                volume_mult,
                symbols,
            } => {
                for series in out.series.values_mut() {
                    if !symbol_selected(symbols, &series.symbol) {
                        continue;
                    }
                    apply_to_window(series, start, end, |_i, bar| {
                        if let Some(v) = bar.volume.as_mut() {
                            *v = bt_core::money::d_mul(*v, *volume_mult, "volume")?;
                        }
                        Ok(())
                    })?;
                }
            }
            Transform::Drift {
                pct_per_bar,
                symbols,
            } => {
                for series in out.series.values_mut() {
                    if !symbol_selected(symbols, &series.symbol) {
                        continue;
                    }
                    let symbol = series.symbol.clone();
                    apply_to_window(series, start, end, |i, bar| {
                        let factor = bt_core::money::d_powi(
                            D::ONE + *pct_per_bar / D::from(100),
                            (i + 1) as u64,
                            "drift",
                        )?;
                        scale_bar(bar, factor, &symbol)
                    })?;
                }
            }
        }
    }
    validate(&out)?;

    let costs = apply_cost_overlay(base_costs, &spec.costs)?;
    Ok((out, costs))
}

/// Apply a per-bar function to bars whose open_time is in [start, end).
fn apply_to_window(
    series: &mut BarSeries,
    start: Ts,
    end: Ts,
    f: impl Fn(usize, &mut Bar) -> CoreResult<()>,
) -> CoreResult<()> {
    let mut i = 0usize;
    for bar in series.bars.iter_mut() {
        if bar.open_time >= start && bar.open_time < end {
            f(i, bar)?;
            i += 1;
        }
    }
    Ok(())
}

fn scale_bar(bar: &mut Bar, factor: D, symbol: &str) -> CoreResult<()> {
    bar.open = bt_core::money::d_mul(bar.open, factor, "gap open")?;
    bar.high = bt_core::money::d_mul(bar.high, factor, "gap high")?;
    bar.low = bt_core::money::d_mul(bar.low, factor, "gap low")?;
    bar.close = bt_core::money::d_mul(bar.close, factor, "gap close")?;
    if bar.low <= D::ZERO {
        return Err(CoreError::InvalidData(format!(
            "scenario transform produces non-positive price for {symbol} at {}: low={}",
            format_ts(bar.open_time),
            bar.low
        )));
    }
    Ok(())
}

fn expand_range(bar: &mut Bar, mult: D, symbol: &str) -> CoreResult<()> {
    let mid = (bar.high + bar.low) / D::from(2);
    bar.high = mid + (bar.high - mid) * mult;
    bar.low = mid + (bar.low - mid) * mult;
    if bar.low <= D::ZERO {
        return Err(CoreError::InvalidData(format!(
            "vol_regime mult {mult} produces non-positive price for {symbol} at {}: low={}",
            format_ts(bar.open_time),
            bar.low
        )));
    }
    Ok(())
}

/// Full OHLC validation of a transformed dataset (never silently clamped).
fn validate(ds: &Dataset) -> CoreResult<()> {
    for series in ds.series.values() {
        for bar in &series.bars {
            bt_data::validate::check_ohlc(bar.open, bar.high, bar.low, bar.close).map_err(
                |detail| {
                    CoreError::InvalidData(format!(
                        "scenario produced invalid OHLC for {} at {}: {detail}",
                        series.symbol,
                        format_ts(bar.open_time)
                    ))
                },
            )?;
            if bar.open <= D::ZERO || bar.close <= D::ZERO {
                return Err(CoreError::InvalidData(format!(
                    "scenario produced non-positive price for {} at {}",
                    series.symbol,
                    format_ts(bar.open_time)
                )));
            }
        }
    }
    Ok(())
}

fn apply_cost_overlay(base: &CostModels, overlay: &CostOverlay) -> CoreResult<CostModels> {
    let mut costs = base.clone();

    if let Some(fixed) = overlay.spread_fixed {
        costs.spread = SpreadModel::Fixed { spread: fixed };
    } else if let Some(mult) = overlay.spread_mult {
        match &costs.spread {
            SpreadModel::Fixed { spread } => {
                costs.spread = SpreadModel::Fixed {
                    spread: bt_core::money::d_mul(*spread, mult, "spread_mult")?,
                };
            }
            SpreadModel::Zero => {
                return Err(CoreError::ConfigError(
                    "scenario sets spread_mult but the base spread model is zero — \
                     use spread_fixed instead"
                        .into(),
                ))
            }
        }
    }

    if let Some(add_bps) = overlay.slippage_bps_add {
        match &costs.slippage {
            SlippageModel::Zero => {
                costs.slippage = SlippageModel::Percentage { bps: add_bps };
            }
            SlippageModel::Percentage { bps } => {
                costs.slippage = SlippageModel::Percentage {
                    bps: bt_core::money::d_add(*bps, add_bps, "slippage_bps_add")?,
                };
            }
            SlippageModel::Fixed { .. } | SlippageModel::SquareRootImpact { .. } => {
                return Err(CoreError::ConfigError(
                    "scenario sets slippage_bps_add but the base slippage model is fixed or \
                     impact-based — switch to a percentage slippage model first"
                        .into(),
                ))
            }
        }
    }

    if let Some(r) = &overlay.financing_daily_rate {
        costs.financing = FinancingModel::DailyRate {
            long: r.long,
            short: r.short,
        };
    }

    Ok(costs)
}

// ---------------------------------------------------------------------------
// Stress runner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct StressRow {
    pub scenario: String,
    pub scenario_hash: String,
    pub perturbation: String,
    pub return_pct: f64,
    pub final_equity: f64,
    pub max_dd_pct: f64,
    pub sharpe: Option<f64>,
    pub trades: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StressReport {
    pub stress_id: String,
    pub baseline: Option<StressRow>,
    pub rows: Vec<StressRow>,
    /// Worst non-error row by return.
    pub worst: Option<StressRow>,
    pub scenario_hashes: Vec<(String, String)>,
}

/// Registry payload for one executed stress cell (inserted serially after
/// the parallel phase — rusqlite is !Sync).
struct CellPayload {
    experiment_id: String,
    result_hash: String,
    data_hash: String,
    return_pct: f64,
    final_equity: f64,
    sharpe: Option<f64>,
    max_dd_pct: f64,
    trades: i64,
}

/// One stress cell = (scenario, perturbation combination). Executed across
/// rayon workers as fully independent kernel runs.
#[allow(clippy::too_many_arguments)]
pub fn run_stress(
    ds: &Dataset,
    strategy_text: &str,
    cfg: &EngineConfig,
    scenarios: &[ScenarioSpec],
    perturbation: &BTreeMap<String, Vec<serde_yaml::Value>>,
    registry: Option<(&Registry, i64, &str)>,
) -> CoreResult<StressReport> {
    use rayon::prelude::*;

    if scenarios.is_empty() {
        return Err(CoreError::ConfigError(
            "no scenarios given — add --scenario files or a --scenario-dir".into(),
        ));
    }

    // Deterministic stress identity: config + sorted scenario hashes + grid.
    let mut scenario_hashes: Vec<(String, String)> = scenarios
        .iter()
        .map(|s| (s.name.clone(), s.hash()))
        .collect();
    scenario_hashes.sort_by(|a, b| a.1.cmp(&b.1));
    let perturb_json = serde_json::to_value(perturbation).unwrap_or_default();
    let stress_id = bt_core::hash::hash_json(&serde_json::json!({
        "config_hash": bt_core::hash::sha256_hex(
            serde_json::to_string(cfg).unwrap_or_default().as_bytes(),
        ),
        "scenarios": scenario_hashes,
        "perturbation": perturb_json,
        "engine_version": env!("CARGO_PKG_VERSION"),
    }));

    // Cell list: (scenario, combo label, overrides) + the baseline cell.
    let combos = sweep::combinations(&SweepConfig {
        mode: "grid".into(),
        grid: perturbation.clone(),
        ..Default::default()
    });
    let mut cells: Vec<(
        Option<&ScenarioSpec>,
        String,
        BTreeMap<String, serde_yaml::Value>,
    )> = Vec::new();
    cells.push((None, "none".into(), BTreeMap::new()));
    for combo in &combos {
        cells.push((None, perturbation_label(combo), combo.clone()));
    }
    for s in scenarios {
        cells.push((Some(s), "none".into(), BTreeMap::new()));
        for combo in &combos {
            cells.push((Some(s), perturbation_label(combo), combo.clone()));
        }
    }

    let strategy_text = strategy_text.to_string();
    let cfg = cfg.clone();
    let ds = ds.clone();

    let results: Vec<(StressRow, Option<CellPayload>)> = cells
        .into_par_iter()
        .map(|(scenario, label, overrides)| {
            let scenario_hash = scenario.map(|s| s.hash()).unwrap_or_default();
            let outcome: CoreResult<(Dataset, CostModels)> = match scenario {
                None => Ok((ds.clone(), cfg.costs.clone())),
                Some(s) => apply_scenario(&ds, s, &cfg.costs),
            };
            let (transformed, costs) = match outcome {
                Ok(x) => x,
                Err(e) => {
                    return (
                        StressRow {
                            scenario: scenario
                                .map(|s| s.name.clone())
                                .unwrap_or_else(|| "baseline".into()),
                            scenario_hash,
                            perturbation: label,
                            return_pct: 0.0,
                            final_equity: 0.0,
                            max_dd_pct: 0.0,
                            sharpe: None,
                            trades: 0,
                            error: Some(e.to_string()),
                        },
                        None,
                    )
                }
            };
            let mut run_cfg = cfg.clone();
            run_cfg.costs = costs;

            let spec = bt_strategy::spec::parse_spec(&apply_overrides(&strategy_text, &overrides))
                .map_err(CoreError::StrategyError)
                .and_then(|spec| runner::execute(&transformed, &spec, &run_cfg));

            match spec {
                Ok(run) => {
                    let m = &run.metrics;
                    (
                        StressRow {
                            scenario: scenario
                                .map(|s| s.name.clone())
                                .unwrap_or_else(|| "baseline".into()),
                            scenario_hash,
                            perturbation: label,
                            return_pct: m.overview.total_return_pct.unwrap_or(0.0),
                            final_equity: m.overview.final_equity,
                            max_dd_pct: m.risk.max_drawdown_pct,
                            sharpe: m.returns.sharpe,
                            trades: m.trades.total_trades,
                            error: None,
                        },
                        Some(CellPayload {
                            experiment_id: run.experiment_id.clone(),
                            result_hash: run.result_hash.clone(),
                            data_hash: transformed.normalized_hash(),
                            return_pct: m.overview.total_return_pct.unwrap_or(0.0),
                            final_equity: m.overview.final_equity,
                            sharpe: m.returns.sharpe,
                            max_dd_pct: m.risk.max_drawdown_pct,
                            trades: m.trades.total_trades as i64,
                        }),
                    )
                }
                Err(e) => (
                    StressRow {
                        scenario: scenario
                            .map(|s| s.name.clone())
                            .unwrap_or_else(|| "baseline".into()),
                        scenario_hash,
                        perturbation: label,
                        return_pct: 0.0,
                        final_equity: 0.0,
                        max_dd_pct: 0.0,
                        sharpe: None,
                        trades: 0,
                        error: Some(e.to_string()),
                    },
                    None,
                ),
            }
        })
        .collect();

    // Serial registry inserts (rusqlite is !Sync).
    if let Some((reg, parent, version)) = registry {
        for (row, cell) in &results {
            if let Some(c) = cell {
                let rr = crate::registry::RegistryRun {
                    id: 0,
                    created_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    experiment_id: c.experiment_id.clone(),
                    result_hash: c.result_hash.clone(),
                    kind: RunKind::StressCell,
                    parent_id: Some(parent),
                    label: format!("{} | {}", row.scenario, row.perturbation),
                    strategy_name: String::new(),
                    strategy_version: version.to_string(),
                    symbols: String::new(),
                    timeframe_secs: 0,
                    data_hash: c.data_hash.clone(),
                    start_ts: String::new(),
                    end_ts: String::new(),
                    initial_capital: String::new(),
                    final_equity: c.final_equity,
                    return_pct: c.return_pct,
                    sharpe: c.sharpe,
                    sortino: None,
                    max_dd_pct: c.max_dd_pct,
                    profit_factor: None,
                    win_rate_pct: None,
                    trades: c.trades,
                    out_dir: String::new(),
                    params_json: serde_json::json!({
                        "scenario": row.scenario,
                        "scenario_hash": row.scenario_hash,
                        "perturbation": row.perturbation,
                    })
                    .to_string(),
                };
                let _ = reg.insert(&rr);
            }
        }
    }

    let baseline: Option<StressRow> = results
        .iter()
        .find(|item| item.0.scenario == "baseline" && item.0.error.is_none())
        .map(|item| item.0.clone());
    // The unperturbed baseline is reported separately; perturbation-only
    // cells stay in the rows (their scenario field is also "baseline" but
    // their perturbation label is not "none").
    let mut rows: Vec<StressRow> = results
        .into_iter()
        .map(|(r, _)| r)
        .filter(|r| !(r.scenario == "baseline" && r.perturbation == "none" && r.error.is_none()))
        .collect();
    rows.sort_by(|a, b| {
        a.return_pct
            .partial_cmp(&b.return_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let worst = rows.iter().find(|r| r.error.is_none()).cloned();

    Ok(StressReport {
        stress_id,
        baseline,
        rows,
        worst,
        scenario_hashes,
    })
}

fn perturbation_label(combo: &BTreeMap<String, serde_yaml::Value>) -> String {
    if combo.is_empty() {
        return "none".into();
    }
    combo
        .iter()
        .map(|(k, v)| {
            let vs = match v {
                serde_yaml::Value::String(s) => s.clone(),
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

fn apply_overrides(strategy_text: &str, overrides: &BTreeMap<String, serde_yaml::Value>) -> String {
    let full: serde_yaml::Value = serde_yaml::from_str(strategy_text)
        .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
    // Override paths are relative to the strategy root: unwrap the
    // {strategy: ...} document wrapper when present. A failed override is an
    // ERROR, never silent (a silently-ignored perturbation would fake
    // robustness results).
    let has_wrapper = matches!(&full, serde_yaml::Value::Mapping(m)
        if m.contains_key(serde_yaml::Value::String("strategy".into())));
    let mut doc = if has_wrapper {
        full.clone()
    } else {
        serde_yaml::Value::Mapping(
            [(serde_yaml::Value::String("strategy".into()), full)]
                .into_iter()
                .collect(),
        )
    };
    for (path, v) in overrides {
        let root = doc
            .get_mut(serde_yaml::Value::String("strategy".into()))
            .expect("strategy key exists");
        sweep::apply_override(root, path, v.clone())
            .unwrap_or_else(|e| panic!("stress perturbation '{path}' invalid: {e}"));
    }
    serde_yaml::to_string(&doc).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const DATA: &str = "timestamp,symbol,open,high,low,close,volume\n";
    const STRATEGY: &str = r#"
strategy:
  name: stress_test
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, {sma: {source: {field: close}, period: 3}}]}
  orders:
    stop_loss: {type: fixed_distance, value: 2}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#;

    fn dataset(n: usize) -> Dataset {
        let mut csv = String::from(DATA);
        let mut price = 100i64;
        for i in 0..n {
            price += if (i / 5) % 2 == 0 { 1 } else { -1 };
            let ts = format!("2024-01-{:02}T{:02}:00:00Z", 1 + i / 24, i % 24);
            csv.push_str(&format!(
                "{ts},X,{p},{hi},{lo},{p},10\n",
                p = price,
                hi = price + 1,
                lo = price - 1
            ));
        }
        use std::sync::atomic::{AtomicU64, Ordering as AOrd};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, AOrd::SeqCst);
        let path =
            std::env::temp_dir().join(format!("bt_stress_data_{}_{}.csv", std::process::id(), seq));
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
    fn gap_shock_scales_and_preserves_ohlc() {
        let ds = dataset(48);
        let spec = ScenarioSpec {
            name: "crash".into(),
            window: None,
            transforms: vec![Transform::GapShock {
                pct: dec!(-5),
                drift_pct_per_bar: dec!(0),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let (out, _) = apply_scenario(&ds, &spec, &CostModels::default()).unwrap();
        let base = &ds.series["X"].bars;
        let shocked = &out.series["X"].bars;
        assert_eq!(shocked[0].open, base[0].open * dec!(0.95));
        assert_eq!(shocked[30].close, base[30].close * dec!(0.95));
        assert!(out.normalized_hash() != ds.normalized_hash());
    }

    #[test]
    fn gap_shock_with_drift_compounds() {
        let ds = dataset(48);
        let spec = ScenarioSpec {
            name: "crash_drift".into(),
            window: None,
            transforms: vec![Transform::GapShock {
                pct: dec!(-10),
                drift_pct_per_bar: dec!(-1),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let (out, _) = apply_scenario(&ds, &spec, &CostModels::default()).unwrap();
        let base = &ds.series["X"].bars;
        let shocked = &out.series["X"].bars;
        assert_eq!(shocked[1].open, base[1].open * dec!(0.90) * dec!(0.99));
        let two = bt_core::money::d_powi(dec!(0.99), 2, "t").unwrap();
        assert_eq!(shocked[2].open, base[2].open * dec!(0.90) * two);
    }

    #[test]
    fn vol_regime_expands_range_keeps_open_close() {
        let ds = dataset(48);
        let spec = ScenarioSpec {
            name: "vol2x".into(),
            window: None,
            transforms: vec![Transform::VolRegime {
                mult: dec!(2),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let (out, _) = apply_scenario(&ds, &spec, &CostModels::default()).unwrap();
        let base = &ds.series["X"].bars;
        let v = &out.series["X"].bars;
        assert_eq!(v[10].open, base[10].open, "open preserved");
        assert_eq!(v[10].close, base[10].close, "close preserved");
        let base_mid = (base[10].high + base[10].low) / dec!(2);
        let new_mid = (v[10].high + v[10].low) / dec!(2);
        assert_eq!(base_mid, new_mid, "midpoint preserved");
        assert!(
            v[10].high - v[10].low > base[10].high - base[10].low,
            "range expanded"
        );
    }

    #[test]
    fn vol_regime_rejects_negative_low() {
        let ds = dataset(48);
        let spec = ScenarioSpec {
            name: "vol_extreme".into(),
            window: None,
            transforms: vec![Transform::VolRegime {
                mult: dec!(100000),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let err = apply_scenario(&ds, &spec, &CostModels::default()).unwrap_err();
        assert!(err.to_string().contains("non-positive"), "got {err}");
    }

    #[test]
    fn liquidity_drought_multiplies_volume() {
        let ds = dataset(48);
        let spec = ScenarioSpec {
            name: "drought".into(),
            window: None,
            transforms: vec![Transform::LiquidityDrought {
                volume_mult: dec!(0.5),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let (out, _) = apply_scenario(&ds, &spec, &CostModels::default()).unwrap();
        assert_eq!(out.series["X"].bars[5].volume, Some(dec!(5)));
    }

    #[test]
    fn window_limits_transform() {
        // 72 bars = Jan 1..Jan 3; the window covers only Jan 2.
        let ds = dataset(72);
        let spec = ScenarioSpec {
            name: "partial".into(),
            window: Some(Window::Range {
                start: "2024-01-02T00:00:00Z".into(),
                end: "2024-01-03T00:00:00Z".into(),
            }),
            transforms: vec![Transform::GapShock {
                pct: dec!(-50),
                drift_pct_per_bar: dec!(0),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let (out, _) = apply_scenario(&ds, &spec, &CostModels::default()).unwrap();
        let base = &ds.series["X"].bars;
        let shocked = &out.series["X"].bars;
        assert_eq!(shocked[0].open, base[0].open, "before window untouched");
        assert_eq!(
            shocked[30].open,
            base[30].open * dec!(0.5),
            "inside window scaled"
        );
        assert_eq!(
            shocked[47].open,
            base[47].open * dec!(0.5),
            "last window bar scaled"
        );
        assert_eq!(shocked[48].open, base[48].open, "after window untouched");
        assert_eq!(shocked[71].open, base[71].open, "end of data untouched");
    }

    #[test]
    fn window_without_intersection_fails_loudly() {
        let ds = dataset(48);
        let spec = ScenarioSpec {
            name: "no_overlap".into(),
            window: Some(Window::Preset("gfc_2008".into())),
            transforms: vec![],
            costs: CostOverlay::default(),
        };
        let err = apply_scenario(&ds, &spec, &CostModels::default()).unwrap_err();
        assert!(err.to_string().contains("does not intersect"), "got {err}");
    }

    #[test]
    fn preset_resolution() {
        assert!(historical_preset("gfc_2008").is_some());
        assert!(historical_preset("covid_2020").is_some());
        assert!(historical_preset("unknown").is_none());
    }

    #[test]
    fn cost_overlays() {
        let base = CostModels {
            spread: SpreadModel::Fixed { spread: dec!(2) },
            slippage: SlippageModel::Percentage { bps: dec!(1) },
            financing: FinancingModel::Zero,
            commission: Default::default(),
        };
        let overlay = CostOverlay {
            spread_mult: Some(dec!(3)),
            spread_fixed: None,
            slippage_bps_add: Some(dec!(2)),
            financing_daily_rate: Some(FinancingRates {
                long: dec!(0.0001),
                short: dec!(-0.0001),
            }),
        };
        let out = apply_cost_overlay(&base, &overlay).unwrap();
        match out.spread {
            SpreadModel::Fixed { spread } => assert_eq!(spread, dec!(6)),
            other => panic!("unexpected spread {other:?}"),
        }
        match out.slippage {
            SlippageModel::Percentage { bps } => assert_eq!(bps, dec!(3)),
            other => panic!("unexpected slippage {other:?}"),
        }
        match out.financing {
            FinancingModel::DailyRate { long, short } => {
                assert_eq!(long, dec!(0.0001));
                assert_eq!(short, dec!(-0.0001));
            }
            other => panic!("unexpected financing {other:?}"),
        }
        let zero_base = CostModels::default();
        let bad = CostOverlay {
            spread_mult: Some(dec!(2)),
            ..Default::default()
        };
        assert!(apply_cost_overlay(&zero_base, &bad).is_err());
    }

    #[test]
    fn scenario_hash_is_deterministic_and_content_bound() {
        let a = ScenarioSpec {
            name: "x".into(),
            window: None,
            transforms: vec![Transform::GapShock {
                pct: dec!(1),
                drift_pct_per_bar: dec!(0),
                symbols: vec![],
            }],
            costs: CostOverlay::default(),
        };
        let b = a.clone();
        assert_eq!(a.hash(), b.hash());
        let c = ScenarioSpec {
            transforms: vec![Transform::GapShock {
                pct: dec!(2),
                drift_pct_per_bar: dec!(0),
                symbols: vec![],
            }],
            ..a.clone()
        };
        assert_ne!(a.hash(), c.hash());
    }

    #[test]
    fn stress_matrix_runs_and_is_deterministic() {
        let ds = dataset(80);
        let cfg = EngineConfig::default();
        let scenarios = vec![
            ScenarioSpec {
                name: "crash".into(),
                window: None,
                transforms: vec![Transform::GapShock {
                    pct: dec!(-2),
                    drift_pct_per_bar: dec!(0),
                    symbols: vec![],
                }],
                costs: CostOverlay::default(),
            },
            ScenarioSpec {
                name: "vol_spike".into(),
                window: None,
                transforms: vec![Transform::VolRegime {
                    mult: dec!(1.5),
                    symbols: vec![],
                }],
                costs: CostOverlay::default(),
            },
        ];
        let mut perturb: BTreeMap<String, Vec<serde_yaml::Value>> = BTreeMap::new();
        perturb.insert(
            "risk.sizing.qty".into(),
            vec![
                serde_yaml::from_str("1").unwrap(),
                serde_yaml::from_str("2").unwrap(),
            ],
        );
        let a = run_stress(&ds, STRATEGY, &cfg, &scenarios, &perturb, None).unwrap();
        let b = run_stress(&ds, STRATEGY, &cfg, &scenarios, &perturb, None).unwrap();
        assert_eq!(a.stress_id, b.stress_id, "stress identity deterministic");
        // 2 perturbation cells + 2 scenarios x (1 scenario + 2 perturbed) = 8 rows
        // (the unperturbed baseline is reported separately)
        assert_eq!(a.rows.len(), 2 + 2 * 3);
        assert!(a.baseline.is_some());
        assert!(a.worst.is_some());
        let sa = serde_json::to_string(&a.rows).unwrap();
        let sb = serde_json::to_string(&b.rows).unwrap();
        assert_eq!(sa, sb, "row set must be byte-identical across runs");
        for w in a.rows.windows(2) {
            assert!(
                w[0].return_pct <= w[1].return_pct,
                "rows sorted worst-first"
            );
        }
        assert_eq!(a.scenario_hashes.len(), 2);
    }

    #[test]
    fn stress_cell_with_costs_differs_from_baseline() {
        let ds = dataset(80);
        let cfg = EngineConfig::default();
        let scenarios = vec![ScenarioSpec {
            name: "wide_market".into(),
            window: None,
            transforms: vec![],
            costs: CostOverlay {
                spread_fixed: Some(dec!(1)),
                ..Default::default()
            },
        }];
        let report = run_stress(&ds, STRATEGY, &cfg, &scenarios, &BTreeMap::new(), None).unwrap();
        let base = report.baseline.as_ref().unwrap();
        let stressed = report
            .rows
            .iter()
            .find(|r| r.scenario == "wide_market")
            .unwrap();
        assert!(
            stressed.return_pct < base.return_pct,
            "stressed {} vs base {}",
            stressed.return_pct,
            base.return_pct
        );
    }
}
