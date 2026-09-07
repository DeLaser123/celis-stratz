//! Golden end-to-end tests: small synthetic datasets where the correct answer
//! is known exactly (spec §31 fixtures + TESTING.md golden layer).

mod common;

use bt_simulation::config::{EngineConfig, ExecutionModel};
use common::ns;
use common::*;

#[test]
fn case_a_buy_at_100_sell_at_110_exact_pnl() {
    // Spec test 1: buy 1 @100, sell @110 => gross 10, equity 100010 (0 costs).
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "99", "110"),
    );
    let s = fixed_qty_long(1);
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1, "exactly one trade");
    let t = &run.trades[0];
    assert_eq!(t.direction, "long");
    assert_eq!(ns(t.entry_price), "100");
    assert_eq!(ns(t.exit_price), "110");
    assert_eq!(ns(t.quantity), "1");
    assert_eq!(ns(t.gross_pnl), "10");
    assert_eq!(ns(t.net_pnl), "10");
    assert_eq!(t.exit_reason, "end_of_data");
    let final_equity = run.equity_curve.last().unwrap().equity;
    assert_eq!(ns(final_equity), "100010");
    // ledger conservation: initial + realized = final balance
    assert_eq!(ns(run.ledger.last().unwrap().balance_after), "100010");
}

#[test]
fn case_b_ambiguous_bar_conservative_policy_takes_stop() {
    // Spec test 7 + test 2: long @100, SL 95, TP 110; one bar touches both.
    // Conservative (default) policy => stop fills FIRST: P&L -5, R = -1.
    let data = format!(
        "{HEADER}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "94", "105"),
        bar("2024-01-01T03:00:00Z", "100", "112", "99", "108"),
    );
    let s2 = spec(
        r#"
strategy:
  name: entry_always
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    stop_loss: {type: fixed_distance, value: 5}
    take_profit: {type: fixed_distance, value: 10}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s2, &cfg);
    // trade 1: stopped out on the ambiguous bar (SL first). trade 2: the flat
    // t3 decision re-enters (entry-always) and closes at data end.
    assert_eq!(run.trades.len(), 2);
    let t = &run.trades[0];
    assert_eq!(t.exit_reason, "stop_loss");
    assert_eq!(ns(t.exit_price), "95");
    assert_eq!(ns(t.net_pnl), "-5");
    assert_eq!(ns(t.initial_risk.unwrap()), "5");
    assert_eq!(ns(t.r_multiple.unwrap()), "-1");
    // the t3 re-entry (fill 100) takes its own TP (110) intrabar on t3
    assert_eq!(ns(run.trades[1].net_pnl), "10");
    // final equity 100000 - 5 + 10 = 100005
    assert_eq!(ns(run.equity_curve.last().unwrap().equity), "100005");
}

