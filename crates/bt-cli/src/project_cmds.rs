//! Project-aware run resolution and registry persistence for the CLI.

use bt_core::error::{CoreError, CoreResult};
use bt_core::time::Ts;
use bt_core::D;
use bt_data::Dataset;
use bt_harness::project::Project;
use bt_harness::registry::Registry;
use bt_simulation::config::EngineConfig;
use bt_simulation::engine::RunResult;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a run uses, resolved from explicit flags or the project contract.
pub struct Resolved {
    pub data_path: PathBuf,
    pub strategy_path: PathBuf,
    pub strategy_version: String,
    pub config: EngineConfig,
    pub signals: BTreeMap<String, Vec<(Ts, D)>>,
    pub project: Option<Project>,
}

/// Resolve inputs from flags falling back to the folder contract.
pub fn resolve_inputs(
    data: Option<&Path>,
    strategy: Option<&Path>,
    config: Option<&Path>,
    signals: Option<&Path>,
    strategy_version: Option<&str>,
) -> CoreResult<Resolved> {
    let project = Project::open_current().ok();

    // Data
    let data_path: PathBuf = match data {
        Some(p) => p.to_path_buf(),
        None => {
            let p = project.as_ref().ok_or_else(|| {
                CoreError::InvalidData("no --data given and not inside a celis project".into())
            })?;
            let files = p.data_files()?;
            match files.len() {
                0 => {
                    return Err(CoreError::InvalidData(
                        "no --data given and Data/ contains no OHLCV csv".into(),
                    ))
                }
                1 => files[0].path.clone(),
                _ => {
                    let names: Vec<String> =
                        files.iter().map(|f| f.path.display().to_string()).collect();
                    return Err(CoreError::InvalidData(format!(
                        "Data/ has {} datasets — pick one with --data: {}",
                        files.len(),
                        names.join(", ")
                    )));
                }
            }
        }
    };

    // Strategy
    let strategy_path: PathBuf = match strategy {
        Some(p) => p.to_path_buf(),
        None => {
            let p = project.as_ref().ok_or_else(|| {
                CoreError::InvalidData("no --strategy given and not inside a celis project".into())
            })?;
            let version = p.resolve_strategy(strategy_version)?;
            let spec = version.path.join("strategy.yaml");
            if !spec.exists() {
                return Err(CoreError::InvalidData(format!(
                    "{} has no strategy.yaml — compile it first (`backtest ai compile`) or write it manually",
                    version.path.display()
                )));
            }
            spec
        }
    };
    let strategy_version_name = match project.as_ref() {
        Some(p) => p
            .resolve_strategy(strategy_version)
            .map(|v| v.name)
            .unwrap_or_else(|_| "external".to_string()),
        None => "external".to_string(),
    };

    // Config (explicit > version config.yaml > defaults)
    let config = match config {
        Some(p) => load_config_file(p)?,
        None => {
            let version_cfg = project
                .as_ref()
                .and_then(|p| p.resolve_strategy(strategy_version).ok())
                .map(|v| v.path.join("config.yaml"))
                .filter(|p| p.exists());
            match version_cfg {
                Some(p) => load_config_file(&p)?,
                None => EngineConfig::default(),
            }
        }
    };

    // Signals (explicit > merged Trades/signals_*.csv)
    let signals = match signals {
        Some(p) => load_signals_file(p)?,
        None => match &project {
            Some(p) => {
                let mut merged: BTreeMap<String, Vec<(Ts, D)>> = BTreeMap::new();
                for f in p.signal_files()? {
                    for (k, v) in load_signals_file(&f)? {
                        merged.entry(k).or_default().extend(v);
                    }
                }
                for v in merged.values_mut() {
                    v.sort_by_key(|(ts, _)| *ts);
                }
                merged
            }
            None => BTreeMap::new(),
        },
    };

    Ok(Resolved {
        data_path,
        strategy_path,
        strategy_version: strategy_version_name,
        config,
        signals,
        project,
    })
}

pub fn load_config_file(path: &Path) -> CoreResult<EngineConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
    if text.trim().is_empty() {
        return Ok(EngineConfig::default());
    }
    EngineConfig::parse(&text)
}

pub fn load_signals_file(path: &Path) -> CoreResult<BTreeMap<String, Vec<(Ts, D)>>> {
    let mut out: BTreeMap<String, Vec<(Ts, D)>> = BTreeMap::new();
    let text = std::fs::read_to_string(path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| CoreError::InvalidData(format!("{}: {e}", path.display())))?
        .clone();
    let header_names: Vec<String> = headers.iter().map(|h| h.trim().to_string()).collect();
    let idx = |name: &str| header_names.iter().position(|h| h == name);
    let (Some(ts_i), Some(key_i), Some(val_i)) = (idx("timestamp"), idx("key"), idx("value"))
    else {
        return Err(CoreError::InvalidData(format!(
            "{}: signals CSV requires columns timestamp,key,value",
            path.display()
        )));
    };
    for rec in reader.records() {
        let rec: csv::StringRecord =
            rec.map_err(|e| CoreError::InvalidData(format!("{}: {e}", path.display())))?;
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ts = bt_core::time::parse_timestamp(rec.get(ts_i).unwrap_or(""), utc, "signals")?;
        let key = rec.get(key_i).unwrap_or("").trim().to_string();
        let val: D = rec
            .get(val_i)
            .unwrap_or("")
            .trim()
            .parse()
            .map_err(|_| CoreError::InvalidData("signals value must be decimal".into()))?;
        out.entry(key).or_default().push((ts, val));
    }
    for v in out.values_mut() {
        v.sort_by_key(|(ts, _)| *ts);
    }
    Ok(out)
}

