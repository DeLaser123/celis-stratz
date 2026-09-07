//! Determinism, look-ahead, and differential tests (spec §3, §17, §18,
//! TESTING.md layers 5-7).

mod common;

use bt_simulation::config::{EngineConfig, IndicatorModeConfig};
use bt_strategy::runtime::IndicatorMode;
use common::*;

#[test]
fn same_inputs_produce_identical_result_hashes() {
    // Spec test 8: two identical executions produce identical result hashes.
    let data = format!(
        "{HEADER}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "102", "98", "101"),
        bar("2024-01-01T01:00:00Z", "101", "103", "100", "102"),
        bar("2024-01-01T02:00:00Z", "102", "104", "99", "100"),
        bar("2024-01-01T03:00:00Z", "100", "103", "99", "103"),
    );
    let s = fixed_qty_long(1);
    let cfg = EngineConfig::default();
    let (a, _) = run(&data, &s, &cfg);
    let (b, _) = run(&data, &s, &cfg);
    assert_eq!(a.result_hash, b.result_hash);
    assert_eq!(a.experiment_id, b.experiment_id);
}

#[test]
fn shuffled_rows_produce_identical_results() {
    // Spec test 9: data shuffled => deterministic normalized result.
    let ordered = format!(
        "{HEADER}{}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "102", "98", "101"),
        bar("2024-01-01T01:00:00Z", "101", "103", "100", "102"),
        bar("2024-01-01T02:00:00Z", "102", "104", "99", "100"),
        bar("2024-01-01T03:00:00Z", "100", "103", "99", "103"),
        bar("2024-01-01T04:00:00Z", "103", "105", "102", "104"),
    );
    let shuffled = format!(
        "{HEADER}{}{}{}{}{}",
        bar("2024-01-01T03:00:00Z", "100", "103", "99", "103"),
        bar("2024-01-01T00:00:00Z", "100", "102", "98", "101"),
        bar("2024-01-01T04:00:00Z", "103", "105", "102", "104"),
        bar("2024-01-01T01:00:00Z", "101", "103", "100", "102"),
        bar("2024-01-01T02:00:00Z", "102", "104", "99", "100"),
    );
    let s = fixed_qty_long(1);
    let mut cfg = EngineConfig::default();
    cfg.market.validation_mode = bt_data::ValidationMode::Permissive; // sorting allowed
    let (a, _) = run(&ordered, &s, &cfg);
    let (b, _) = run(&shuffled, &s, &cfg);
    assert_eq!(a.result_hash, b.result_hash);
}

