p = r"crates/bt-cli/src/main.rs"
s = open(p, encoding="utf8").read()

# module
if "mod pull_chart;" not in s:
    old = "mod selfupdate;"
    assert old in s, "mod anchor"
    s = s.replace(old, "mod pull_chart;\nmod selfupdate;", 1)

# variant
if "PullChart {" not in s:
    old = """    /// Update the installed stratz binary (source build or GitHub release).
    SelfUpdate {"""
    new = """    /// Download OHLCV candles from Dukascopy into the project's Data/ folder.
    PullChart {
        /// Instrument symbol, e.g. EURUSD.
        symbol: String,
        /// Timeframe with optional lookback: "5,3" = 5-min, 3 years back.
        /// Suffixes: bare number = minutes; 5m/1h/4h/1d. Lookback: 3 = 3y,
        /// 3y / 6mo / 2w / 30d.
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
    SelfUpdate {"""
    assert old in s, "variant anchor"
    s = s.replace(old, new, 1)

# dispatch
if "Commands::PullChart {" not in s:
    old = "        Commands::SelfUpdate { check, force, disable, enable, set_source } => {"
    new = """        Commands::PullChart { symbol, spec, from, to, side, decimals, output } => {
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
        Commands::SelfUpdate { check, force, disable, enable, set_source } => {"""
    assert old in s, "dispatch anchor"
    s = s.replace(old, new, 1)

open(p, "w", encoding="utf8", newline="\n").write(s)
print("pull-chart wired")
