//! Self-contained HTML report: inline CSS + inline SVG (equity curve and
//! drawdown), monthly returns, headline metrics, and the trade ledger. No
//! external assets, no JavaScript dependencies — one file, open anywhere.

use bt_analytics::metrics::MetricsReport;
use bt_core::time::format_ts;
use bt_core::D;

/// Render the full HTML report for one run.
pub fn render_run_report(
    metrics: &MetricsReport,
    strategy: &str,
    symbols: &str,
    equity_curve: &[(String, f64, f64)], // (ts, equity, drawdown_pct)
    trades_rows: &str,
    extra_head: &str,
) -> String {
    let m = metrics;
    let equity_svg = sparkline_svg(
        &equity_curve.iter().map(|(_, e, _)| *e).collect::<Vec<_>>(),
        860,
        220,
        "#0f766e",
    );
    let dd_svg = sparkline_svg(
        &equity_curve.iter().map(|(_, _, d)| -*d).collect::<Vec<_>>(),
        860,
        120,
        "#b91c1c",
    );
    let monthly = monthly_table(m);
    let headline = format!(
        r#"<div class="grid">
  <div class="card"><div class="k">Final equity</div><div class="v">{:.2}</div></div>
  <div class="card"><div class="k">Return</div><div class="v">{:.2}%</div></div>
  <div class="card"><div class="k">CAGR</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Sharpe</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Sortino</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Max DD</div><div class="v neg">-{:.2}%</div></div>
  <div class="card"><div class="k">Profit factor</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Win rate</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Trades</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Avg R</div><div class="v">{}</div></div>
  <div class="card"><div class="k">Risk of ruin</div><div class="v">{:.2}%</div></div>
</div>"#,
        m.overview.final_equity,
        m.overview.total_return_pct.unwrap_or(0.0),
        opt(m.overview.cagr_pct, "%"),
        opt(m.returns.sharpe, ""),
        opt(m.returns.sortino, ""),
        m.risk.max_drawdown_pct.abs(),
        opt(m.trades.profit_factor, ""),
        opt(m.trades.win_rate_pct, "%"),
        m.trades.total_trades,
        opt(m.r_stats.avg_r, ""),
        m.risk.risk_of_ruin_pct,
    );
    let conventions = format!(
        "Returns: {:?} · annualization {:.1} · rf {:.2}% · std {:?} · R {:?} · PF {:?}",
        m.conventions.returns_freq,
        m.conventions.annualization_factor,
        m.conventions.risk_free_rate_annual * 100.0,
        m.conventions.std_mode,
        m.conventions.r_convention,
        m.conventions.profit_factor_basis,
    );
    let period = format!(
        "{} → {} ({} bars)",
        m.overview
            .start_ts
            .map(format_ts)
            .unwrap_or_else(|| "n/a".into()),
        m.overview
            .end_ts
            .map(format_ts)
            .unwrap_or_else(|| "n/a".into()),
        m.overview.total_bars,
    );
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>Stratz report — {strategy}</title>{extra_head}
<style>
 body {{ font-family: "Segoe UI", system-ui, sans-serif; margin: 24px; color: #1f2937; background: #f8fafc; }}
 h1 {{ font-size: 20px; margin: 0 0 4px; }} h2 {{ font-size: 15px; margin: 28px 0 8px; color: #334155; }}
 .sub {{ color: #64748b; font-size: 13px; margin-bottom: 18px; }}
 .grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(140px, 1fr)); gap: 10px; }}
 .card {{ background: #fff; border: 1px solid #e2e8f0; border-radius: 8px; padding: 10px 12px; }}
 .k {{ font-size: 11px; color: #64748b; text-transform: uppercase; letter-spacing: .04em; }}
 .v {{ font-size: 17px; font-weight: 600; margin-top: 2px; }} .neg {{ color: #b91c1c; }}
 table {{ border-collapse: collapse; width: 100%; background: #fff; font-size: 12px; }}
 th, td {{ border: 1px solid #e2e8f0; padding: 4px 8px; text-align: right; }}
 th {{ background: #f1f5f9; text-align: right; }} td:first-child, th:first-child {{ text-align: left; }}
 .chart {{ background: #fff; border: 1px solid #e2e8f0; border-radius: 8px; padding: 8px; }}
 .conv {{ font-size: 11px; color: #94a3b8; margin-top: 16px; }}
</style></head><body>
<h1>Celis backtest report — {strategy}</h1>
<div class="sub">Symbols: {symbols} &nbsp;·&nbsp; Period: {period}</div>
{headline}
<h2>Equity curve</h2><div class="chart">{equity_svg}</div>
<h2>Drawdown</h2><div class="chart">{dd_svg}</div>
<h2>Monthly returns</h2>{monthly}
<h2>Trades</h2>{trades_rows}
<div class="conv">Conventions: {conventions}</div>
</body></html>"#,
        strategy = strategy,
        extra_head = extra_head,
        symbols = symbols,
        period = period,
        headline = headline,
        equity_svg = equity_svg,
        dd_svg = dd_svg,
        monthly = monthly,
        trades_rows = trades_rows,
        conventions = conventions,
    )
}

fn opt(v: Option<f64>, suffix: &str) -> String {
    match v {
        Some(x) => format!("{x:.2}{suffix}"),
        None => "n/a".into(),
    }
}

/// Simple deterministic SVG line chart (no JS).
pub fn sparkline_svg(values: &[f64], width: u32, height: u32, color: &str) -> String {
    if values.len() < 2 {
        return "<svg width=\"0\" height=\"0\"></svg>".into();
    }
    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).abs().max(1e-12);
    let w = width as f64;
    let h = height as f64;
    let pad = 4.0;
    let mut path = String::new();
    for (i, v) in values.iter().enumerate() {
        let x = pad + (w - 2.0 * pad) * (i as f64 / (values.len() - 1) as f64);
        let y = h - pad - (h - 2.0 * pad) * ((v - min) / span);
        path.push_str(&format!("{}{x:.1},{y:.1}", if i == 0 { "M" } else { "L" }));
    }
    let zero_y = if min < 0.0 {
        h - pad - (h - 2.0 * pad) * ((0.0 - min) / span)
    } else {
        -1.0
    };
    let zero_line = if min < 0.0 && zero_y > 0.0 {
        format!(
            r##"<line x1="{pad}" y1="{zero_y:.1}" x2="{x2}" y2="{zero_y:.1}" stroke="#e2e8f0" stroke-width="1"/>"##,
            x2 = w - pad
        )
    } else {
        String::new()
    };
    let max_label = format!("{max:.2}");
    let min_label = format!("{min:.2}");
    let y_min = h - 4.0;
    format!(
        r##"<svg viewBox="0 0 {w} {h}" width="{width}" height="{height}" xmlns="http://www.w3.org/2000/svg" role="img">
{zero_line}
<path d="{path}" fill="none" stroke="{color}" stroke-width="1.6"/>
<text x="6" y="12" font-size="10" fill="#94a3b8">{max_label}</text>
<text x="6" y="{y_min}" font-size="10" fill="#94a3b8">{min_label}</text>
</svg>"##,
        w = w,
        h = h,
        width = width,
        height = height,
        path = path,
        color = color,
    )
}

fn monthly_table(m: &MetricsReport) -> String {
    let mut html = String::from(
        r#"<table><tr><th>Year</th><th>Month</th><th>Return %</th><th>End equity</th></tr>"#,
    );
    for p in &m.monthly_returns {
        let cls = if p.return_pct < 0.0 {
            " class=\"neg\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td{}>{:.2}</td><td>{:.2}</td></tr>",
            p.year,
            p.month.map(|x| x.to_string()).unwrap_or_default(),
            cls,
            p.return_pct,
            p.end_equity,
        ));
    }
    html.push_str("</table>");
    html
}

/// Trade ledger rows as HTML (bounded).
pub fn trades_table_html(trades: &[bt_core::ledger::TradeRecord], max_rows: usize) -> String {
    let mut html = String::from(
        r#"<table><tr><th>#</th><th>Symbol</th><th>Dir</th><th>Entry</th><th>Exit</th><th>Qty</th><th>Net P&L</th><th>R</th><th>Reason</th></tr>"#,
    );
    for t in trades.iter().take(max_rows) {
        let cls = if t.net_pnl < D::ZERO {
            " class=\"neg\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td{}>{}</td><td>{}</td><td>{}</td></tr>",
            t.trade_id,
            t.symbol,
            t.direction,
            t.entry_price,
            t.exit_price,
            t.quantity,
            cls,
            t.net_pnl,
            t.r_multiple
                .map(|r| format!("{r:.2}"))
                .unwrap_or_default(),
            t.exit_reason,
        ));
    }
    if trades.len() > max_rows {
        html.push_str(&format!(
            "<tr><td colspan=\"9\">… {} more trades (see trades.csv)</td></tr>",
            trades.len() - max_rows
        ));
    }
    html.push_str("</table>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_renders_svg() {
        let svg = sparkline_svg(&[1.0, 2.0, 3.0, 2.5], 200, 80, "#000");
        assert!(svg.contains("<svg"));
        assert!(svg.contains("M"));
        assert!(svg.contains("stroke=\"#000\""));
    }

    #[test]
    fn report_is_self_contained() {
        let m = test_metrics();
        let html = render_run_report(
            &m,
            "demo",
            "X",
            &[
                ("2024-01-01".into(), 100.0, 0.0),
                ("2024-01-02".into(), 101.0, 1.0),
            ],
            &trades_table_html(&[], 50),
            "",
        );
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(!html.contains("http://cdn"), "no external assets");
        assert!(html.contains("Final equity"));
    }

    fn test_metrics() -> MetricsReport {
        let curve = vec![];
        bt_analytics::metrics::compute_metrics(&bt_analytics::metrics::MetricsInput {
            equity_curve: &curve,
            trades: &[],
            initial_capital: D::from(100000),
            bar_secs: 3600,
            config: &bt_analytics::AnalyticsConfig::default(),
            risk_of_ruin_pct: 0.0,
        })
    }
}
