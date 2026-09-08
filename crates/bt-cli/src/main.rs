//! `backtest` CLI — the harness shell (spec §29 + enterprise harness).
//! Thin consumer of the kernel crates and the bt-harness layer.

mod ai_cmds;
mod project_cmds;
mod pull_chart;
mod selfupdate;

use bt_core::error::{CoreError, CoreResult};
use bt_core::time::Ts;
use bt_core::D;
use bt_data::Dataset;
use bt_harness::project::Project;
use bt_harness::registry::RunKind;
use bt_simulation::config::EngineConfig;
use bt_simulation::outputs::write_outputs;
use bt_simulation::report::render;
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "stratz",
    version = env!("CARGO_PKG_VERSION"),
    about = "Stratz — deterministic, auditable trading backtesting engine + harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold a stratz project in the current folder.
    Init,
    /// Verify the folder contract of the current project.
    Doctor,
    /// Run a backtest over historical data.
    Run {
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long)]
        strategy: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Strategy version folder (default: latest under Strategy/).
        #[arg(long)]
        strategy_version: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        /// External signals CSV (default: Trades/signals_*.csv in a project).
        #[arg(long)]
        signals: Option<PathBuf>,
        /// Print machine-readable summary JSON to stdout.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Validate a market data CSV (strict unless configured otherwise).
    ValidateData {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Validate a strategy specification: VALID or INVALID with reasons.
    ValidateStrategy {
        #[arg(long)]
        strategy: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Render the human-readable report from a results directory.
    Report {
        #[arg()]
        results: PathBuf,
        /// Also write a self-contained report.html into the results directory.
        #[arg(long, default_value_t = false)]
        html: bool,
    },
    /// Compare two result directories side by side.
    Compare {
        #[arg()]
        a: PathBuf,
        #[arg()]
        b: PathBuf,
    },
    /// Query run artifacts or the registry with read-only SQL.
    Query {
        /// A results directory, a registry .db path, or "registry".
        #[arg()]
        target: String,
        #[arg()]
        sql: String,
    },
    /// Experiment registry operations.
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    /// Parameter sweep (grid or seeded random) with rayon parallelism.
    Sweep {
        /// Sweep grid YAML (objective, mode, grid paths).
        #[arg(long)]
        grid: PathBuf,
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long)]
        strategy: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        strategy_version: Option<String>,
    },
    /// Walk-forward analysis with stitched out-of-sample equity.
    WalkForward {
        /// In-sample window, e.g. 90d or 26w.
        #[arg(long)]
        window: String,
        /// Out-of-sample window, e.g. 30d or 8w.
        #[arg(long)]
        oos: String,
        /// Anchor the IS window at data start (grows) instead of rolling.
        #[arg(long, default_value_t = false)]
        anchored: bool,
        /// Optional sweep grid optimized inside each IS window.
        #[arg(long)]
        optimize: Option<PathBuf>,
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long)]
        strategy: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        strategy_version: Option<String>,
    },
    /// Stress matrix: run the strategy under deterministic scenario
    /// transforms (gap shocks, volatility regimes, liquidity droughts,
    /// cost overlays) plus optional parameter perturbation.
    Stress {
        /// Scenario YAML files (repeatable).
        #[arg(long)]
        scenario: Vec<PathBuf>,
        /// Directory scanned for *.yaml scenario files.
        #[arg(long)]
        scenario_dir: Option<PathBuf>,
        /// Parameter perturbation: path=v1,v2,v3 (repeatable).
        #[arg(long)]
        perturb: Vec<String>,
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long)]
        strategy: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        strategy_version: Option<String>,
    },
    /// Overfitting robustness: Deflated Sharpe + PBO/CSCV across runs.
    Robust {
        /// Result directories (2+), or --registry-tag to select runs.
        #[arg()]
        runs: Vec<PathBuf>,
        #[arg(long)]
        registry_tag: Option<String>,
        /// CSCV blocks (default 16; needs >= that many return periods).
        #[arg(long, default_value_t = 16)]
        blocks: usize,
    },
    /// Re-run Monte Carlo analysis over an existing result directory.
    MonteCarlo {
        #[arg()]
        results: PathBuf,
        #[arg(long, default_value_t = 1000)]
        paths: u32,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Download OHLCV candles from Dukascopy into the project's Data/ folder.
    PullChart {
        /// Instrument symbol, e.g. EURUSD.
        symbol: String,
        /// Timeframe with optional lookback: "5,3" = 5-min, 3 years back.
        /// Suffixes: bare number = minutes; 5m/1h/4h/1d. Lookback: 3 = 3y,
        /// 6mo / 2w / 30d.
        spec: String,
        /// Range start (overrides lookback), e.g. 2022-01-01.
        #[arg(long)]
        from: Option<String>,
        /// Range end (defaults to now), e.g. 2024-06-30.
        #[arg(long)]
        to: Option<String>,
        /// Quote side to download: bid or ask.
        #[arg(long, default_value = "bid")]
        side: String,
        /// Price decimal factor override (default: 5, or 3 for JPY pairs).
        #[arg(long)]
        decimals: Option<u32>,
        /// Output file or directory (default: Data/<SYMBOL>_<tf>.csv in a project).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Update the installed stratz binary (source build or GitHub release).
    SelfUpdate {
        /// Report availability without applying.
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Re-download the latest GitHub release even if not newer.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Turn auto-update off.
        #[arg(long, default_value_t = false)]
        disable: bool,
        /// Turn auto-update back on.
        #[arg(long, default_value_t = false)]
        enable: bool,
        /// Point the dev source marker at this repo path.
        #[arg(long)]
        set_source: Option<PathBuf>,
    },
    /// AI commands (natural-language strategy compilation, grounded analysis).
    Ai {
        #[command(subcommand)]
        cmd: AiCmd,
    },
    /// Print the engine version.
    Version,
}

#[derive(Subcommand)]
enum AiCmd {
    /// Compile Strategy/<v>/strategy.md into strategy.yaml via the LLM,
    /// validated by the engine's own compiler (repair loop included).
    Compile {
        #[arg(long)]
        strategy_version: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        max_tokens: Option<u32>,
        #[arg(long)]
        temperature: Option<f64>,
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        /// Build and hash the prompt without calling the API.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Review a compiled strategy spec for look-ahead/ambiguity/overfit risks.
    Review {
        #[arg(long)]
        strategy_version: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        max_tokens: Option<u32>,
        #[arg(long)]
        temperature: Option<f64>,
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Grounded analysis of a result directory (numbers verified against artifacts).
    Explain {
        #[arg()]
        results: PathBuf,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        max_tokens: Option<u32>,
        #[arg(long)]
        temperature: Option<f64>,
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Ask a question grounded in the project's notes and data profile.
    Ask {
        #[arg()]
        question: String,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        max_tokens: Option<u32>,
        #[arg(long)]
        temperature: Option<f64>,
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Show AI configuration (never prints the key itself).
    Status {
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Inspect the AI audit ledger.
    Ledger {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Save or clear the API key in the OS keyring.
    Login {
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long, default_value_t = false)]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// List recent runs.
    List {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one run with its full lineage.
    Show {
        #[arg()]
        id: i64,
    },
    /// Tag a run (e.g. "best", "candidate").
    Tag {
        #[arg()]
        id: i64,
        #[arg()]
        tag: String,
    },
}

fn main() {
    selfupdate::precheck();
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> CoreResult<()> {
    match cli.command {
        Commands::PullChart {
            symbol,
            spec,
            from,
            to,
            side,
            decimals,
            output,
        } => {
            let args = pull_chart::PullChartArgs {
                symbol: &symbol,
                spec: &spec,
                from: from.as_deref(),
                to: to.as_deref(),
                side: &side,
                decimals,
                output: output.as_ref(),
            };
            pull_chart::cmd(&args)
        }
        Commands::SelfUpdate {
            check,
            force,
            disable,
            enable,
            set_source,
        } => {
            if disable {
                selfupdate::set_disabled(true)?;
            } else if enable {
                selfupdate::set_disabled(false)?;
            } else if let Some(src) = &set_source {
                selfupdate::set_source(src)?;
            } else {
                selfupdate::report_and_apply(force, check)?;
            }
            Ok(())
        }
        Commands::Ai { cmd } => ai_dispatch(cmd),
        Commands::Version => {
            println!("stratz {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Commands::Init => {
            let cwd = std::env::current_dir().map_err(CoreError::Io)?;
            if cwd.join(".stratz").exists() || cwd.join("Strategy").exists() {
                return Err(CoreError::InvalidData(
                    "this folder is already a stratz project".into(),
                ));
            }
            let p = Project::init(&cwd)?;
            println!("stratz project created at {}", p.root.display());
            println!("  Strategy/v1/strategy.md   <- describe your strategy here");
            println!("  Data/                     <- chart data (OHLCV csv)");
            println!("  Trades/                   <- signals_*.csv, history_*.csv");
            println!("  Notes/                    <- markdown context for the AI");
            println!("next: stratz doctor");
            Ok(())
        }
        Commands::Doctor => {
            let p = Project::open_current()?;
            let report = p.doctor()?;
            println!("project: {}", report.root.display());
            for c in &report.checks {
                let mark = match c.status {
                    bt_harness::project::CheckStatus::Ok => "ok  ",
                    bt_harness::project::CheckStatus::Warn => "warn",
                    bt_harness::project::CheckStatus::Error => "ERR ",
                };
                println!("  [{mark}] {:<9} {}", c.name, c.detail);
            }
            if report.is_healthy() {
                println!("healthy");
            } else {
                println!("NOT healthy — fix the errors above");
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Run {
            data,
            strategy,
            config,
            strategy_version,
            out,
            signals,
            json,
        } => {
            let resolved = project_cmds::resolve_inputs(
                data.as_deref(),
                strategy.as_deref(),
                config.as_deref(),
                signals.as_deref(),
                strategy_version.as_deref(),
            )?;
            let dataset = project_cmds::load_dataset(&resolved.data_path, &resolved.config)?;
            let spec = project_cmds::load_strategy(&resolved.strategy_path)?;
            let (run, events) = project_cmds::execute_with_events(
                &dataset,
                &spec,
                &resolved.config,
                &resolved.signals,
            )?;

            // Persist inside a project (artifacts + event log + registry).
            let mut run_dir_note = String::new();
            if let Some(project) = &resolved.project {
                let registry = bt_harness::registry::Registry::open(&project.registry_path())?;
                let dir = project_cmds::persist_run(
                    project,
                    &run,
                    &events,
                    &project_cmds::RunRecord {
                        kind: RunKind::Single,
                        parent_id: None,
                        label: &resolved.strategy_version,
                        strategy_version: &resolved.strategy_version,
                        params_json: "{}",
                    },
                    &registry,
                )?;
                run_dir_note = dir.display().to_string();
            } else if let Some(o) = &out {
                std::fs::create_dir_all(o)?;
                write_outputs(o, &run)?;
                run_dir_note = o.display().to_string();
            }

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&run.summary)
                        .map_err(|e| CoreError::InvalidData(format!("summary json: {e}")))?
                );
            } else {
                let symbol = run
                    .summary
                    .get("symbols")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|s| s.as_str())
                    .unwrap_or("-")
                    .to_string();
                let strategy_name = run
                    .summary
                    .get("strategy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-")
                    .to_string();
                println!(
                    "{}",
                    render(
                        &run.metrics,
                        &strategy_name,
                        &symbol,
                        resolved.config.runtime.display_dp
                    )
                );
                println!();
                println!("experiment_id: {}", run.experiment_id);
                println!("result_hash:   {}", run.result_hash);
                if !run_dir_note.is_empty() {
                    println!("outputs:       {run_dir_note}");
                }
            }
            Ok(())
        }
        Commands::ValidateData { data, config } => {
            let cfg = load_config_opt(config.as_deref())?;
            let ds = project_cmds::load_dataset(&data, &cfg)?;
            println!(
                "VALID: {} symbols, {} rows kept, interval {}s",
                ds.series.len(),
                ds.report.rows_kept,
                ds.base_interval_secs
            );
            if !ds.report.issues.is_empty() {
                for i in &ds.report.issues {
                    println!("  issue: {} row {} {}", i.code.as_str(), i.row, i.detail);
                }
                println!("INVALID: {} issue(s) found", ds.report.issues.len());
                std::process::exit(1);
            }
            println!("data_hash: {}", ds.data_hash);
            Ok(())
        }
        Commands::ValidateStrategy { strategy, config } => {
            let text = std::fs::read_to_string(&strategy).map_err(|e| {
                CoreError::InvalidData(format!("cannot read strategy {}: {e}", strategy.display()))
            })?;
            let spec = bt_strategy::spec::parse_spec(&text).map_err(CoreError::StrategyError)?;
            let cfg = load_config_opt(config.as_deref())?;
            let base_secs = match &cfg.market.timeframe {
                Some(tf) => bt_core::time::parse_interval(tf)?,
                None => 3600,
            };
            match bt_strategy::runtime::compile(&spec, base_secs) {
                Ok(c) => {
                    println!("VALID: strategy '{}'", c.name);
                    println!("  symbols: {}", c.symbols.join(", "));
                    println!("  indicators: {}", c.indicators.len());
                    if !c.htfs.is_empty() {
                        println!(
                            "  higher timeframes: {}",
                            c.htfs
                                .iter()
                                .map(|(n, s)| format!("{n}({s}s)"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    Ok(())
                }
                Err(errors) => {
                    println!("INVALID: {} reason(s):", errors.len());
                    for e in &errors {
                        println!("  - {e}");
                    }
                    std::process::exit(1);
                }
            }
        }
        Commands::Report { results, html } => {
            let metrics = read_metrics(&results)?;
            let summary = read_json(&results.join("summary.json"))?;
            let strategy = summary
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or("-")
                .to_string();
            let symbol = summary
                .get("symbols")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .unwrap_or("-")
                .to_string();
            println!("{}", render(&metrics, &strategy, &symbol, 2));
            println!();
            if let Some(id) = summary.get("experiment_id").and_then(|v| v.as_str()) {
                println!("experiment_id: {id}");
            }
            if let Some(h) = summary.get("result_hash").and_then(|v| v.as_str()) {
                println!("result_hash:   {h}");
            }
            if html {
                let page = bt_harness::html::render_run_report(
                    &metrics,
                    &strategy,
                    &symbol,
                    &read_equity_series(&results)?,
                    &read_trades_html_table(&results)?,
                    "",
                );
                let path = results.join("report.html");
                std::fs::write(&path, page)?;
                println!("html report:   {}", path.display());
            }
            Ok(())
        }
        Commands::Compare { a, b } => {
            let va = read_json(&a.join("summary.json"))?;
            let vb = read_json(&b.join("summary.json"))?;
            println!("{:<20} {:>24} {:>24}", "metric", a.display(), b.display());
            for key in [
                "final_equity",
                "return_pct",
                "cagr_pct",
                "sharpe",
                "sortino",
                "max_drawdown_pct",
                "profit_factor",
                "win_rate_pct",
                "trades",
            ] {
                let fa = va.get(key).map(fmt_val).unwrap_or_else(|| "n/a".into());
                let fb = vb.get(key).map(fmt_val).unwrap_or_else(|| "n/a".into());
                println!("{:<20} {:>24} {:>24}", key, fa, fb);
            }
            let ha = va.get("result_hash").and_then(|v| v.as_str()).unwrap_or("");
            let hb = vb.get("result_hash").and_then(|v| v.as_str()).unwrap_or("");
            println!(
                "{:<20} {:>24}",
                "identical results",
                if ha == hb {
                    "YES (same result_hash)"
                } else {
                    "no"
                }
            );
            Ok(())
        }
        Commands::Query { target, sql } => {
            let conn = open_query_target(&target)?;
            let (cols, rows) = bt_harness::query::run_select(&conn, &sql)?;
            println!("{}", cols.join(","));
            let n_rows = rows.len();
            for r in &rows {
                println!("{}", r.join(","));
            }
            println!("[{n_rows} rows]");
            Ok(())
        }
        Commands::Registry { cmd } => {
            let project = Project::open_current()?;
            let reg = bt_harness::registry::Registry::open(&project.registry_path())?;
            match cmd {
                RegistryCmd::List { kind, limit } => {
                    let kind = kind.as_deref().map(parse_kind_str);
                    let runs = reg.list(kind, limit)?;
                    println!("    id  created             kind         label                   return%   sharpe  trades  experiment");
                    for r in runs {
                        println!(
                            "{:>5}  {:<19}  {:<11}  {:<22}  {:>12.2}  {:>8.2}  {:>7}  {}",
                            r.id,
                            r.created_at,
                            r.kind.as_str(),
                            truncate(&r.label, 22),
                            r.return_pct,
                            r.sharpe.unwrap_or(f64::NAN),
                            r.trades,
                            &r.experiment_id[..8.min(r.experiment_id.len())],
                        );
                    }
                    Ok(())
                }
                RegistryCmd::Show { id } => {
                    let lineage = reg.lineage(id)?;
                    if lineage.is_empty() {
                        return Err(CoreError::InvalidData(format!("run {id} not found")));
                    }
                    for r in lineage {
                        println!(
                            "#{} [{}] {} — {} return={:.2}% sharpe={} trades={} out={}",
                            r.id,
                            r.kind.as_str(),
                            r.label,
                            r.experiment_id,
                            r.return_pct,
                            fmt_opt(r.sharpe),
                            r.trades,
                            r.out_dir
                        );
                    }
                    Ok(())
                }
                RegistryCmd::Tag { id, tag } => {
                    reg.set_tag(id, &tag)?;
                    println!("tagged #{id} as '{tag}'");
                    Ok(())
                }
            }
        }
        Commands::Sweep {
            grid,
            data,
            strategy,
            config,
            strategy_version,
        } => {
            let resolved = project_cmds::resolve_inputs(
                data.as_deref(),
                strategy.as_deref(),
                config.as_deref(),
                None,
                strategy_version.as_deref(),
            )?;
            let dataset = project_cmds::load_dataset(&resolved.data_path, &resolved.config)?;
            let strategy_text = std::fs::read_to_string(&resolved.strategy_path)
                .map_err(|e| CoreError::InvalidData(format!("read strategy: {e}")))?;
            let sweep_cfg = load_sweep_config(&grid)?;
            let (report, manifest_id) =
                run_sweep_registered(&resolved, &dataset, &strategy_text, &sweep_cfg)?;
            println!(
                "sweep: {} combinations ({} evaluated, {} failed), objective={}",
                report.total_combinations,
                report.evaluated,
                report.failed,
                report.objective.name()
            );
            println!("combination                                    objective    return%    sharpe   maxDD%  error");
            for r in &report.rows {
                println!(
                    "{:<40} {:>12} {:>10.2} {:>8.2} {:>8.2} {}",
                    truncate(&r.label, 40),
                    r.objective_value
                        .map(|v| format!("{v:.3}"))
                        .unwrap_or_else(|| "-".into()),
                    r.return_pct,
                    r.sharpe.unwrap_or(f64::NAN),
                    r.max_dd_pct,
                    r.error.as_deref().unwrap_or("")
                );
            }
            if let Some(best) = &report.best {
                println!("best: {}", best.label);
            }
            if let Some(w) = &report.non_monotonic {
                println!("WARNING: {w}");
            }
            if let Some(id) = manifest_id {
                println!("registry: sweep cells recorded under manifest #{id}");
            }
            Ok(())
        }
        Commands::WalkForward {
            window,
            oos,
            anchored,
            optimize,
            data,
            strategy,
            config,
            strategy_version,
        } => {
            let resolved = project_cmds::resolve_inputs(
                data.as_deref(),
                strategy.as_deref(),
                config.as_deref(),
                None,
                strategy_version.as_deref(),
            )?;
            let dataset = project_cmds::load_dataset(&resolved.data_path, &resolved.config)?;
            let strategy_text = std::fs::read_to_string(&resolved.strategy_path)
                .map_err(|e| CoreError::InvalidData(format!("read strategy: {e}")))?;
            let wf_cfg = bt_harness::walkforward::WalkForwardConfig {
                window_days: bt_harness::walkforward::parse_window_days(&window)?,
                oos_days: bt_harness::walkforward::parse_window_days(&oos)?,
                anchored,
                optimize: match &optimize {
                    Some(p) => Some(load_sweep_config(p)?),
                    None => None,
                },
            };
            let (report, manifest_id) =
                run_wf_registered(&resolved, &dataset, &strategy_text, &wf_cfg)?;
            println!(
                "walk-forward: {} window(s), mode={}, objective={}",
                report.windows.len(),
                report.mode,
                wf_cfg
                    .optimize
                    .as_ref()
                    .map(|o| o.objective.name())
                    .unwrap_or("sharpe")
            );
            println!(
                "w   OOS window                     IS obj   OOS obj   OOS ret%  trades  error"
            );
            for w in &report.windows {
                println!(
                    "{:<3} {:<24} {:>10} {:>10} {:>9.2} {:>7}  {}",
                    w.index,
                    format!("{}→{}", &w.oos_start[..10], &w.oos_end[..10]),
                    w.is_objective
                        .map(|v| format!("{v:.3}"))
                        .unwrap_or_else(|| "-".into()),
                    w.oos_objective
                        .map(|v| format!("{v:.3}"))
                        .unwrap_or_else(|| "-".into()),
                    w.oos_return_pct,
                    w.oos_trades,
                    w.error.as_deref().unwrap_or("")
                );
            }
            println!(
                "stitched OOS: return {:.2}%  maxDD {:.2}%  WFE {}",
                report.stitched_return_pct,
                report.stitched_max_dd_pct,
                report
                    .walk_forward_efficiency
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or_else(|| "-".into())
            );
            if let Some(id) = manifest_id {
                println!("registry: windows recorded under manifest #{id}");
            }
            Ok(())
        }
        Commands::Stress {
            scenario,
            scenario_dir,
            perturb,
            data,
            strategy,
            config,
            strategy_version,
        } => {
            let resolved = project_cmds::resolve_inputs(
                data.as_deref(),
                strategy.as_deref(),
                config.as_deref(),
                None,
                strategy_version.as_deref(),
            )?;
            let dataset = project_cmds::load_dataset(&resolved.data_path, &resolved.config)?;
            let strategy_text = std::fs::read_to_string(&resolved.strategy_path)
                .map_err(|e| CoreError::InvalidData(format!("read strategy: {e}")))?;

            let mut scenario_paths = scenario.clone();
            if let Some(dir) = &scenario_dir {
                let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
                    .map_err(CoreError::Io)?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        let ext = p.extension().and_then(|e| e.to_str());
                        ext == Some("yaml") || ext == Some("yml")
                    })
                    .collect();
                entries.sort();
                scenario_paths.extend(entries);
            }
            if scenario_paths.is_empty() {
                return Err(CoreError::InvalidData(
                    "no scenarios given — use --scenario <file.yaml> or --scenario-dir <dir>"
                        .into(),
                ));
            }
            scenario_paths.sort();
            let mut scenarios = Vec::new();
            for p in &scenario_paths {
                scenarios.push(bt_harness::stress::ScenarioSpec::load(p)?);
            }

            let mut perturbation: BTreeMap<String, Vec<serde_yaml::Value>> = BTreeMap::new();
            for spec_str in &perturb {
                let (path, values) = spec_str.split_once('=').ok_or_else(|| {
                    CoreError::ConfigError(format!("--perturb '{spec_str}' must be path=v1,v2,v3"))
                })?;
                let vals: Vec<serde_yaml::Value> = values
                    .split(',')
                    .map(|v| serde_yaml::from_str(v.trim()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| CoreError::ConfigError(format!("--perturb '{spec_str}': {e}")))?;
                if vals.is_empty() {
                    return Err(CoreError::ConfigError(format!(
                        "--perturb '{spec_str}': empty value list"
                    )));
                }
                perturbation.insert(path.trim().to_string(), vals);
            }

            let (report, manifest_id) = run_stress_registered(
                &resolved,
                &dataset,
                &strategy_text,
                &scenarios,
                &perturbation,
            )?;

            println!(
                "stress: {} scenario(s), {} cell(s) (stress id {})",
                scenarios.len(),
                report.rows.len(),
                &report.stress_id[..16]
            );
            println!("scenario                 perturbation                  return%   final eq   maxDD%  trades  error");
            if let Some(b) = &report.baseline {
                println!(
                    "{:<26} {:<28} {:>10.2} {:>10.2} {:>8.2} {:>7}",
                    truncate(&b.scenario, 26),
                    truncate(&b.perturbation, 28),
                    b.return_pct,
                    b.final_equity,
                    b.max_dd_pct,
                    b.trades
                );
            }
            for r in &report.rows {
                println!(
                    "{:<26} {:<28} {:>10.2} {:>10.2} {:>8.2} {:>7}  {}",
                    truncate(&r.scenario, 26),
                    truncate(&r.perturbation, 28),
                    r.return_pct,
                    r.final_equity,
                    r.max_dd_pct,
                    r.trades,
                    r.error.as_deref().unwrap_or("")
                );
            }
            match (&report.worst, &report.baseline) {
                (Some(w), Some(b)) => println!(
                    "worst case: {} ({}): {:.2}% vs baseline {:.2}%  ->  impact {:.2}pp",
                    w.scenario,
                    w.perturbation,
                    w.return_pct,
                    b.return_pct,
                    w.return_pct - b.return_pct
                ),
                (Some(w), None) => println!(
                    "worst case: {} ({}): {:.2}%",
                    w.scenario, w.perturbation, w.return_pct
                ),
                _ => {}
            }
            if let Some(project) = &resolved.project {
                let dir = project
                    .results_dir()
                    .join(&report.stress_id[..16.min(report.stress_id.len())]);
                std::fs::create_dir_all(&dir)?;
                let path = dir.join("stress_report.json");
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&report)
                        .map_err(|e| CoreError::InvalidData(format!("stress json: {e}")))?,
                )?;
                println!("report: {}", path.display());
            }
            if let Some(id) = manifest_id {
                println!("registry: stress cells recorded under manifest #{id}");
            }
            Ok(())
        }

        Commands::Robust {
            runs,
            registry_tag,
            blocks,
        } => {
            let mut dirs = runs;
            if dirs.is_empty() {
                let tag = registry_tag.as_ref().ok_or_else(|| {
                    CoreError::InvalidData("give run directories or --registry-tag <tag>".into())
                })?;
                let project = Project::open_current()?;
                let reg = bt_harness::registry::Registry::open(&project.registry_path())?;
                let mut tagged = 0usize;
                for r in reg.runs_with_tag(tag)? {
                    if !r.out_dir.is_empty() {
                        dirs.push(PathBuf::from(&r.out_dir));
                        tagged += 1;
                    }
                }
                if tagged < 2 {
                    return Err(CoreError::InvalidData(format!(
                        "tag '{tag}' selects {tagged} run(s) with persisted artifacts; tag at least 2                          runs that were executed inside the project (sweep cells have no artifacts)"
                    )));
                }
            }
            if dirs.len() < 2 {
                return Err(CoreError::InvalidData(
                    "robustness analysis needs at least 2 runs".into(),
                ));
            }
            let series = load_return_series(&dirs)?;
            let report = bt_harness::robust::robust_report(&series, blocks)?;
            println!(
                "robustness: {} trials, {} aligned periods, {} CSCV blocks",
                report.trials, report.periods, blocks
            );
            for (i, s) in report.trial_sharpes.iter().enumerate() {
                println!("  trial {}: sharpe {}", i + 1, fmt_opt(*s));
            }
            println!("observed sharpe:   {}", fmt_opt(report.observed_sharpe));
            println!("deflated sharpe:   {}", fmt_opt(report.deflated_sharpe));
            if let Some(p) = &report.pbo {
                println!("PBO (CSCV):        {:.3} ({} splits)", p.pbo, p.splits);
            }
            println!("verdict: {}", report.verdict);
            Ok(())
        }
        Commands::MonteCarlo {
            results,
            paths,
            seed,
        } => {
            let path = results.join("trades.csv");
            let text = std::fs::read_to_string(&path)
                .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
            let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
            let headers = reader
                .headers()
                .map_err(|e| CoreError::InvalidData(format!("trades.csv: {e}")))?
                .clone();
            let net_i = headers
                .iter()
                .position(|h| h.trim() == "net_pnl")
                .ok_or_else(|| CoreError::InvalidData("trades.csv missing net_pnl".into()))?;
            let mut pnls: Vec<D> = Vec::new();
            for rec in reader.records() {
                let rec: csv::StringRecord =
                    rec.map_err(|e| CoreError::InvalidData(format!("trades.csv: {e}")))?;
                pnls.push(
                    rec.get(net_i)
                        .unwrap_or("0")
                        .trim()
                        .parse()
                        .unwrap_or(Decimal::ZERO),
                );
            }
            let trades: Vec<bt_core::ledger::TradeRecord> = pnls
                .into_iter()
                .enumerate()
                .map(|(i, net)| bt_core::ledger::TradeRecord {
                    trade_id: i as u64,
                    symbol: String::new(),
                    direction: String::new(),
                    entry_timestamp: Ts::UNIX_EPOCH,
                    entry_price: Decimal::ONE,
                    exit_timestamp: Ts::UNIX_EPOCH,
                    exit_price: Decimal::ONE,
                    quantity: Decimal::ONE,
                    gross_pnl: net,
                    commission: Decimal::ZERO,
                    spread_cost: Decimal::ZERO,
                    slippage_cost: Decimal::ZERO,
                    financing: Decimal::ZERO,
                    dividends: Decimal::ZERO,
                    net_pnl: net,
                    initial_risk: None,
                    r_multiple: None,
                    holding_bars: 0,
                    holding_time_secs: 0,
                    mae: Decimal::ZERO,
                    mfe: Decimal::ZERO,
                    entry_reason: String::new(),
                    exit_reason: String::new(),
                })
                .collect();
            let mc = bt_analytics::mc::run_monte_carlo(
                &trades,
                Decimal::from(100000),
                paths,
                seed,
                bt_analytics::metrics::McMethod::Reshuffle,
                Decimal::from(50),
            );
            println!("monte carlo: {paths} paths, seed {seed}");
            println!(
                "final equity p5/p50/p95: {:.2} / {:.2} / {:.2}",
                mc.final_equity.p5, mc.final_equity.p50, mc.final_equity.p95
            );
            println!(
                "max dd% p50/p95: {:.2} / {:.2}",
                mc.max_drawdown_pct.p50, mc.max_drawdown_pct.p95
            );
            println!("risk of ruin: {:.2}%", mc.risk_of_ruin_pct);
            let path = results.join("monte_carlo.json");
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&mc)
                    .map_err(|e| CoreError::InvalidData(format!("mc json: {e}")))?,
            )?;
            Ok(())
        }
    }
}

// ---------- helpers ----------

fn load_config_opt(path: Option<&Path>) -> CoreResult<EngineConfig> {
    match path {
        None => Ok(EngineConfig::default()),
        Some(p) => project_cmds::load_config_file(p),
    }
}

fn read_json(path: &Path) -> CoreResult<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| CoreError::InvalidData(format!("{}: {e}", path.display())))
}

fn read_metrics(results: &Path) -> CoreResult<bt_analytics::MetricsReport> {
    let v = read_json(&results.join("metrics.json"))?;
    serde_json::from_value(v).map_err(|e| CoreError::InvalidData(format!("metrics.json: {e}")))
}

fn read_equity_series(results: &Path) -> CoreResult<Vec<(String, f64, f64)>> {
    let text = std::fs::read_to_string(results.join("equity_curve.csv"))
        .map_err(|e| CoreError::InvalidData(format!("equity_curve.csv: {e}")))?;
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| CoreError::InvalidData(format!("{e}")))?
        .clone();
    let names: Vec<String> = headers.iter().map(|h| h.trim().to_string()).collect();
    let ti = names.iter().position(|h| h == "ts").unwrap_or(0);
    let ei = names.iter().position(|h| h == "equity").unwrap_or(1);
    let di = names.iter().position(|h| h == "drawdown_pct").unwrap_or(5);
    let mut out = Vec::new();
    for rec in reader.records() {
        let rec: csv::StringRecord = rec.map_err(|e| CoreError::InvalidData(format!("{e}")))?;
        out.push((
            rec.get(ti).unwrap_or("").to_string(),
            rec.get(ei).and_then(|v| v.parse().ok()).unwrap_or(0.0),
            rec.get(di).and_then(|v| v.parse().ok()).unwrap_or(0.0),
        ));
    }
    Ok(out)
}

fn read_trades_html_table(results: &Path) -> CoreResult<String> {
    let path = results.join("trades.csv");
    if !path.exists() {
        return Ok("<p>no trades</p>".into());
    }
    let text = std::fs::read_to_string(&path).map_err(CoreError::Io)?;
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| CoreError::InvalidData(format!("{e}")))?
        .clone();
    let mut html = String::from("<table><tr>");
    for h in headers.iter() {
        html.push_str(&format!("<th>{}</th>", h.trim()));
    }
    html.push_str("</tr>");
    for (i, rec) in reader.records().enumerate() {
        if i >= 100 {
            html.push_str(&format!(
                "<tr><td colspan=\"{}\">… more rows in trades.csv</td></tr>",
                headers.len()
            ));
            break;
        }
        let rec: csv::StringRecord = rec.map_err(|e| CoreError::InvalidData(format!("{e}")))?;
        html.push_str("<tr>");
        for j in 0..headers.len() {
            html.push_str(&format!("<td>{}</td>", rec.get(j).unwrap_or("")));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    Ok(html)
}

fn open_query_target(target: &str) -> CoreResult<rusqlite::Connection> {
    let path = Path::new(target);
    if target == "registry" {
        let project = Project::open_current()?;
        bt_harness::query::open_registry_db(&project.registry_path())
    } else if path.is_dir() {
        bt_harness::query::open_run_db(path)
    } else if path.extension().and_then(|e| e.to_str()) == Some("db") {
        bt_harness::query::open_registry_db(path)
    } else {
        Err(CoreError::InvalidData(format!(
            "query target '{target}' is neither a results directory nor a registry db"
        )))
    }
}

fn parse_kind_str(s: &str) -> RunKind {
    match s {
        "sweep_cell" => RunKind::SweepCell,
        "sweep_manifest" => RunKind::SweepManifest,
        "walk_forward_window" => RunKind::WalkForwardWindow,
        "walk_forward_manifest" => RunKind::WalkForwardManifest,
        "stress_cell" => RunKind::StressCell,
        "stress_manifest" => RunKind::StressManifest,
        _ => RunKind::Single,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_else(|| "-".into())
}

fn fmt_val(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Number(n) => format!("{n}"),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".into(),
        other => other.to_string(),
    }
}

fn load_sweep_config(path: &Path) -> CoreResult<bt_harness::sweep::SweepConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
    // Accept both the bare config and a {sweep: ...} wrapper.
    if let Ok(wrapped) = serde_yaml::from_str::<Wrapper>(&text) {
        return Ok(wrapped.sweep);
    }
    serde_yaml::from_str(&text)
        .map_err(|e| CoreError::ConfigError(format!("sweep config {}: {e}", path.display())))
}

#[derive(serde::Deserialize)]
struct Wrapper {
    sweep: bt_harness::sweep::SweepConfig,
}

/// Sweep with a manifest row + cells recorded in the project registry.
fn run_sweep_registered(
    resolved: &project_cmds::Resolved,
    dataset: &Dataset,
    strategy_text: &str,
    sweep_cfg: &bt_harness::sweep::SweepConfig,
) -> CoreResult<(bt_harness::sweep::SweepReport, Option<i64>)> {
    match &resolved.project {
        Some(project) => {
            let reg = bt_harness::registry::Registry::open(&project.registry_path())?;
            let manifest = reg.insert(&manifest_row(
                resolved,
                RunKind::SweepManifest,
                &format!("sweep {} param(s)", sweep_cfg.grid.len()),
            ))?;
            let report = bt_harness::sweep::run_sweep(
                dataset,
                strategy_text,
                &resolved.config,
                sweep_cfg,
                Some((&reg, manifest, &resolved.strategy_version)),
            )?;
            Ok((report, Some(manifest)))
        }
        None => Ok((
            bt_harness::sweep::run_sweep(
                dataset,
                strategy_text,
                &resolved.config,
                sweep_cfg,
                None,
            )?,
            None,
        )),
    }
}

fn run_wf_registered(
    resolved: &project_cmds::Resolved,
    dataset: &Dataset,
    strategy_text: &str,
    wf_cfg: &bt_harness::walkforward::WalkForwardConfig,
) -> CoreResult<(bt_harness::walkforward::WalkForwardReport, Option<i64>)> {
    match &resolved.project {
        Some(project) => {
            let reg = bt_harness::registry::Registry::open(&project.registry_path())?;
            let manifest = reg.insert(&manifest_row(
                resolved,
                RunKind::WalkForwardManifest,
                &format!("walk-forward {}d/{}d", wf_cfg.window_days, wf_cfg.oos_days),
            ))?;
            let report = bt_harness::walkforward::run_walk_forward(
                dataset,
                strategy_text,
                &resolved.config,
                wf_cfg,
                Some((&reg, manifest, &resolved.strategy_version)),
            )?;
            Ok((report, Some(manifest)))
        }
        None => Ok((
            bt_harness::walkforward::run_walk_forward(
                dataset,
                strategy_text,
                &resolved.config,
                wf_cfg,
                None,
            )?,
            None,
        )),
    }
}

fn run_stress_registered(
    resolved: &project_cmds::Resolved,
    dataset: &Dataset,
    strategy_text: &str,
    scenarios: &[bt_harness::stress::ScenarioSpec],
    perturbation: &BTreeMap<String, Vec<serde_yaml::Value>>,
) -> CoreResult<(bt_harness::stress::StressReport, Option<i64>)> {
    match &resolved.project {
        Some(project) => {
            let reg = bt_harness::registry::Registry::open(&project.registry_path())?;
            let manifest = reg.insert(&manifest_row(
                resolved,
                RunKind::StressManifest,
                &format!("stress {} scenario(s)", scenarios.len()),
            ))?;
            let report = bt_harness::stress::run_stress(
                dataset,
                strategy_text,
                &resolved.config,
                scenarios,
                perturbation,
                Some((&reg, manifest, &resolved.strategy_version)),
            )?;
            Ok((report, Some(manifest)))
        }
        None => Ok((
            bt_harness::stress::run_stress(
                dataset,
                strategy_text,
                &resolved.config,
                scenarios,
                perturbation,
                None,
            )?,
            None,
        )),
    }
}

fn manifest_row(
    resolved: &project_cmds::Resolved,
    kind: RunKind,
    label: &str,
) -> bt_harness::registry::RegistryRun {
    bt_harness::registry::RegistryRun {
        id: 0,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        experiment_id: "manifest".into(),
        result_hash: String::new(),
        kind,
        parent_id: None,
        label: label.to_string(),
        strategy_name: resolved
            .strategy_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        strategy_version: resolved.strategy_version.clone(),
        symbols: String::new(),
        timeframe_secs: 0,
        data_hash: String::new(),
        start_ts: String::new(),
        end_ts: String::new(),
        initial_capital: String::new(),
        final_equity: 0.0,
        return_pct: 0.0,
        sharpe: None,
        sortino: None,
        max_dd_pct: 0.0,
        profit_factor: None,
        win_rate_pct: None,
        trades: 0,
        out_dir: String::new(),
        params_json: "{}".into(),
    }
}

/// Load per-run daily return series aligned on the intersection of dates.
fn load_return_series(dirs: &[PathBuf]) -> CoreResult<Vec<Vec<f64>>> {
    let mut per_run: Vec<BTreeMap<String, f64>> = Vec::new();
    for d in dirs {
        let path = d.join("daily_returns.csv");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
        let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
        let headers = reader
            .headers()
            .map_err(|e| CoreError::InvalidData(format!("{e}")))?
            .clone();
        let names: Vec<String> = headers.iter().map(|h| h.trim().to_string()).collect();
        let ti = names.iter().position(|h| h == "ts").unwrap_or(0);
        let ri = names.iter().position(|h| h == "return_pct").unwrap_or(1);
        let mut map = BTreeMap::new();
        for rec in reader.records() {
            let rec: csv::StringRecord = rec.map_err(|e| CoreError::InvalidData(format!("{e}")))?;
            let ts = rec.get(ti).unwrap_or("").to_string();
            let r: f64 = rec
                .get(ri)
                .unwrap_or("0")
                .parse()
                .map_err(|_| CoreError::InvalidData("daily_returns.csv: bad return".into()))?;
            map.insert(ts, r);
        }
        per_run.push(map);
    }
    // Dates present in ALL runs.
    let common: Vec<String> = per_run
        .first()
        .map(|first| {
            first
                .keys()
                .filter(|k| per_run.iter().all(|m| m.contains_key(*k)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if common.is_empty() {
        return Err(CoreError::InvalidData(
            "no common dates across the given runs — align the data windows first".into(),
        ));
    }
    Ok(per_run
        .iter()
        .map(|m| common.iter().map(|d| m[d]).collect())
        .collect())
}

fn ai_dispatch(cmd: AiCmd) -> CoreResult<()> {
    fn flags_for<'a>(
        api_key: &'a Option<String>,
        model: &'a Option<String>,
        base_url: &'a Option<String>,
        max_tokens: &'a Option<u32>,
        temperature: &'a Option<f64>,
        no_cache: bool,
    ) -> ai_cmds::AiFlags<'a> {
        ai_cmds::AiFlags {
            api_key: api_key.as_deref(),
            model: model.as_deref(),
            base_url: base_url.as_deref(),
            max_tokens: *max_tokens,
            temperature: *temperature,
            no_cache,
        }
    }
    match cmd {
        AiCmd::Compile {
            strategy_version,
            config,
            api_key,
            model,
            base_url,
            max_tokens,
            temperature,
            no_cache,
            dry_run,
        } => {
            let project = Project::open_current()?;
            let flags = flags_for(
                &api_key,
                &model,
                &base_url,
                &max_tokens,
                &temperature,
                no_cache,
            );
            let mut ai = ai_cmds::setup_with(Some(project.clone()), &flags, !dry_run)?;
            if let Some(c) = &config {
                // explicit version config override wins over config.toml
                let cfg = project_cmds::load_config_file(c)?;
                ai.settings.runs = {
                    let mut r = ai.settings.runs.clone();
                    r.default_starting_capital = cfg.account.starting_capital.to_string();
                    r
                };
            }
            ai_cmds::cmd_compile(&project, &mut ai, strategy_version.as_deref(), dry_run)
        }
        AiCmd::Review {
            strategy_version,
            config,
            api_key,
            model,
            base_url,
            max_tokens,
            temperature,
            no_cache,
            dry_run,
        } => {
            let _ = config;
            let project = Project::open_current()?;
            let flags = flags_for(
                &api_key,
                &model,
                &base_url,
                &max_tokens,
                &temperature,
                no_cache,
            );
            let mut ai = ai_cmds::setup_with(Some(project.clone()), &flags, !dry_run)?;
            ai_cmds::cmd_review(&project, &mut ai, strategy_version.as_deref(), dry_run)
        }
        AiCmd::Explain {
            results,
            api_key,
            model,
            base_url,
            max_tokens,
            temperature,
            no_cache,
            dry_run,
        } => {
            let flags = flags_for(
                &api_key,
                &model,
                &base_url,
                &max_tokens,
                &temperature,
                no_cache,
            );
            let project = Project::open_current().ok();
            let mut ai = ai_cmds::setup_with(project.clone(), &flags, !dry_run)?;
            ai_cmds::cmd_explain(project.as_ref(), &mut ai, &results, dry_run)
        }
        AiCmd::Ask {
            question,
            api_key,
            model,
            base_url,
            max_tokens,
            temperature,
            no_cache,
            dry_run,
        } => {
            let project = Project::open_current()?;
            let flags = flags_for(
                &api_key,
                &model,
                &base_url,
                &max_tokens,
                &temperature,
                no_cache,
            );
            let mut ai = ai_cmds::setup_with(Some(project.clone()), &flags, !dry_run)?;
            ai_cmds::cmd_ask(&project, &mut ai, &question, dry_run)
        }
        AiCmd::Status { api_key } => {
            let project = Project::open_current().ok();
            let flags = ai_cmds::AiFlags {
                api_key: api_key.as_deref(),
                model: None,
                base_url: None,
                max_tokens: None,
                temperature: None,
                no_cache: false,
            };
            let ai = ai_cmds::setup_with(project.clone(), &flags, false)?;
            selfupdate::status()?;
            ai_cmds::cmd_status(&ai, project.as_ref())
        }
        AiCmd::Ledger { limit } => {
            let project = Project::open_current().ok();
            let flags = ai_cmds::AiFlags {
                api_key: None,
                model: None,
                base_url: None,
                max_tokens: None,
                temperature: None,
                no_cache: false,
            };
            let ai = ai_cmds::setup_with(project.clone(), &flags, false)?;
            ai_cmds::cmd_ledger(&ai, limit)
        }
        AiCmd::Login { api_key, clear } => ai_cmds::cmd_login(api_key.as_deref(), clear),
    }
}
