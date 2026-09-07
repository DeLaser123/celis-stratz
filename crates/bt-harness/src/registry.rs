//! Experiment registry (SQLite). The harness's memory: every run — singles,
//! sweep cells, walk-forward windows, stress scenarios — is recorded with its
//! content hashes and lineage so research decisions stay traceable.

use bt_core::error::{CoreError, CoreResult};
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Single,
    SweepCell,
    SweepManifest,
    WalkForwardWindow,
    WalkForwardManifest,
    StressCell,
    StressManifest,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Single => "single",
            RunKind::SweepCell => "sweep_cell",
            RunKind::SweepManifest => "sweep_manifest",
            RunKind::WalkForwardWindow => "walk_forward_window",
            RunKind::WalkForwardManifest => "walk_forward_manifest",
            RunKind::StressCell => "stress_cell",
            RunKind::StressManifest => "stress_manifest",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryRun {
    pub id: i64,
    pub created_at: String,
    pub experiment_id: String,
    pub result_hash: String,
    pub kind: RunKind,
    pub parent_id: Option<i64>,
    pub label: String,
    pub strategy_name: String,
    pub strategy_version: String,
    pub symbols: String,
    pub timeframe_secs: i64,
    pub data_hash: String,
    pub start_ts: String,
    pub end_ts: String,
    pub initial_capital: String,
    pub final_equity: f64,
    pub return_pct: f64,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub max_dd_pct: f64,
    pub profit_factor: Option<f64>,
    pub win_rate_pct: Option<f64>,
    pub trades: i64,
    pub out_dir: String,
    pub params_json: String,
}

pub struct Registry {
    conn: Connection,
}

