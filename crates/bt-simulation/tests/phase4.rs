//! Phase-4 feature tests: entry order types, participation-capped partial
//! fills, corporate actions (dividends + splits), cross-symbol access,
//! volume-aware slippage.

mod common;

use bt_simulation::config::EngineConfig;
use common::*;

#[test]
fn limit_buy_entry_fills_only_when_price_reaches() {
    // Stop-entry spec: buy limit at 99. The bar range (98..101) touches it on
    // bar 1 only if the low reaches 99; use a limit at 99.5 with lows above.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "101", "102", "100.5", "101"), // low never <= 99.5
        bar("2024-01-01T02:00:00Z", "100", "101", "99", "100.5"),  // low 99 <= limit
    );
    let s = spec(
        r#"
strategy:
  name: limit_entry
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    entry: {type: limit, price: 99.5}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    let t = &run.trades[0];
    // limit fill: trigger low <= 99.5 on bar 2, fill at min(limit, open) = 99.5... open=100 > 99.5 => fill at limit
    assert_eq!(ns(t.entry_price), "99.5");
    // position closed at data end
    assert_eq!(t.exit_reason, "end_of_data");
}

#[test]
fn limit_entry_never_chases() {
    // Limit at 98 but the low never reaches it -> no trade at all.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "102", "99.5", "101"),
        bar("2024-01-01T02:00:00Z", "101", "103", "100", "102"),
    );
    let s = spec(
        r#"
strategy:
  name: limit_never
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    entry: {type: limit, price: 98}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert!(run.trades.is_empty(), "limit never touched => no trade");
}

#[test]
fn stop_buy_entry_fills_when_price_breaks_up() {
    // Buy stop at 101.5: bar 1 high 102 triggers; open 100 below => fill at stop.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "102", "99", "101"),
        bar("2024-01-01T02:00:00Z", "101", "104", "100", "103"),
    );
    let s = spec(
        r#"
strategy:
  name: stop_entry
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    entry: {type: stop, price: 101.5}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    assert_eq!(ns(run.trades[0].entry_price), "101.5");
}

#[test]
fn stop_entry_expression_price() {
    // Entry stop priced at the previous bar's high via an expression:
    // {stop price: {add: [{field: high}, 1]}}. Bar 1 high=102 => stop 103 on
    // later bars; bar 2 high 104 triggers.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "102", "99", "101"),
        bar("2024-01-01T02:00:00Z", "101", "104", "100", "103"),
    );
    let s = spec(
        r#"
strategy:
  name: expr_stop
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  orders:
    entry: {type: stop, price: {add: [{field: high}, 1]}}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    // Decision on bar 0 (close=100): price = bar 0 high (100) + 1 = 101.
    // The order is created ONCE with that fixed price; bar 1 (high 102)
    // triggers it, fill at 101. (Dynamic re-pricing would require the
    // strategy to re-issue orders.)
    assert_eq!(ns(run.trades[0].entry_price), "101");
}

#[test]
fn short_stop_entry() {
    // Sell stop at 99 (short entry): bar 2 low 98 <= 99 triggers, fill at stop.
    let data = format!(
        "{HEADER}{}{}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
        bar("2024-01-01T01:00:00Z", "100", "101", "99", "100"),
        bar("2024-01-01T02:00:00Z", "100", "100", "98", "99"),
    );
    let s = spec(
        r#"
strategy:
  name: short_stop
  symbols: [X]
  entry:
    direction: short
    when: {gt: [{field: close}, 0]}
  orders:
    entry: {type: stop, price: 99}
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
    let (run, _sink) = run(&data, &s, &cfg);
    assert_eq!(run.trades.len(), 1);
    assert_eq!(run.trades[0].direction, "short");
    assert_eq!(ns(run.trades[0].entry_price), "99");
}

#[test]
fn participation_cap_partial_fills() {
    // Volume 1000/bar, cap 10% => max 100 units/bar. Order 250 fills
    // 100/100/50 over three bars (all at open 100).
    let data = format!(
        "{HEADER}{}{}{}{}{}",
        "2024-01-01T00:00:00Z,X,100,100,100,100,1000\n",
        "2024-01-01T01:00:00Z,X,100,100,100,100,1000\n",
        "2024-01-01T02:00:00Z,X,100,100,100,100,1000\n",
        "2024-01-01T03:00:00Z,X,100,100,100,100,1000\n",
        "2024-01-01T04:00:00Z,X,100,100,100,100,1000\n",
    );
    let s = spec(
        r#"
strategy:
  name: capped
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: fixed_quantity, qty: 250}
"#,
    );
    let mut cfg = EngineConfig::default();
    cfg.execution.participation_cap = Some(d(10) / d(100)); // 0.10
    let (run, _sink) = run(&data, &s, &cfg);
    // one trade of 250 units accumulated over 3 partial entry fills
    assert_eq!(run.trades.len(), 1);
    assert_eq!(ns(run.trades[0].quantity), "250");
    // fills: 100 + 100 + 50 entries + 1 end-of-data exit
    assert_eq!(run.fills.len(), 4);
    assert_eq!(ns(run.fills[0].quantity), "100");
    assert_eq!(ns(run.fills[1].quantity), "100");
    assert_eq!(ns(run.fills[2].quantity), "50");
    assert_eq!(run.fills[3].reason, "end_of_data");
}