#[test]
fn case_b2_ambiguous_bar_optimistic_policy_takes_target() {
    let data = format!(
        "{HEADER}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "94", "105"),
        bar("2024-01-01T03:00:00Z", "100", "112", "99", "108"),
    );
    let s2 = spec(
        r#"
strategy:
  name: entry_always
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    stop_loss: {type: fixed_distance, value: 5}
    take_profit: {type: fixed_distance, value: 10}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let mut cfg = EngineConfig::default();
    cfg.execution.intrabar_policy = bt_execution::AmbiguityPolicy::Optimistic;
    let (run, _sink) = run(&data, &s2, &cfg);
    assert_eq!(run.trades.len(), 2, "tp exit + re-entry closed at data end");
    let t = &run.trades[0];
    assert_eq!(t.exit_reason, "take_profit");
    assert_eq!(ns(t.exit_price), "110");
    assert_eq!(ns(t.net_pnl), "10");
    assert_eq!(ns(t.r_multiple.unwrap()), "2");
    assert_eq!(ns(run.trades[1].net_pnl), "10");
    assert_eq!(ns(run.equity_curve.last().unwrap().equity), "100020");
}

#[test]
fn case_b3_ambiguous_bar_reject_policy_defers_exit() {
    // policy=reject must NOT guess: exit deferred to the next unambiguous bar.
    let data = format!(
        "{HEADER}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "94", "105"),
        bar("2024-01-01T03:00:00Z", "100", "112", "99", "108"),
    );
    let s2 = spec(
        r#"
strategy:
  name: entry_always
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    stop_loss: {type: fixed_distance, value: 5}
    take_profit: {type: fixed_distance, value: 10}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let mut cfg = EngineConfig::default();
    cfg.execution.intrabar_policy = bt_execution::AmbiguityPolicy::Reject;
    let (run, sink) = run(&data, &s2, &cfg);
    assert_eq!(run.trades.len(), 1);
    let t = &run.trades[0];
    // deferred past the ambiguous bar; exits on t3 at the target
    assert_eq!(t.exit_reason, "take_profit");
    assert_eq!(
        t.exit_timestamp,
        ts_of("2024-01-01T03:00:00Z"),
        "exit happens on the NEXT bar after the ambiguous one"
    );
    // the deferral was logged
    assert!(sink
        .events
        .iter()
        .any(|e| format!("{:?}", e.event).contains("AmbiguityDeferred")));
}

#[test]
fn case_c_costs_charged_and_attributed_separately() {
    // Spec tests 4-6: commission, spread, slippage all charged, decomposed.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "99", "110"),
    );
    let s = fixed_qty_long(10);
    let mut cfg = EngineConfig::default();
    cfg.costs.spread = bt_execution::SpreadModel::Fixed {
        spread: d_from_str("0.02"),
    };
    cfg.costs.slippage = bt_execution::SlippageModel::Fixed {
        value: d_from_str("0.01"),
    };
    cfg.costs.commission = bt_execution::CommissionModel::Fixed {
        per_order: d(1),
        per_unit: Some(d_from_str("0.5")),
    };
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    let t = &run.trades[0];
    // raw market prices: entry 100, exit 110 (costs attributed separately)
    assert_eq!(ns(t.entry_price), "100");
    assert_eq!(ns(t.exit_price), "110");
    // gross uses RAW prices: (110-100)*10 = 100
    assert_eq!(ns(t.gross_pnl), "100");
    // commission: 2 * (1 + 0.5*10) = 12
    assert_eq!(ns(t.commission), "12");
    // spread: 0.01 * 10 * 2 fills = 0.2
    assert_eq!(ns(t.spread_cost), "0.2");
    // slippage: 0.01 * 10 * 2 fills = 0.2
    assert_eq!(ns(t.slippage_cost), "0.2");
    // net: 100 - 12 - 0.2 - 0.2 = 87.6
    assert_eq!(ns(t.net_pnl), "87.6");
    assert_eq!(ns(run.equity_curve.last().unwrap().equity), "100087.6");
    // fills.csv data decomposes raw vs fill price
    assert_eq!(run.fills.len(), 2);
    assert_eq!(ns(run.fills[0].raw_price), "100");
    assert_eq!(ns(run.fills[0].fill_price), "100.02");
    assert_eq!(ns(run.fills[1].fill_price), "109.98");
}

#[test]
fn case_d_short_roundtrip_positive_pnl() {
    // Spec test 3: short @100, cover @90 => +10.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "101", "89", "90"),
    );
    let s = fixed_qty_short(1);
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    let t = &run.trades[0];
    assert_eq!(t.direction, "short");
    assert_eq!(ns(t.entry_price), "100");
    assert_eq!(ns(t.exit_price), "90");
    assert_eq!(ns(t.net_pnl), "10");
    assert_eq!(ns(run.equity_curve.last().unwrap().equity), "100010");
}

#[test]
fn case_e_percent_risk_sizing_floors_to_step() {
    // Spec test 15: equity 100k, risk 1% => 1000; distance 5 => 200 units.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "99", "105"),
    );
    let s2 = spec(
        r#"
strategy:
  name: entry_always
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    stop_loss: {type: fixed_distance, value: 5}
  risk:
    sizing: {mode: percent_risk, value: 1.0}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s2, &cfg);
    assert_eq!(run.trades.len(), 1);
    assert_eq!(ns(run.trades[0].quantity), "200");
    // risk-based sizing: stop anchored at fill - 5
    let t = &run.trades[0];
    assert_eq!(ns(t.initial_risk.unwrap()), "1000");
}