impl Registry {
    pub fn open(path: &Path) -> CoreResult<Registry> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CoreError::Io)?;
        }
        let conn = Connection::open(path)
            .map_err(|e| CoreError::InvalidData(format!("open registry: {e}")))?;
        let reg = Registry { conn };
        reg.migrate();
        Ok(reg)
    }

    pub fn open_in_memory() -> CoreResult<Registry> {
        let conn = Connection::open_in_memory()
            .map_err(|e| CoreError::InvalidData(format!("open registry: {e}")))?;
        let reg = Registry { conn };
        reg.migrate();
        Ok(reg)
    }

    fn migrate(&self) {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS runs (
                    id               INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at       TEXT NOT NULL,
                    experiment_id    TEXT NOT NULL,
                    result_hash      TEXT NOT NULL DEFAULT '',
                    kind             TEXT NOT NULL,
                    parent_id        INTEGER REFERENCES runs(id),
                    label            TEXT NOT NULL DEFAULT '',
                    strategy_name    TEXT NOT NULL DEFAULT '',
                    strategy_version TEXT NOT NULL DEFAULT '',
                    symbols          TEXT NOT NULL DEFAULT '',
                    timeframe_secs   INTEGER NOT NULL DEFAULT 0,
                    data_hash        TEXT NOT NULL DEFAULT '',
                    start_ts         TEXT NOT NULL DEFAULT '',
                    end_ts           TEXT NOT NULL DEFAULT '',
                    initial_capital  TEXT NOT NULL DEFAULT '',
                    final_equity     REAL NOT NULL DEFAULT 0,
                    return_pct       REAL NOT NULL DEFAULT 0,
                    sharpe           REAL,
                    sortino          REAL,
                    max_dd_pct       REAL NOT NULL DEFAULT 0,
                    profit_factor    REAL,
                    win_rate_pct     REAL,
                    trades           INTEGER NOT NULL DEFAULT 0,
                    out_dir          TEXT NOT NULL DEFAULT '',
                    params_json      TEXT NOT NULL DEFAULT '{}'
                );
                CREATE INDEX IF NOT EXISTS idx_runs_kind ON runs(kind);
                CREATE INDEX IF NOT EXISTS idx_runs_parent ON runs(parent_id);
                CREATE TABLE IF NOT EXISTS tags (
                    run_id INTEGER NOT NULL REFERENCES runs(id),
                    tag    TEXT NOT NULL,
                    PRIMARY KEY (run_id, tag)
                );
                "#,
            )
            .expect("registry migrate");
    }

    /// Insert a run row; returns its registry id.
    pub fn insert(&self, r: &RegistryRun) -> CoreResult<i64> {
        self.conn
            .execute(
                r#"INSERT INTO runs (
                    created_at, experiment_id, result_hash, kind, parent_id, label,
                    strategy_name, strategy_version, symbols, timeframe_secs, data_hash,
                    start_ts, end_ts, initial_capital, final_equity, return_pct,
                    sharpe, sortino, max_dd_pct, profit_factor, win_rate_pct, trades,
                    out_dir, params_json
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)"#,
                rusqlite::params![
                    r.created_at,
                    r.experiment_id,
                    r.result_hash,
                    r.kind.as_str(),
                    r.parent_id,
                    r.label,
                    r.strategy_name,
                    r.strategy_version,
                    r.symbols,
                    r.timeframe_secs,
                    r.data_hash,
                    r.start_ts,
                    r.end_ts,
                    r.initial_capital,
                    r.final_equity,
                    r.return_pct,
                    r.sharpe,
                    r.sortino,
                    r.max_dd_pct,
                    r.profit_factor,
                    r.win_rate_pct,
                    r.trades,
                    r.out_dir,
                    r.params_json,
                ],
            )
            .map_err(|e| CoreError::InvalidData(format!("registry insert: {e}")))?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_tag(&self, run_id: i64, tag: &str) -> CoreResult<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO tags (run_id, tag) VALUES (?1, ?2)",
                rusqlite::params![run_id, tag],
            )
            .map_err(|e| CoreError::InvalidData(format!("registry tag: {e}")))?;
        Ok(())
    }

    fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegistryRun> {
        Ok(RegistryRun {
            id: row.get("id")?,
            created_at: row.get("created_at")?,
            experiment_id: row.get("experiment_id")?,
            result_hash: row.get("result_hash")?,
            kind: parse_kind(&row.get::<_, String>("kind")?),
            parent_id: row.get("parent_id")?,
            label: row.get("label")?,
            strategy_name: row.get("strategy_name")?,
            strategy_version: row.get("strategy_version")?,
            symbols: row.get("symbols")?,
            timeframe_secs: row.get("timeframe_secs")?,
            data_hash: row.get("data_hash")?,
            start_ts: row.get("start_ts")?,
            end_ts: row.get("end_ts")?,
            initial_capital: row.get("initial_capital")?,
            final_equity: row.get("final_equity")?,
            return_pct: row.get("return_pct")?,
            sharpe: row.get("sharpe")?,
            sortino: row.get("sortino")?,
            max_dd_pct: row.get("max_dd_pct")?,
            profit_factor: row.get("profit_factor")?,
            win_rate_pct: row.get("win_rate_pct")?,
            trades: row.get("trades")?,
            out_dir: row.get("out_dir")?,
            params_json: row.get("params_json")?,
        })
    }

    /// Most recent runs (optionally filtered by kind), newest first.
    pub fn list(&self, kind: Option<RunKind>, limit: usize) -> CoreResult<Vec<RegistryRun>> {
        let rows: Vec<RegistryRun> = match kind {
            None => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT * FROM runs ORDER BY id DESC LIMIT ?1")
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map(rusqlite::params![limit as i64], Self::row_to_run)
                    .map_err(sql_err)?;
                rows.flatten().collect()
            }
            Some(k) => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT * FROM runs WHERE kind = ?1 ORDER BY id DESC LIMIT ?2")
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![k.as_str(), limit as i64],
                        Self::row_to_run,
                    )
                    .map_err(sql_err)?;
                rows.flatten().collect()
            }
        };
        Ok(rows)
    }

    pub fn get(&self, id: i64) -> CoreResult<Option<RegistryRun>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM runs WHERE id = ?1")
            .map_err(sql_err)?;
        let mut rows = stmt
            .query_map(rusqlite::params![id], Self::row_to_run)
            .map_err(sql_err)?;
        rows.next().transpose().map_err(sql_err)
    }

    /// Full ancestor chain of a run (parent, grandparent, ...).
    pub fn lineage(&self, id: i64) -> CoreResult<Vec<RegistryRun>> {
        let mut out = Vec::new();
        let mut cur = id;
        for _ in 0..16 {
            let Some(run) = self.get(cur)? else { break };
            match run.parent_id {
                Some(p) => {
                    out.push(run);
                    cur = p;
                }
                None => {
                    out.push(run);
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn tags(&self, run_id: i64) -> CoreResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM tags WHERE run_id = ?1 ORDER BY tag")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params![run_id], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        Ok(rows.flatten().collect())
    }

    pub fn runs_with_tag(&self, tag: &str) -> CoreResult<Vec<RegistryRun>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT r.* FROM runs r JOIN tags t ON t.run_id = r.id \
                 WHERE t.tag = ?1 ORDER BY r.id DESC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(rusqlite::params![tag], Self::row_to_run)
            .map_err(sql_err)?;
        Ok(rows.flatten().collect())
    }

    /// Direct SQL access (read path used by the query engine).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

fn parse_kind(s: &str) -> RunKind {
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

fn sql_err(e: rusqlite::Error) -> CoreError {
    CoreError::InvalidData(format!("registry: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(exp: &str, kind: RunKind, parent: Option<i64>) -> RegistryRun {
        RegistryRun {
            id: 0,
            created_at: "2026-09-07T00:00:00Z".into(),
            experiment_id: exp.into(),
            result_hash: "rh".into(),
            kind,
            parent_id: parent,
            label: "cell".into(),
            strategy_name: "sma".into(),
            strategy_version: "v1".into(),
            symbols: "EURUSD".into(),
            timeframe_secs: 3600,
            data_hash: "dh".into(),
            start_ts: "2024-01-01T00:00:00Z".into(),
            end_ts: "2024-02-01T00:00:00Z".into(),
            initial_capital: "100000".into(),
            final_equity: 101_000.0,
            return_pct: 1.0,
            sharpe: Some(1.5),
            sortino: Some(2.5),
            max_dd_pct: 2.0,
            profit_factor: Some(1.3),
            win_rate_pct: Some(55.0),
            trades: 10,
            out_dir: "out".into(),
            params_json: "{}".into(),
        }
    }

    #[test]
    fn insert_list_lineage_tags() {
        let reg = Registry::open_in_memory().unwrap();
        let manifest = reg
            .insert(&sample("exp-manifest", RunKind::SweepManifest, None))
            .unwrap();
        let cell = reg
            .insert(&sample("exp-cell", RunKind::SweepCell, Some(manifest)))
            .unwrap();
        let single = reg
            .insert(&sample("exp-single", RunKind::Single, None))
            .unwrap();
        reg.set_tag(cell, "best").unwrap();

        assert_eq!(reg.list(Some(RunKind::SweepCell), 10).unwrap().len(), 1);
        assert_eq!(reg.list(None, 10).unwrap().len(), 3);

        let lineage = reg.lineage(cell).unwrap();
        assert_eq!(lineage.len(), 2, "cell -> manifest");
        assert_eq!(lineage[0].kind, RunKind::SweepCell);
        assert_eq!(lineage[1].kind, RunKind::SweepManifest);

        let tagged = reg.runs_with_tag("best").unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].id, cell);
        let _ = single;
    }
}