#[test]
fn square_root_impact_scales_with_participation() {
    // Impact slippage: bigger order => worse fill price.
    let data = format!(
        "{HEADER}{}{}{}",
        "2024-01-01T00:00:00Z,X,100,100,100,100,1000
",
        "2024-01-01T01:00:00Z,X,100,100,100,100,1000
",
        "2024-01-01T02:00:00Z,X,100,100,100,100,1000
",
    );
    let s = spec(
        r#"
strategy:
  name: impact
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: fixed_quantity, qty: 100}
"#,
    );
    let mut cfg = EngineConfig::default();
    cfg.costs.slippage = bt_execution::SlippageModel::SquareRootImpact { coefficient: d(1) };
    let (small, _) = run(&data, &s, &cfg);
    // smaller order on the same volume: less impact
    let s_small = spec(
        r#"
strategy:
  name: impact
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: fixed_quantity, qty: 10}
"#,
    );
    let (tiny, _) = run(&data, &s_small, &cfg);
    let big_px = ns(small.fills[0].fill_price);
    let small_px = ns(tiny.fills[0].fill_price);
    // prices are 100 * (1 + c*sqrt(qty/vol)): 100*(1+sqrt(0.1)) vs 100*(1+sqrt(0.01))
    assert!(
        big_px > small_px,
        "bigger participation must slip more: {big_px} vs {small_px}"
    );
    // qty=10 on vol=1000: participation 0.01, sqrt=0.1, impact = 100*0.1 = 10
    let small_val = bt_core::money::to_f64(tiny.fills[0].fill_price);
    assert!(
        (small_val - 110.0).abs() < 1e-6,
        "expected ~110, got {small_val}"
    );
}

#[test]
fn cash_dividend_credits_long_pays_short() {
    // Long 10 units on X: dividend 0.5/unit on ex-date => +5 cash.
    let data = format!(
        "{HEADER}{}{}{}{}",
        "2024-01-01T00:00:00Z,X,100,100,100,100,10\n",
        "2024-01-01T01:00:00Z,X,100,101,99,100,10\n",
        // ex-date bar
        "2024-01-01T02:00:00Z,X,100,102,100,101,10\n",
        "2024-01-01T03:00:00Z,X,101,102,100,101,10\n",
    );
    let s = spec(
        r#"
strategy:
  name: div_hold
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: fixed_quantity, qty: 10}
"#,
    );
    let cfg = EngineConfig::default();
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
    let mut ds = dataset;
    ds.actions.push(bt_data::actions::CorporateAction {
        ts: ts_of("2024-01-01T02:00:00Z"),
        symbol: "X".into(),
        kind: bt_data::actions::CorporateActionKind::Dividend {
            amount: d_from_str("0.5"),
        },
    });
    let mut sink = bt_core::event::VecSink::new();
    let signals = std::collections::BTreeMap::new();
    let run = bt_simulation::SimulationEngine::run(&ds, &s, &cfg, &signals, &mut sink).unwrap();
    assert_eq!(run.trades.len(), 1);
    assert_eq!(ns(run.trades[0].dividends), "5");
    // net pnl includes dividend: price PnL (100->101)*10 = 10, dividend +5
    assert_eq!(ns(run.trades[0].net_pnl), "15");
}

