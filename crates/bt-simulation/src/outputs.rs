//! Output writers (spec §27-28). Deterministic content in fixed order:
//! full-precision decimals in CSVs (no implicit rounding), JSON via serde.

use crate::engine::RunResult;
use bt_analytics::metrics::daily_equity_series;
use bt_core::error::CoreResult;
use bt_core::ledger::TradeRecord;
use bt_core::order::Order;
use bt_core::time::format_ts;
use std::io::Write;
use std::path::Path;

fn csv_err(e: csv::Error) -> bt_core::CoreError {
    bt_core::CoreError::InvalidData(format!("csv write error: {e}"))
}

fn write_csv(path: &Path, header: &[&str], rows: Vec<Vec<String>>) -> CoreResult<()> {
    let mut w = csv::WriterBuilder::new().from_path(path).map_err(csv_err)?;
    w.write_record(header).map_err(csv_err)?;
    for r in rows {
        w.write_record(&r).map_err(csv_err)?;
    }
    w.flush()
        .map_err(|e| bt_core::CoreError::InvalidData(format!("csv flush error: {e}")))?;
    Ok(())
}

fn opt_str<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_default()
}

/// Write the complete result set into `dir` (created if needed).
pub fn write_outputs(dir: &Path, run: &RunResult) -> CoreResult<()> {
    std::fs::create_dir_all(dir)?;
    write_trades(&dir.join("trades.csv"), &run.trades)?;
    write_orders(&dir.join("orders.csv"), &run.orders)?;
    write_fills(dir, run)?;
    write_equity(dir, run)?;
    write_returns(dir, run)?;
    write_json(
        &dir.join("metrics.json"),
        &serde_json::to_value(&run.metrics).map_err(json_err)?,
    )?;
    write_json(
        &dir.join("monte_carlo.json"),
        &serde_json::to_value(&run.monte_carlo).map_err(json_err)?,
    )?;
    write_json(&dir.join("experiment.json"), &run.experiment)?;
    write_json(&dir.join("summary.json"), &run.summary)?;
    // ledger: full audit of balance changes
    write_ledger(dir, run)?;
    Ok(())
}

fn json_err(e: serde_json::Error) -> bt_core::CoreError {
    bt_core::CoreError::InvalidData(format!("json write error: {e}"))
}

fn write_json(path: &Path, v: &serde_json::Value) -> CoreResult<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "{}", serde_json::to_string_pretty(v).map_err(json_err)?)?;
    Ok(())
}

fn write_trades(path: &Path, trades: &[TradeRecord]) -> CoreResult<()> {
    let rows = trades
        .iter()
        .map(|t| {
            vec![
                t.trade_id.to_string(),
                t.symbol.clone(),
                t.direction.clone(),
                format_ts(t.entry_timestamp),
                t.entry_price.to_string(),
                format_ts(t.exit_timestamp),
                t.exit_price.to_string(),
                t.quantity.to_string(),
                t.gross_pnl.to_string(),
                t.commission.to_string(),
                t.spread_cost.to_string(),
                t.slippage_cost.to_string(),
                t.financing.to_string(),
                t.net_pnl.to_string(),
                opt_str(&t.initial_risk),
                opt_str(&t.r_multiple),
                t.holding_bars.to_string(),
                t.holding_time_secs.to_string(),
                t.mae.to_string(),
                t.mfe.to_string(),
                t.entry_reason.clone(),
                t.exit_reason.clone(),
            ]
        })
        .collect();
    write_csv(
        path,
        &[
            "trade_id",
            "symbol",
            "direction",
            "entry_timestamp",
            "entry_price",
            "exit_timestamp",
            "exit_price",
            "quantity",
            "gross_pnl",
            "commission",
            "spread_cost",
            "slippage_cost",
            "financing",
            "net_pnl",
            "initial_risk",
            "r_multiple",
            "holding_bars",
            "holding_time_secs",
            "mae",
            "mfe",
            "entry_reason",
            "exit_reason",
        ],
        rows,
    )
}

fn write_orders(path: &Path, orders: &[Order]) -> CoreResult<()> {
    let rows = orders
        .iter()
        .map(|o| {
            vec![
                o.order_id.to_string(),
                o.strategy_id.clone(),
                o.symbol.clone(),
                o.side.as_str().to_string(),
                format!("{:?}", o.order_type).to_lowercase(),
                o.quantity.to_string(),
                opt_str(&o.limit_price),
                opt_str(&o.stop_price),
                format_ts(o.creation_ts),
                o.activation_ts.map(format_ts).unwrap_or_default(),
                o.expiration_ts.map(format_ts).unwrap_or_default(),
                format!("{:?}", o.status).to_lowercase(),
                o.filled_qty.to_string(),
                o.remaining_qty.to_string(),
                opt_str(&o.avg_fill_price),
                o.commission.to_string(),
                o.slippage.to_string(),
                opt_str(&o.parent_order_id),
                format!("{:?}", o.position_effect).to_lowercase(),
                format!("{:?}", o.reason).to_lowercase(),
            ]
        })
        .collect();
    write_csv(
        path,
        &[
            "order_id",
            "strategy_id",
            "symbol",
            "side",
            "order_type",
            "quantity",
            "limit_price",
            "stop_price",
            "creation_ts",
            "activation_ts",
            "expiration_ts",
            "status",
            "filled_qty",
            "remaining_qty",
            "avg_fill_price",
            "commission",
            "slippage",
            "parent_order_id",
            "position_effect",
            "reason",
        ],
        rows,
    )
}

