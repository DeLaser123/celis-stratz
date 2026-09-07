//! Human-readable terminal report (spec §27).

use bt_analytics::metrics::MetricsReport;

pub fn render(metrics: &MetricsReport, strategy: &str, symbol: &str, dp: u32) -> String {
    let m = metrics;
    let f = |v: Option<f64>, suffix: &str| -> String {
        match v {
            Some(x) => format!("{x:.dp$}{suffix}", dp = dp as usize),
            None => "n/a".into(),
        }
    };
    let line = "─".repeat(52);
    format!(
        "\
STRATZ RESULT
{line}
Strategy:      {strategy}
Symbol:        {symbol}
Period:        {start} → {end}
Initial:       {initial:.dp$}
Final Equity:  {final:.dp$}
Return:        {ret}
CAGR:          {cagr}
Sharpe:        {sharpe}
Sortino:       {sortino}
Max DD:        {dd}
Profit Factor: {pf}
Win Rate:      {wr}
Expectancy:    {exp} R
Avg R:         {avgr}
Trades:        {trades}
{line}
Assumptions:   ann={ann} rf={rf} R={rconv} pf={pfconv}",
        line = line,
        strategy = strategy,
        symbol = symbol,
        start = m
            .overview
            .start_ts
            .map(bt_core::time::format_ts)
            .unwrap_or_else(|| "n/a".into()),
        end = m
            .overview
            .end_ts
            .map(bt_core::time::format_ts)
            .unwrap_or_else(|| "n/a".into()),
        dp = dp as usize,
        initial = m.overview.initial_capital,
        final = m.overview.final_equity,
        ret = f(m.overview.total_return_pct, "%"),
        cagr = f(m.overview.cagr_pct, "%"),
        sharpe = f(m.returns.sharpe, ""),
        sortino = f(m.returns.sortino, ""),
        dd = f(Some(-m.risk.max_drawdown_pct), "%"),
        pf = f(m.trades.profit_factor, ""),
        wr = f(m.trades.win_rate_pct, "%"),
        exp = f(m.r_stats.avg_r, ""),
        avgr = f(m.r_stats.avg_r, ""),
        trades = m.trades.total_trades,
        ann = m.conventions.annualization_factor,
        rf = m.conventions.risk_free_rate_annual,
        rconv = format!("{:?}", m.conventions.r_convention).to_lowercase(),
        pfconv = format!("{:?}", m.conventions.profit_factor_basis).to_lowercase(),
    )
}
