//! `stratz pull-chart` — download Dukascopy OHLCV candles into the project's
//! Data/ folder (or --output), validated and ready for `stratz run`.

use bt_core::error::{CoreError, CoreResult};
use bt_core::time::{format_ts, parse_timestamp, Ts};
use bt_data::dukascopy::{self, PullSpec, Side};
use bt_harness::project::Project;
use std::path::PathBuf;

pub struct PullChartArgs<'a> {
    pub symbol: &'a str,
    /// Timeframe with optional lookback: "5,3" (5-minute, 3 years back),
    /// "1h,2y", "15", "1d --from 2022-01-01".
    pub spec: &'a str,
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub side: &'a str,
    pub decimals: Option<u32>,
    pub output: Option<&'a PathBuf>,
    /// Skip the resumable download cache (fresh copy from the network).
    pub no_cache: bool,
}

pub fn cmd(args: &PullChartArgs<'_>) -> CoreResult<()> {
    let symbol = args.symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(CoreError::InvalidData("symbol is empty".into()));
    }
    let (timeframe_secs, lookback) = dukascopy::parse_tf_spec(args.spec)?;

    // Range: --from/--to override the lookback; to defaults to now.
    let now = Ts::UNIX_EPOCH;
    let _ = now;
    let to = match args.to {
        Some(s) => parse_timestamp(s, "UTC".parse().unwrap(), "--to")?,
        None => chrono::Utc::now(),
    };
    let from = match args.from {
        Some(s) => parse_timestamp(s, "UTC".parse().unwrap(), "--from")?,
        None => {
            let lookback = lookback.ok_or_else(|| {
                CoreError::ConfigError(
                    "no range given: use \"tf,lookback\" (e.g. 5,3) or --from <date>".into(),
                )
            })?;
            lookback.to_from_ts(to)
        }
    };
    if from >= to {
        return Err(CoreError::ConfigError(format!(
            "range is empty: from {} >= to {}",
            format_ts(from),
            format_ts(to)
        )));
    }

    let side = match args.side.trim().to_lowercase().as_str() {
        "bid" => Side::Bid,
        "ask" => Side::Ask,
        other => {
            return Err(CoreError::ConfigError(format!(
                "invalid side '{other}' (bid|ask)"
            )))
        }
    };
    let decimals = args
        .decimals
        .unwrap_or_else(|| PullSpec::default_decimals(&symbol));

    let spec = PullSpec {
        symbol: symbol.clone(),
        timeframe_secs,
        from,
        to,
        side,
        decimals,
    };

    eprintln!(
        "pulling {symbol} {} candles from {} to {} (Dukascopy, {} side, {} decimals)",
        bt_data::dukascopy::tf_name(timeframe_secs),
        format_ts(from),
        format_ts(to),
        args.side,
        decimals
    );

    // Download cache: closed periods are immutable history, so an
    // interrupted or throttled pull resumes where it stopped on rerun.
    let project = Project::open_current().ok();
    let cache = if args.no_cache {
        None
    } else {
        let dir = match &project {
            Some(p) => p
                .root
                .join(bt_harness::project::STATE_DIR)
                .join("cache")
                .join("dukascopy"),
            None => std::env::temp_dir().join("stratz-dukascopy-cache"),
        };
        // The cache is an optimization, never a requirement.
        bt_data::dukascopy::Bi5Cache::new(&dir).ok()
    };

    let report = dukascopy::pull(&spec, cache.as_ref(), |done, total| {
        eprintln!("  progress: {done}/{total} files");
    })?;

    if report.series.bars.is_empty() {
        return Err(CoreError::InvalidData(format!(
            "no candles returned for {symbol} in the requested range \
             (files fetched: {}, missing: {})",
            report.files_fetched, report.files_missing
        )));
    }

    // Resolve output: --output > project Data/ > ./data
    let tf_name = bt_data::dukascopy::tf_name(timeframe_secs);
    let file_name = format!("{symbol}_{tf_name}.csv");
    let (out_path, project_note) = match args.output {
        Some(p) if p.extension().and_then(|e| e.to_str()) == Some("csv") => {
            (p.clone(), String::new())
        }
        Some(p) => (p.join(&file_name), String::new()),
        None => match &project {
            Some(project) => {
                let p = project.root.join("Data").join(&file_name);
                (
                    p,
                    format!(
                        " (auto-detected by stratz run — timeframe {}s)",
                        timeframe_secs
                    ),
                )
            }
            None => (
                std::env::current_dir()
                    .unwrap_or_default()
                    .join("data")
                    .join(&file_name),
                String::new(),
            ),
        },
    };
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(bt_core::CoreError::Io)?;
    }

    // Write the project-contract CSV.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&out_path).map_err(bt_core::CoreError::Io)?;
        writeln!(f, "timestamp,symbol,open,high,low,close,volume")
            .map_err(bt_core::CoreError::Io)?;
        for bar in &report.series.bars {
            writeln!(
                f,
                "{},{},{},{},{},{},{}",
                format_ts(bar.open_time),
                report.series.symbol,
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            )
            .map_err(bt_core::CoreError::Io)?;
        }
    }

    // Validate by loading back through the strict CSV loader (same rules the
    // engine applies).
    let tz: chrono_tz::Tz = "UTC".parse().unwrap();
    let dataset = bt_data::csv::load_bars_csv(
        &out_path,
        tz,
        bt_data::TimestampConvention::Open,
        bt_data::ValidationMode::Strict,
        bt_data::LoadLimits::default(),
        Some(timeframe_secs),
    )?;

    println!(
        "pulled {} {} candles: {} bars, {} .. {}",
        symbol,
        tf_name,
        dataset.report.rows_kept,
        dataset
            .series
            .values()
            .next()
            .and_then(|s| s.bars.first())
            .map(|b| bt_core::time::format_ts(b.open_time))
            .unwrap_or_default(),
        dataset
            .series
            .values()
            .next()
            .and_then(|s| s.bars.last())
            .map(|b| bt_core::time::format_ts(b.open_time))
            .unwrap_or_default(),
    );
    println!(
        "files: {} fetched, {} missing (empty days), {} reused from cache{}",
        report.files_fetched,
        report.files_missing,
        report.files_cached,
        cache
            .as_ref()
            .map(|c| format!(" ({})", c.dir().display()))
            .unwrap_or_default(),
    );
    println!(
        "validation: {} ({} rows kept)",
        dataset.report.summary(),
        dataset.report.rows_kept
    );
    println!("data hash: {}", dataset.data_hash);
    println!("written: {}{}", out_path.display(), project_note);
    Ok(())
}