fn write_fills(dir: &Path, run: &RunResult) -> CoreResult<()> {
    let rows = run
        .fills
        .iter()
        .map(|f| {
            vec![
                f.fill_id.to_string(),
                f.order_id.to_string(),
                format_ts(f.ts),
                f.symbol.clone(),
                f.side.clone(),
                f.quantity.to_string(),
                f.raw_price.to_string(),
                f.fill_price.to_string(),
                f.commission.to_string(),
                f.spread_cost.to_string(),
                f.slippage_cost.to_string(),
                f.notional.to_string(),
                f.reason.clone(),
            ]
        })
        .collect();
    write_csv(
        &dir.join("fills.csv"),
        &[
            "fill_id",
            "order_id",
            "ts",
            "symbol",
            "side",
            "quantity",
            "raw_price",
            "fill_price",
            "commission",
            "spread_cost",
            "slippage_cost",
            "notional",
            "reason",
        ],
        rows,
    )
}

fn write_equity(dir: &Path, run: &RunResult) -> CoreResult<()> {
    let eq_rows = run
        .equity_curve
        .iter()
        .map(|p| {
            vec![
                format_ts(p.ts),
                p.equity.to_string(),
                p.balance.to_string(),
                p.unrealized.to_string(),
                p.drawdown.to_string(),
                p.drawdown_pct.to_string(),
                if p.in_position { "1" } else { "0" }.to_string(),
            ]
        })
        .collect();
    write_csv(
        &dir.join("equity_curve.csv"),
        &[
            "ts",
            "equity",
            "balance",
            "unrealized",
            "drawdown",
            "drawdown_pct",
            "in_position",
        ],
        eq_rows,
    )?;
    let dd_rows = run
        .equity_curve
        .iter()
        .map(|p| {
            vec![
                format_ts(p.ts),
                p.drawdown.to_string(),
                p.drawdown_pct.to_string(),
            ]
        })
        .collect();
    write_csv(
        &dir.join("drawdown.csv"),
        &["ts", "drawdown", "drawdown_pct"],
        dd_rows,
    )?;
    Ok(())
}

fn write_returns(dir: &Path, run: &RunResult) -> CoreResult<()> {
    // daily returns (chained from last equity per UTC day)
    let daily = daily_equity_series(&run.equity_curve, bt_core::D::ZERO);
    let mut prev: Option<bt_core::D> = None;
    let rows = daily
        .iter()
        .map(|(ts, eq)| {
            let r = match prev {
                Some(p) if !p.is_zero() => {
                    bt_core::money::to_f64((*eq / p - bt_core::D::from(1)) * bt_core::D::from(100))
                }
                _ => 0.0,
            };
            prev = Some(*eq);
            vec![format_ts(*ts), format!("{r}")]
        })
        .collect();
    write_csv(&dir.join("daily_returns.csv"), &["ts", "return_pct"], rows)?;

    let monthly = run
        .metrics
        .monthly_returns
        .iter()
        .map(|m| {
            vec![
                m.year.to_string(),
                m.month.map(|x| x.to_string()).unwrap_or_default(),
                format!("{}", m.return_pct),
                format!("{}", m.end_equity),
            ]
        })
        .collect();
    write_csv(
        &dir.join("monthly_returns.csv"),
        &["year", "month", "return_pct", "end_equity"],
        monthly,
    )?;

    let yearly = run
        .metrics
        .yearly_returns
        .iter()
        .map(|y| {
            vec![
                y.year.to_string(),
                format!("{}", y.return_pct),
                format!("{}", y.end_equity),
            ]
        })
        .collect();
    write_csv(
        &dir.join("yearly_returns.csv"),
        &["year", "return_pct", "end_equity"],
        yearly,
    )?;

    let rolling = run
        .metrics
        .rolling_sharpe
        .iter()
        .map(|p| {
            vec![
                format_ts(p.ts),
                p.rolling_sharpe.map(|s| format!("{s}")).unwrap_or_default(),
            ]
        })
        .collect();
    write_csv(
        &dir.join("rolling_metrics.csv"),
        &["ts", "rolling_sharpe"],
        rolling,
    )?;
    Ok(())
}

fn write_ledger(dir: &Path, run: &RunResult) -> CoreResult<()> {
    let rows = run
        .ledger
        .iter()
        .map(|e| {
            vec![
                e.seq.to_string(),
                format_ts(e.ts),
                format!("{:?}", e.entry_type).to_lowercase(),
                e.amount.to_string(),
                e.balance_after.to_string(),
                e.reference.clone(),
            ]
        })
        .collect();
    write_csv(
        &dir.join("ledger.csv"),
        &[
            "seq",
            "ts",
            "entry_type",
            "amount",
            "balance_after",
            "reference",
        ],
        rows,
    )
}