/// Load the full dataset for a resolved run (timezone/config from config).
pub fn load_dataset(data_path: &Path, cfg: &EngineConfig) -> CoreResult<Dataset> {
    let tz: chrono_tz::Tz = cfg.market.timezone.parse().map_err(|_| {
        CoreError::ConfigError(format!("unknown timezone '{}'", cfg.market.timezone))
    })?;
    let expected = match &cfg.market.timeframe {
        Some(tf) => Some(bt_core::time::parse_interval(tf)?),
        None => None,
    };
    let mut dataset = match data_path.extension().and_then(|e| e.to_str()) {
        Some("parquet") => {
            bt_data::parquet_io::load_bars_parquet(data_path, tz, cfg.limits, expected)?
        }
        _ => bt_data::csv::load_bars_csv(
            data_path,
            tz,
            cfg.market.timestamp_convention,
            cfg.market.validation_mode,
            cfg.limits,
            expected,
        )?,
    };
    // Optional corporate-actions side-file next to the data file.
    let actions_path = data_path.with_file_name("corporate_actions.csv");
    if actions_path.exists() {
        let actions = bt_data::actions::load_corporate_actions_csv(&actions_path, tz)?;
        dataset.actions.extend(actions);
        dataset
            .actions
            .sort_by(|a, b| (a.ts, &a.symbol).cmp(&(b.ts, &b.symbol)));
    }
    Ok(dataset)
}

/// Parse a strategy spec from a path.
pub fn load_strategy(strategy_path: &Path) -> CoreResult<bt_strategy::spec::StrategySpec> {
    let text = std::fs::read_to_string(strategy_path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", strategy_path.display())))?;
    bt_strategy::spec::parse_spec(&text).map_err(CoreError::StrategyError)
}

/// Metadata describing how a run should be recorded.
pub struct RunRecord<'a> {
    pub kind: bt_harness::registry::RunKind,
    pub parent_id: Option<i64>,
    pub label: &'a str,
    pub strategy_version: &'a str,
    pub params_json: &'a str,
}

/// Persist a run inside a project: artifacts + event log + registry row.
/// Returns the output directory. Outside a project this is a no-op.
pub fn persist_run(
    project: &Project,
    run: &RunResult,
    events: &[bt_core::event::EventRecord],
    record: &RunRecord<'_>,
    registry: &Registry,
) -> CoreResult<PathBuf> {
    let short = &run.experiment_id[..16.min(run.experiment_id.len())];
    let out = project.results_dir().join(short);
    std::fs::create_dir_all(&out)?;
    bt_simulation::outputs::write_outputs(&out, run)?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out.join("event_log.jsonl"))?);
    use std::io::Write;
    for e in events {
        writeln!(w, "{}", serde_json::to_string(e).unwrap_or_default())?;
    }
    w.flush().map_err(CoreError::Io)?;
    let _ = registry.insert(&bt_harness::registry::RegistryRun {
        id: 0,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        experiment_id: run.experiment_id.clone(),
        result_hash: run.result_hash.clone(),
        kind: record.kind,
        parent_id: record.parent_id,
        label: record.label.to_string(),
        strategy_name: run
            .summary
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        strategy_version: record.strategy_version.to_string(),
        symbols: run
            .summary
            .get("symbols")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default(),
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
        start_ts: run
            .metrics
            .overview
            .start_ts
            .map(bt_core::time::format_ts)
            .unwrap_or_default(),
        end_ts: run
            .metrics
            .overview
            .end_ts
            .map(bt_core::time::format_ts)
            .unwrap_or_default(),
        initial_capital: format!("{:.2}", run.metrics.overview.initial_capital),
        final_equity: run.metrics.overview.final_equity,
        return_pct: run.metrics.overview.total_return_pct.unwrap_or(0.0),
        sharpe: run.metrics.returns.sharpe,
        sortino: run.metrics.returns.sortino,
        max_dd_pct: run.metrics.risk.max_drawdown_pct,
        profit_factor: run.metrics.trades.profit_factor,
        win_rate_pct: run.metrics.trades.win_rate_pct,
        trades: run.metrics.trades.total_trades as i64,
        out_dir: out.display().to_string(),
        params_json: record.params_json.to_string(),
    });
    Ok(out)
}

/// Execute a run collecting its events (for persistence).
pub fn execute_with_events(
    dataset: &Dataset,
    spec: &bt_strategy::spec::StrategySpec,
    cfg: &EngineConfig,
    signals: &BTreeMap<String, Vec<(Ts, D)>>,
) -> CoreResult<(RunResult, Vec<bt_core::event::EventRecord>)> {
    let mut sink = bt_core::event::VecSink::new();
    let run = bt_simulation::SimulationEngine::run(dataset, spec, cfg, signals, &mut sink)?;
    Ok((run, sink.events))
}
