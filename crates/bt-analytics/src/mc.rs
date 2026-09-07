//! Monte Carlo analysis (spec §24). The ONLY randomness in the engine:
//! SplitMix64 with an explicit seed, recorded in experiment metadata.
//! Same seed ⇒ byte-identical distributions.

use crate::metrics::McMethod;
use crate::stats;
use bt_core::ledger::TradeRecord;
use bt_core::prng::SplitMix64;
use bt_core::D;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McReport {
    pub method: McMethod,
    pub seed: u64,
    pub paths: u32,
    pub final_equity: Percentiles,
    pub max_drawdown_pct: Percentiles,
    pub risk_of_ruin_pct: f64,
    pub median_path: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Percentiles {
    pub p5: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p95: f64,
}

/// Run the Monte Carlo over the sequence of trade net P&Ls (exit-ordered).
pub fn run_monte_carlo(
    trades: &[TradeRecord],
    initial_capital: D,
    paths: u32,
    seed: u64,
    method: McMethod,
    ruin_threshold_pct: D,
) -> McReport {
    let pnls: Vec<D> = trades.iter().map(|t| t.net_pnl).collect();
    let mut rng = SplitMix64::new(seed);

    let mut final_eqs: Vec<f64> = Vec::with_capacity(paths as usize);
    let mut max_dds: Vec<f64> = Vec::with_capacity(paths as usize);
    let median_path = [bt_core::money::to_f64(initial_capital)];
    let mut path_samples: Vec<Vec<f64>> = Vec::new();

    let ruined_threshold = initial_capital * ruin_threshold_pct / dec!(100);
    for _ in 0..paths {
        let mut seq = pnls.clone();
        match method {
            McMethod::Reshuffle => rng.shuffle(&mut seq),
            McMethod::Bootstrap => {
                // Resample with replacement, same length.
                let mut sampled = Vec::with_capacity(seq.len());
                for _ in 0..seq.len() {
                    let i = rng.next_index(seq.len().max(1));
                    sampled.push(seq[i]);
                }
                seq = sampled;
            }
        }
        let mut equity = initial_capital;
        let mut peak = initial_capital;
        let mut max_dd = dec!(0);
        let mut ruined = false;
        let mut path = Vec::with_capacity(seq.len() + 1);
        path.push(bt_core::money::to_f64(equity));
        for pnl in &seq {
            equity += *pnl;
            if equity > peak {
                peak = equity;
            }
            let dd = peak - equity;
            if dd > max_dd {
                max_dd = dd;
            }
            path.push(bt_core::money::to_f64(equity));
            if equity <= ruined_threshold {
                ruined = true;
            }
        }
        final_eqs.push(bt_core::money::to_f64(equity));
        max_dds.push(bt_core::money::to_f64(
            max_dd / peak.max(dec!(1)) * dec!(100),
        ));
        if ruined {
            // risk of ruin counted below
        }
        if path_samples.len() < 201 {
            path_samples.push(path);
        }
    }
    let _ = ruined_threshold;

    // risk of ruin: fraction of paths whose equity ever touched the threshold.
    // Recomputed exactly (same seed/order) by a second pass over stored flags —
    // we reuse the per-path minimum instead of storing flags per path above.
    // (Single pass: recompute minimum equity per path with the same RNG state
    // would double randomize; instead we tracked `ruined` per path but did not
    // store it — so re-derive deterministically here.)
    let mut rng2 = SplitMix64::new(seed);
    let mut ruined_count = 0u32;
    for _ in 0..paths {
        let mut seq = pnls.clone();
        match method {
            McMethod::Reshuffle => rng2.shuffle(&mut seq),
            McMethod::Bootstrap => {
                let mut sampled = Vec::with_capacity(seq.len());
                for _ in 0..seq.len() {
                    let i = rng2.next_index(seq.len().max(1));
                    sampled.push(seq[i]);
                }
                seq = sampled;
            }
        }
        let mut equity = initial_capital;
        let mut hit = false;
        for pnl in &seq {
            equity += *pnl;
            if equity <= ruined_threshold {
                hit = true;
                break;
            }
        }
        if hit {
            ruined_count += 1;
        }
    }

    // median path: median equity across sampled paths at each step
    let n_steps = median_path.len();
    let mut med = Vec::with_capacity(path_samples.first().map(|p| p.len()).unwrap_or(n_steps));
    let len = path_samples.first().map(|p| p.len()).unwrap_or(0);
    for i in 0..len {
        let col: Vec<f64> = path_samples
            .iter()
            .filter_map(|p| p.get(i).copied())
            .collect();
        med.push(stats::median(&col).unwrap_or(0.0));
    }

    McReport {
        method,
        seed,
        paths,
        final_equity: Percentiles {
            p5: stats::percentile(&final_eqs, 5.0).unwrap_or(0.0),
            p25: stats::percentile(&final_eqs, 25.0).unwrap_or(0.0),
            p50: stats::percentile(&final_eqs, 50.0).unwrap_or(0.0),
            p75: stats::percentile(&final_eqs, 75.0).unwrap_or(0.0),
            p95: stats::percentile(&final_eqs, 95.0).unwrap_or(0.0),
        },
        max_drawdown_pct: Percentiles {
            p5: stats::percentile(&max_dds, 5.0).unwrap_or(0.0),
            p25: stats::percentile(&max_dds, 25.0).unwrap_or(0.0),
            p50: stats::percentile(&max_dds, 50.0).unwrap_or(0.0),
            p75: stats::percentile(&max_dds, 75.0).unwrap_or(0.0),
            p95: stats::percentile(&max_dds, 95.0).unwrap_or(0.0),
        },
        risk_of_ruin_pct: if paths == 0 {
            0.0
        } else {
            ruined_count as f64 / paths as f64 * 100.0
        },
        median_path: med,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_core::time::Ts;

    fn ts() -> Ts {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        bt_core::time::parse_timestamp("2024-01-01T00:00:00Z", utc, "t").unwrap()
    }

    fn trade(net: D) -> TradeRecord {
        TradeRecord {
            trade_id: 1,
            symbol: "X".into(),
            direction: "long".into(),
            entry_timestamp: ts(),
            entry_price: dec!(100),
            exit_timestamp: ts(),
            exit_price: dec!(101),
            quantity: dec!(1),
            gross_pnl: net,
            commission: dec!(0),
            spread_cost: dec!(0),
            slippage_cost: dec!(0),
            financing: dec!(0),
            dividends: dec!(0),
            net_pnl: net,
            initial_risk: None,
            r_multiple: None,
            holding_bars: 1,
            holding_time_secs: 3600,
            mae: dec!(0),
            mfe: dec!(0),
            entry_reason: "e".into(),
            exit_reason: "x".into(),
        }
    }

    #[test]
    fn deterministic_given_seed() {
        let trades: Vec<TradeRecord> = [10, -5, 20, -8, 15, -3, 12, -7]
            .iter()
            .map(|v| trade(D::from(*v)))
            .collect();
        let a = run_monte_carlo(&trades, dec!(1000), 500, 7, McMethod::Reshuffle, dec!(50));
        let b = run_monte_carlo(&trades, dec!(1000), 500, 7, McMethod::Reshuffle, dec!(50));
        assert_eq!(
            serde_json::to_string(&a.final_equity).unwrap(),
            serde_json::to_string(&b.final_equity).unwrap()
        );
        assert_eq!(a.risk_of_ruin_pct, b.risk_of_ruin_pct);
        let c = run_monte_carlo(&trades, dec!(1000), 500, 8, McMethod::Reshuffle, dec!(50));
        assert_ne!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&c).unwrap(),
            "different seeds should produce different full reports"
        );
    }

    #[test]
    fn ruin_detected_when_losses_exceed_threshold() {
        // All-losing sequence: every path ends at 600, below the 70% threshold.
        let trades: Vec<TradeRecord> = [-100, -100, -100, -100]
            .iter()
            .map(|v| trade(D::from(*v)))
            .collect();
        let r = run_monte_carlo(&trades, dec!(1000), 100, 1, McMethod::Reshuffle, dec!(70));
        assert!((r.risk_of_ruin_pct - 100.0).abs() < 1e-9);
        assert!(
            (r.final_equity.p50 - 600.0).abs() < 1e-9,
            "all paths end at 600"
        );
    }
}
