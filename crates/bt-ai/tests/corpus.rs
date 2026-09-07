//! P3 acceptance: a 10-strategy natural-language corpus mechanized into
//! valid, compiler-passing specs. Runs LIVE against the configured provider
//! and is skipped when CELIS_API_KEY is absent (offline coverage comes from
//! the mock-provider unit tests in bt-ai).

use bt_ai::compile::{compile_strategy, CompileInput};
use bt_ai::gateway::{Gateway, GatewayConfig};
use bt_ai::prompts::DataContext;

const CORPUS: [&str; 10] = [
    // 1 — simple trend follow
    "Buy when the 20-bar SMA crosses above the 50-bar SMA. Exit when it crosses below. \
     Stop loss 1.5% fixed distance. Risk 1% of equity per trade.",
    // 2 — RSI mean reversion
    "Go long when RSI(14) drops below 30 and close is above the 200-bar EMA. \
     Take profit at 3x the stop distance. Stop at the 20-day low. Use fixed quantity 10.",
    // 3 — Bollinger breakout
    "Enter long when close breaks above the upper Bollinger band (20, 2). \
     Exit when close falls below the middle band. Stop loss 2%. Risk 0.5% of equity.",
    // 4 — session-filtered momentum
    "Buy when pct change of close over 4 bars is greater than 0.5% during the \
     New York session (09:30 to 16:00 America/New_York). Fixed stop 25 ticks. \
     Take profit 75 ticks. Fixed quantity 5.",
    // 5 — short side
    "Sell short when RSI(7) is above 75 and price crosses below the 10-bar EMA. \
     Cover when RSI falls below 50. Stop loss 1% fixed distance. Risk amount 500.",
    // 6 — MACD trend
    "Long when the MACD line crosses above the signal line and ADX-style trend is not \
     required. Exit when MACD crosses below signal. ATR(14) stop at 2.5x. \
     Percent-of-equity sizing at 10%.",
    // 7 — Bollinger mean reversion with filter
    "Buy when close is below the lower Bollinger band (20, 2) and RSI(2) is under 10. \
     Exit at the middle band. Fixed stop 2.0. Fixed quantity 3.",
    // 8 — day-of-week filter
    "Only trade Mondays and Tuesdays. Enter long when close crosses above the \
     highest high of the last 10 bars. Stop at the lowest low of the last 10 bars. \
     Take profit at 2 times the risk. Risk 1.5% of equity.",
    // 9 — higher timeframe confirmation
    "On the 1D timeframe require close above the 20-bar daily SMA. On the base \
     timeframe buy when close crosses above the 10-bar EMA. ATR(14)-based trailing \
     stop at 3x. Percent risk 0.75%.",
    // 10 — drawdown guard + range
    "Enter long when close is between 1.0800 and 1.1200 and the 50-bar ROC is positive. \
     Exit when unrealized pnl percent is above 2. Stop loss 60 pips. \
     Fixed amount 25000 notional.",
];

fn data_context() -> DataContext {
    DataContext {
        symbols: vec!["EURUSD".into()],
        base_timeframe: "1h".into(),
        bars: 300,
        start: "2024-01-01T00:00:00Z".into(),
        end: "2024-01-13T11:00:00Z".into(),
        timezone: "UTC".into(),
    }
}

#[test]
fn corpus_10_strategies_compile_live() {
    let Some(api_key) = std::env::var("CELIS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        eprintln!(
            "SKIPPED: CELIS_API_KEY not set — live corpus test requires an API key. \
                   Offline machinery is covered by the mock-provider unit tests."
        );
        return;
    };
    let base_url = std::env::var("CELIS_AI_BASE_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let model = std::env::var("CELIS_AI_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "glm-5.3-flash".into());

    let cfg = GatewayConfig {
        model,
        base_url,
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("bt_ai_corpus_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let gateway = Gateway::new(
        cfg,
        &api_key,
        Some(bt_ai::ResponseCache::new(dir.join("cache")).unwrap()),
        Some(bt_ai::LedgerWriter::new(dir.join("ledger.jsonl")).unwrap()),
    );
    let ctx = data_context();
    let mut failures = Vec::new();
    for (i, md) in CORPUS.iter().enumerate() {
        let input = CompileInput {
            strategy_md: md,
            notes: "",
            context: &ctx,
            base_secs: 3600,
            max_repair_rounds: 3,
            strategy_version: "corpus",
        };
        let mut g = Gateway::new(
            bt_ai::GatewayConfig {
                model: std::env::var("CELIS_AI_MODEL")
                    .ok()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "glm-5.3-flash".into()),
                ..Default::default()
            },
            &api_key,
            Some(bt_ai::ResponseCache::new(dir.join("cache")).unwrap()),
            Some(bt_ai::LedgerWriter::new(dir.join("ledger.jsonl")).unwrap()),
        );
        match compile_strategy(&mut g, &input, false) {
            Ok(Ok(outcome)) => {
                assert!(!outcome.spec_yaml.is_empty());
                assert!(
                    !outcome.assumptions.is_empty() || outcome.rounds == 1,
                    "strategy {i}: assumptions should be documented"
                );
                println!(
                    "strategy {} OK: '{}' rounds={} assumptions={}",
                    i + 1,
                    outcome.strategy_name,
                    outcome.rounds,
                    outcome.assumptions.len()
                );
            }
            Ok(Err(report)) => {
                failures.push(format!("strategy {}: dry-run returned unexpectedly", i + 1));
                let _ = report;
            }
            Err(e) => failures.push(format!("strategy {}: {e}", i + 1)),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{}/{} corpus strategies failed:\n{}",
        failures.len(),
        CORPUS.len(),
        failures.join("\n")
    );
    let _ = gateway;
}