#[test]
fn precompute_and_streaming_indicator_paths_agree() {
    // Differential test (spec §37): the optimized (precompute) path must give
    // exactly the reference (streaming) results.
    let data = format!(
        "{HEADER}{}",
        (0..48)
            .map(|i| {
                let ts = format!("2024-01-0{}T{:02}:00:00Z", 1 + i / 24, i % 24);
                let c = 100 + (i % 7) as i64 - 3;
                bar(
                    &ts,
                    &c.to_string(),
                    &(c + 1).to_string(),
                    &(c - 1).to_string(),
                    &c.to_string(),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    );
    let s2 = spec(
        r#"
strategy:
  name: sma_cross
  symbols: [X]
  entry:
    direction: long
    when:
      cross_above: [{field: close}, {sma: {source: {field: close}, period: 5}}]
  exit:
    when:
      cross_below: [{field: close}, {sma: {source: {field: close}, period: 5}}]
  orders:
    stop_loss: {type: fixed_distance, value: 2}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (a, _) = run(&data, &s2, &cfg);
    let mut cfg2 = cfg.clone();
    cfg2.runtime.indicator_mode = IndicatorModeConfig::Streaming;
    let (b, _) = run(&data, &s2, &cfg2);
    assert_eq!(
        a.result_hash, b.result_hash,
        "precompute vs streaming must match"
    );
    // sanity: the strategy actually traded
    assert!(
        a.trades.len() >= 2,
        "expected multiple round trips, got {}",
        a.trades.len()
    );
    let _ = IndicatorMode::Precompute;
}

#[test]
fn higher_timeframe_values_hidden_until_daily_close() {
    // Spec test 11: the daily close of day 1 must be invisible until the last
    // hourly bar of day 1 closes (decision ts = Jan 2 00:00).
    let mut rows = String::from(HEADER);
    for h in 0..24 {
        let ts = format!("2024-01-01T{:02}:00:00Z", h);
        rows.push_str(&bar(&ts, "100", "101", "99", "100"));
    }
    // Jan 2 00:00 open bar (fill bar) + one more for end handling
    rows.push_str(&bar("2024-01-02T00:00:00Z", "100", "101", "99", "100"));
    rows.push_str(&bar("2024-01-02T01:00:00Z", "100", "101", "99", "100"));

    // Entry requires the DAILY close > 1.4. Day 1 daily close = 100 (last close
    // of the bucket). If the daily value leaked early, the signal would fire on
    // day 1; it must fire at the decision ts Jan 2 00:00 -> fill Jan 2 01:00.
    let s2 = spec(
        r#"
strategy:
  name: daily_gate
  symbols: [X]
  timeframes: [1D]
  entry:
    direction: long
    when: {gt: [{timeframe: 1D, of: {field: close}}, 1.4]}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&rows, &s2, &cfg);
    assert_eq!(run.trades.len(), 1);
    let fill_ts = run.fills[0].ts;
    assert_eq!(
        fill_ts,
        ts_of("2024-01-02T00:00:00Z"),
        "first fill must be at the first bar AFTER the daily close becomes observable \
         (decision Jan 2 00:00 -> next bar open is Jan 2 00:00)"
    );
}

#[test]
fn external_signals_respect_timestamps() {
    // Spec §18: a signal observed at 02:00 cannot influence earlier decisions.
    let data = format!(
        "{HEADER}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T03:00:00Z", "100", "101", "99", "100"),
    );
    let mut signals = std::collections::BTreeMap::new();
    // observation only at 02:00 (i.e., becomes available for the 02:00 decision)
    signals.insert(
        "bias".to_string(),
        vec![(ts_of("2024-01-01T02:00:00Z"), bt_core::D::from(1))],
    );
    let path = write_temp_csv("data", &data);
    let dataset = bt_data::csv::load_bars_csv(
        &path,
        "UTC".parse().unwrap(),
        bt_data::TimestampConvention::Open,
        bt_data::ValidationMode::Strict,
        bt_data::LoadLimits::default(),
        Some(3600),
    )
    .unwrap();
    let s = spec(s2_text());
    let cfg = EngineConfig::default();
    let mut sink = bt_core::event::VecSink::new();
    let run =
        bt_simulation::SimulationEngine::run(&dataset, &s, &cfg, &signals, &mut sink).unwrap();
    assert_eq!(run.trades.len(), 1);
    // the 02:00 observation is available to the decision AT 02:00 (bar
    // 01:00's close) -> next_open fill at the 02:00 bar. Any EARLIER fill
    // would be look-ahead.
    assert_eq!(run.fills[0].ts, ts_of("2024-01-01T02:00:00Z"));
}

fn s2_text() -> &'static str {
    r#"
strategy:
  name: signal_entry
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{signal: {key: bias}}, 0.5]}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#
}

#[test]
fn margin_violation_and_risk_rejections_are_logged() {
    // Risk engine blocks a notional that exceeds the configured cap; the
    // rejection must appear in the event log (auditability).
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "101", "99", "100"),
    );
    let s2 = spec(
        r#"
strategy:
  name: big_entry
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: percent_equity, value: 500}
"#,
    );
    let mut cfg = EngineConfig::default();
    cfg.risk.max_position_notional_pct = d(100);
    let (run, sink) = run(&data, &s2, &cfg);
    assert!(run.trades.is_empty(), "must be blocked");
    assert!(sink
        .events
        .iter()
        .any(|e| format!("{:?}", e.event).contains("RiskRejected")));
}
