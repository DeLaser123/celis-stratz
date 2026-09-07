//! Shared helpers for the end-to-end simulation test suites.

#![allow(dead_code)]

use bt_core::event::VecSink;
use bt_core::time::Ts;
use bt_core::D;
use bt_data::Dataset;
use bt_simulation::config::EngineConfig;
use bt_simulation::engine::{RunResult, SimulationEngine};
use bt_strategy::spec::StrategySpec;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_ID: AtomicU64 = AtomicU64::new(1);

pub fn write_temp_csv(name: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("bt_sim_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let id = TMP_ID.fetch_add(1, Ordering::SeqCst);
    let path = dir.join(format!("{name}_{id}.csv"));
    std::fs::write(&path, content).unwrap();
    path
}

pub const HEADER: &str = "timestamp,symbol,open,high,low,close,volume\n";

pub fn bar(ts: &str, o: &str, h: &str, l: &str, c: &str) -> String {
    format!("{ts},X,{o},{h},{l},{c},10\n")
}

/// Parse spec YAML text.
pub fn spec(text: &str) -> StrategySpec {
    bt_strategy::spec::parse_spec(text).expect("strategy spec must parse")
}

/// Long entry-always strategy with fixed quantity sizing.
pub fn fixed_qty_long(qty: i64) -> StrategySpec {
    spec(&format!(
        r#"
strategy:
  name: entry_always
  symbols: [X]
  entry:
    direction: long
    when: {{gt: [{{field: close}}, 0]}}
  risk:
    sizing: {{mode: fixed_quantity, qty: {qty}}}
"#
    ))
}

/// Short entry-always strategy with fixed quantity sizing.
pub fn fixed_qty_short(qty: i64) -> StrategySpec {
    spec(&format!(
        r#"
strategy:
  name: entry_always_short
  symbols: [X]
  entry:
    direction: short
    when: {{gt: [{{field: close}}, 0]}}
  risk:
    sizing: {{mode: fixed_quantity, qty: {qty}}}
"#
    ))
}

/// Run the engine over a CSV string.
pub fn run(data_csv: &str, strategy: &StrategySpec, cfg: &EngineConfig) -> (RunResult, VecSink) {
    let path = write_temp_csv("data", data_csv);
    let dataset: Dataset = bt_data::csv::load_bars_csv(
        &path,
        "UTC".parse().unwrap(),
        bt_data::TimestampConvention::Open,
        cfg.market.validation_mode,
        bt_data::LoadLimits::default(),
        Some(3600),
    )
    .expect("data must validate");
    let mut sink = VecSink::new();
    let signals = std::collections::BTreeMap::new();
    let run = SimulationEngine::run(&dataset, strategy, cfg, &signals, &mut sink)
        .expect("simulation must succeed");
    (run, sink)
}

pub fn ts_of(s: &str) -> Ts {
    let utc: chrono_tz::Tz = "UTC".parse().unwrap();
    bt_core::time::parse_timestamp(s, utc, "t").unwrap()
}

pub fn d(v: i64) -> D {
    D::from(v)
}

/// Normalized decimal string for deterministic assertions.
pub fn ns(d: D) -> String {
    d.normalize().to_string()
}
