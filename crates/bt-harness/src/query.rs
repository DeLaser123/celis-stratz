//! Query engine: run artifacts (CSV) and the registry exposed as SQLite
//! tables. This is the harness-only analytical layer — the kernel writes
//! artifacts, the harness makes them queryable with plain SQL.

use bt_core::error::{CoreError, CoreResult};
use rusqlite::Connection;
use std::path::Path;

/// Import one results directory (trades/orders/fills/equity_curve/
/// daily_returns CSVs) into an in-memory SQLite database.
pub fn open_run_db(dir: &Path) -> CoreResult<Connection> {
    let conn = Connection::open_in_memory().map_err(sql_err)?;
    conn.execute_batch(
        r#"
        CREATE TABLE equity_curve (ts TEXT, equity REAL, balance REAL, unrealized REAL,
                                   drawdown REAL, drawdown_pct REAL, in_position INTEGER);
        CREATE TABLE trades (trade_id INTEGER, symbol TEXT, direction TEXT,
                             entry_timestamp TEXT, entry_price REAL, exit_timestamp TEXT,
                             exit_price REAL, quantity REAL, gross_pnl REAL,
                             commission REAL, spread_cost REAL, slippage_cost REAL,
                             financing REAL, net_pnl REAL, initial_risk REAL, r_multiple REAL,
                             holding_bars INTEGER, holding_time_secs INTEGER, mae REAL, mfe REAL,
                             entry_reason TEXT, exit_reason TEXT);
        CREATE TABLE fills (fill_id INTEGER, order_id INTEGER, ts TEXT, symbol TEXT, side TEXT,
                            quantity REAL, raw_price REAL, fill_price REAL, commission REAL,
                            spread_cost REAL, slippage_cost REAL, notional REAL, reason TEXT);
        CREATE TABLE orders (order_id INTEGER, symbol TEXT, side TEXT, order_type TEXT,
                             quantity REAL, creation_ts TEXT, status TEXT,
                             avg_fill_price REAL, reason TEXT);
        CREATE TABLE daily_returns (ts TEXT, return_pct REAL);
        "#,
    )
    .map_err(sql_err)?;

    import_csv(
        &conn,
        &dir.join("equity_curve.csv"),
        "equity_curve",
        &[
            "ts",
            "equity",
            "balance",
            "unrealized",
            "drawdown",
            "drawdown_pct",
            "in_position",
        ],
    )?;
    import_csv(
        &conn,
        &dir.join("trades.csv"),
        "trades",
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
    )?;
    import_csv(
        &conn,
        &dir.join("fills.csv"),
        "fills",
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
    )?;
    import_csv(
        &conn,
        &dir.join("orders.csv"),
        "orders",
        &[
            "order_id",
            "symbol",
            "side",
            "order_type",
            "quantity",
            "creation_ts",
            "status",
            "avg_fill_price",
            "reason",
        ],
    )?;
    import_csv(
        &conn,
        &dir.join("daily_returns.csv"),
        "daily_returns",
        &["ts", "return_pct"],
    )?;
    Ok(conn)
}

/// Open the registry database (read-only usage).
pub fn open_registry_db(path: &Path) -> CoreResult<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| CoreError::InvalidData(format!("open registry: {e}")))
}

/// Execute a SELECT and return (columns, rows-as-strings). Non-SELECT
/// statements are rejected: the query engine is read-only.
pub fn run_select(conn: &Connection, sql: &str) -> CoreResult<(Vec<String>, Vec<Vec<String>>)> {
    let trimmed = sql.trim_start().to_lowercase();
    if !trimmed.starts_with("select") && !trimmed.starts_with("with") {
        return Err(CoreError::InvalidData(
            "only SELECT/WITH queries are allowed (query engine is read-only)".into(),
        ));
    }
    let mut stmt = conn.prepare(sql).map_err(sql_err)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let n = cols.len();
    let mut rows_out: Vec<Vec<String>> = Vec::new();
    let mut rows = stmt.query([]).map_err(sql_err)?;
    while let Some(row) = rows.next().map_err(sql_err)? {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let v = row.get_ref(i).map_err(sql_err)?;
            let s = match v {
                rusqlite::types::ValueRef::Null => String::new(),
                rusqlite::types::ValueRef::Integer(i) => i.to_string(),
                rusqlite::types::ValueRef::Real(f) => format!("{f}"),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).to_string(),
                rusqlite::types::ValueRef::Blob(b) => String::from_utf8_lossy(b).to_string(),
            };
            out.push(s);
        }
        rows_out.push(out);
    }
    Ok((cols, rows_out))
}

fn import_csv(conn: &Connection, path: &Path, table: &str, columns: &[&str]) -> CoreResult<()> {
    if !path.exists() {
        return Ok(()); // optional artifacts are simply absent
    }
    let text = std::fs::read_to_string(path).map_err(CoreError::Io)?;
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| CoreError::InvalidData(format!("{}: {e}", path.display())))?
        .clone();
    let placeholders = columns.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({placeholders})",
        columns.join(",")
    );
    let mut insert = conn.prepare(&sql).map_err(sql_err)?;
    for rec in reader.records() {
        let rec = rec.map_err(|e| CoreError::InvalidData(format!("{}: {e}", path.display())))?;
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(columns.len());
        for c in columns {
            let idx = headers
                .iter()
                .position(|h| h.trim().eq_ignore_ascii_case(c));
            let raw = idx.and_then(|i| rec.get(i)).unwrap_or("");
            let val = if raw.is_empty() {
                rusqlite::types::Value::Null
            } else if let Ok(i) = raw.parse::<i64>() {
                rusqlite::types::Value::Integer(i)
            } else if let Ok(f) = raw.parse::<f64>() {
                rusqlite::types::Value::Real(f)
            } else {
                rusqlite::types::Value::Text(raw.to_string())
            };
            params.push(val);
        }
        insert
            .execute(rusqlite::params_from_iter(params))
            .map_err(sql_err)?;
    }
    Ok(())
}

fn sql_err(e: rusqlite::Error) -> CoreError {
    CoreError::InvalidData(format!("query: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bt_query_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn query_run_artifacts() {
        let dir = tmpdir("run");
        let mut f = std::fs::File::create(dir.join("trades.csv")).unwrap();
        writeln!(f, "trade_id,symbol,direction,entry_timestamp,entry_price,exit_timestamp,exit_price,quantity,gross_pnl,commission,spread_cost,slippage_cost,financing,net_pnl,initial_risk,r_multiple,holding_bars,holding_time_secs,mae,mfe,entry_reason,exit_reason").unwrap();
        writeln!(f, "1,X,long,2024-01-01T00:00:00Z,100,2024-01-02T00:00:00Z,110,1,10,0,0,0,0,10,5,2,24,86400,2,10,entry,exit").unwrap();
        writeln!(f, "2,X,long,2024-01-03T00:00:00Z,100,2024-01-04T00:00:00Z,95,1,-5,0,0,0,0,-5,5,-1,24,86400,5,1,entry,stop_loss").unwrap();
        let conn = open_run_db(&dir).unwrap();
        let (cols, rows) = run_select(
            &conn,
            "SELECT trade_id, net_pnl FROM trades WHERE net_pnl > 0",
        )
        .unwrap();
        assert_eq!(cols, vec!["trade_id", "net_pnl"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "1");
        // read-only guard
        assert!(run_select(&conn, "DELETE FROM trades").is_err());
    }
}