#[test]
fn split_adjusts_open_position() {
    // Long 10 @ 100. 2-for-1 split => 20 units @ 50. Exit at 50 => PnL 0
    // (economics unchanged). Exit at 51 => +20*1 = +20.
    let data = format!(
        "{HEADER}{}{}{}{}",
        "2024-01-01T00:00:00Z,X,100,100,100,100,10\n",
        "2024-01-01T01:00:00Z,X,100,101,99,100,10\n",
        // ex-date bar: post-split prices
        "2024-01-01T02:00:00Z,X,50,51,49,50,10\n",
        "2024-01-01T03:00:00Z,X,50,52,49,51,10\n",
    );
    let s = spec(
        r#"
strategy:
  name: split_hold
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{field: close}, 0]}
  risk:
    sizing: {mode: fixed_quantity, qty: 10}
"#,
    );
    let cfg = EngineConfig::default();
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
    let mut ds = dataset;
    ds.actions.push(bt_data::actions::CorporateAction {
        ts: ts_of("2024-01-01T02:00:00Z"),
        symbol: "X".into(),
        kind: bt_data::actions::CorporateActionKind::Split { ratio: d(2) },
    });
    let mut sink = bt_core::event::VecSink::new();
    let signals = std::collections::BTreeMap::new();
    let run = bt_simulation::SimulationEngine::run(&ds, &s, &cfg, &signals, &mut sink).unwrap();
    assert_eq!(run.trades.len(), 1);
    let t = &run.trades[0];
    // split-adjusted exit at 51 => +10*2*(51-50) = +20
    assert_eq!(ns(t.net_pnl), "20");
    assert_eq!(ns(t.quantity), "20");
    assert_eq!(ns(t.exit_price), "51");
}

#[test]
fn cross_symbol_condition_gates_entry() {
    // Entry on X requires Y's close > 150. Y's price only exceeds 150 from
    // bar 2, so X's first fill happens after that.
    let data = format!(
        "{HEADER}{}{}{}{}{}{}",
        "2024-01-01T00:00:00Z,X,100,100,100,100,10\n",
        "2024-01-01T00:00:00Z,Y,100,100,100,100,10\n",
        "2024-01-01T01:00:00Z,X,100,101,99,101,10\n",
        "2024-01-01T01:00:00Z,Y,100,101,99,100,10\n",
        "2024-01-01T02:00:00Z,X,101,102,100,102,10\n",
        "2024-01-01T02:00:00Z,Y,100,200,99,160,10\n",
    );
    let s = spec(
        r#"
strategy:
  name: cross_sym
  symbols: [X, Y]
  entry:
    direction: long
    when:
      all:
        - gt: [{field: close}, 0]
        - gt: [{symbol: Y, of: {field: close}}, 150]
  risk:
    sizing: {mode: fixed_quantity, qty: 1}
"#,
    );
    let cfg = EngineConfig::default();
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
    let mut sink = bt_core::event::VecSink::new();
    let signals = std::collections::BTreeMap::new();
    let run =
        bt_simulation::SimulationEngine::run(&dataset, &s, &cfg, &signals, &mut sink).unwrap();
    // Y closes 100,100,160. Y>150 observable only from bar 3 (Y close 160 at
    // decision ts 02:00). X entry decision at 02:00 -> fill next bar (none)
    // => no X trade; but X signals at 00:00/01:00 must be blocked.
    assert!(
        run.trades.is_empty(),
        "cross-symbol gate must block entries before Y qualifies"
    );
}

#[test]
fn cross_symbol_reference_undeclared_symbol_rejected() {
    let s = spec(
        r#"
strategy:
  name: bad_sym
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{symbol: ZZZ, of: {field: close}}, 1]}
"#,
    );
    let cfg = EngineConfig::default();
    let data = format!(
        "{HEADER}{}",
        bar("2024-01-01T00:00:00Z", "100", "100", "100", "100"),
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
    let mut sink = bt_core::event::VecSink::new();
    let signals = std::collections::BTreeMap::new();
    let err = match bt_simulation::SimulationEngine::run(&dataset, &s, &cfg, &signals, &mut sink) {
        Err(e) => e,
        Ok(_) => panic!("undeclared cross-symbol reference must be rejected"),
    };
    assert!(
        err.to_string().contains("ZZZ"),
        "exact reason required: {err}"
    );
}

fn d_from_str(s: &str) -> bt_core::D {
    s.parse().unwrap()
}