#[test]
fn case_f_future_data_cannot_alter_completed_trades() {
    // Spec test 10: a trade completed on the prefix is identical whether the
    // extra bars exist or not.
    let prefix = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "99", "110"),
    );
    let full = format!(
        "{HEADER}{}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "111", "99", "110"),
        bar("2024-01-01T03:00:00Z", "110", "120", "80", "85"),
        bar("2024-01-01T04:00:00Z", "85", "90", "70", "75"),
    );
    let s2 = spec(
        r#"
strategy:
  name: entry_always
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    stop_loss: {type: fixed_distance, value: 5}
    take_profit: {type: fixed_distance, value: 10}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (short_run, _) = run(&prefix, &s2, &cfg);
    let (full_run, _) = run(&full, &s2, &cfg);
    assert_eq!(short_run.trades.len(), 1);
    // the full run legitimately re-enters after the completed trade; the
    // trades COMPLETED WITHIN THE PREFIX must be identical
    assert!(!full_run.trades.is_empty());
    let a = &short_run.trades[0];
    let b = &full_run.trades[0];
    assert_eq!(a.entry_timestamp, b.entry_timestamp);
    assert_eq!(a.exit_timestamp, b.exit_timestamp);
    assert_eq!(a.entry_price, b.entry_price);
    assert_eq!(a.exit_price, b.exit_price);
    assert_eq!(a.net_pnl, b.net_pnl);
    // equity history up to the shared point is identical too
    assert_eq!(
        short_run.equity_curve.last().unwrap().equity,
        full_run.equity_curve[2].equity
    );
}

#[test]
fn case_g_position_reversal() {
    // Spec test 13: long signal while short... here: short signal while long
    // reverses through the netting engine; two trade records result.
    let data = format!(
        "{HEADER}{}{}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "101"),
        bar("2024-01-01T02:00:00Z", "101", "105", "100", "104"),
        bar("2024-01-01T03:00:00Z", "104", "107", "103", "106"),
        bar("2024-01-01T04:00:00Z", "106", "107", "103", "104"),
    );
    let s2 = spec(
        r#"
strategy:
  name: reversal
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  entry_short:
    direction: short
    when: {gt: [{field: close}, 105]}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s2, &cfg);
    assert_eq!(run.trades.len(), 2, "long trip then short trip");
    let long = &run.trades[0];
    assert_eq!(long.direction, "long");
    assert_eq!(long.exit_reason, "reversal");
    assert_eq!(ns(long.net_pnl), "6", "long 100 -> 106");
    let short = &run.trades[1];
    assert_eq!(short.direction, "short");
    assert_eq!(ns(short.entry_price), "106");
    assert_eq!(short.exit_reason, "end_of_data");
    assert_eq!(ns(short.net_pnl), "2", "short 106 -> 104");
    assert_eq!(ns(run.equity_curve.last().unwrap().equity), "100008");
}

#[test]
fn case_h_two_symbols_independent_positions() {
    // Spec test 14: multiple concurrent positions across symbols.
    let data = format!(
        "{HEADER}{}{}{}{}{}{}",
        "2024-01-01T00:00:00Z,X,100,100,100,100,10\n",
        "2024-01-01T00:00:00Z,Y,200,200,200,200,10\n",
        "2024-01-01T01:00:00Z,X,100,101,99,101,10\n",
        "2024-01-01T01:00:00Z,Y,200,201,199,201,10\n",
        "2024-01-01T02:00:00Z,X,101,106,100,105,10\n",
        "2024-01-01T02:00:00Z,Y,201,202,190,195,10\n",
    );
    let s2 = spec(
        r#"
strategy:
  name: both
  symbols: [X, Y]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s2, &cfg);
    assert_eq!(run.trades.len(), 2);
    let xt = run.trades.iter().find(|t| t.symbol == "X").unwrap();
    let yt = run.trades.iter().find(|t| t.symbol == "Y").unwrap();
    assert_eq!(ns(xt.net_pnl), "5", "X 100 -> 105");
    assert_eq!(ns(yt.net_pnl), "-5", "Y 200 -> 195");
    assert_eq!(ns(run.equity_curve.last().unwrap().equity), "100000");
}

#[test]
fn case_i_current_close_execution_model() {
    // current_close model fills the entry at the signal bar's close itself.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "101"),
        bar("2024-01-01T02:00:00Z", "101", "111", "100", "110"),
    );
    let s = fixed_qty_long(1);
    let mut cfg = EngineConfig::default();
    cfg.execution.model = ExecutionModel::CurrentClose;
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    let t = &run.trades[0];
    // filled at t0 close (100) rather than t1 open; the execution timestamp
    // is the decision timestamp (= t0's close time = 01:00).
    assert_eq!(t.entry_timestamp, ts_of("2024-01-01T01:00:00Z"));
    assert_eq!(ns(t.entry_price), "100");
}

fn d_from_str(s: &str) -> bt_core::D {
    s.parse().unwrap()
}
