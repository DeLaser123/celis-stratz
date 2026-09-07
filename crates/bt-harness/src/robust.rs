//! Overfitting robustness statistics (the enterprise "prove it" layer).
//!
//! - **Deflated Sharpe Ratio** (Bailey & López de Prado, JPM 2014): deflates
//!   the observed Sharpe by the expected maximum Sharpe across N trials.
//! - **Probability of Backtest Overfitting via CSCV** (Bailey, Borwein,
//!   López de Prado & Zhu, JCF 2016): S=16 blocks, all C(16,8)=12,870
//!   IS/OOS combinatorial splits; PBO = fraction of splits where the
//!   in-sample-best strategy ranks below the OOS median.
//!
//! Both are fully deterministic and reference-tested below. The normal
//! quantile function is Acklam's rational approximation (documented).

use bt_core::error::{CoreError, CoreResult};
use serde::Serialize;

/// Euler–Mascheroni constant.
const GAMMA: f64 = 0.577_215_664_901_533;

/// Inverse standard normal CDF (Acklam's rational approximation, |err| < 1.15e-9).
pub fn inv_norm_cdf(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let p_low = 0.02425;
    if !(0.0 < p && p < 1.0) {
        if p == 0.0 {
            return f64::NEG_INFINITY;
        }
        if p == 1.0 {
            return f64::INFINITY;
        }
        return f64::NAN;
    }
    if p < p_low {
        let q = (-p.ln()).sqrt();
        return (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0);
    }
    if p <= 1.0 - p_low {
        let q = p - 0.5;
        let r = q * q;
        return (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0);
    }
    let q = (-(1.0 - p).ln()).sqrt();
    -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
        / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
}

/// Standard normal CDF via erf approximation (Abramowitz & Stegun 7.1.26):
/// Φ(x) = 0.5 · (1 + erf(x/√2)).
pub fn norm_cdf(x: f64) -> f64 {
    let z = x / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * z.abs());
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-z * z).exp();
    let erf = if z >= 0.0 { y } else { -y };
    0.5 * (1.0 + erf)
}

/// Deflated Sharpe Ratio: P(true SR > 0) after deflating for N trials.
///
/// `sr_observed`: annualized or per-period SR of the selected strategy.
/// `sr_trials`: SRs of ALL N trials (including the winner).
/// `track_record_len`: number of return periods behind the observed SR.
/// `skew`/`kurtosis_excess`: of the selected strategy's returns.
pub fn deflated_sharpe(
    sr_observed: f64,
    sr_trials: &[f64],
    track_record_len: usize,
    skew: f64,
    kurtosis_excess: f64,
) -> CoreResult<f64> {
    let n = sr_trials.len();
    if n < 3 || track_record_len < 2 {
        // The expected-max formula needs N > e (Lopez de Prado): with fewer
        // trials there is nothing to deflate for.
        return Err(CoreError::InvalidData(
            "deflated Sharpe needs at least 3 trials and a track record >= 2 periods".into(),
        ));
    }
    let mean = sr_trials.iter().sum::<f64>() / n as f64;
    let var = if n > 1 {
        sr_trials
            .iter()
            .map(|s| (s - mean) * (s - mean))
            .sum::<f64>()
            / (n - 1) as f64
    } else {
        0.0
    };
    // Expected max SR across N trials (López de Prado eq. 3-4).
    let emc = 1.0 / n as f64; // 1/N
    let z1 = inv_norm_cdf(1.0 - emc);
    let z2 = inv_norm_cdf(1.0 - emc * std::f64::consts::E);
    // With a single trial (or identical trial SRs) the variance is zero and
    // the expected-max formula degenerates (0 * inf = NaN): SR* = 0.
    let sr_star = if var == 0.0 || !var.is_finite() {
        0.0
    } else {
        var.sqrt() * ((1.0 - GAMMA) * z1 + GAMMA * z2)
    };
    // Probabilistic Sharpe Ratio with non-normality correction (eq. 7).
    let t = track_record_len as f64;
    let denom = 1.0 - skew * sr_observed + kurtosis_excess / 4.0 * sr_observed * sr_observed;
    // The non-normality correction can invert (denominator <= 0) for very
    // thin-tailed distributions with a large SR. In that case we fall back to
    // the normality assumption (denominator = 1) — the conservative direction,
    // because the broken correction would otherwise INFLATE confidence.
    let denom = if denom > 0.0 { denom } else { 1.0 };
    let psr = ((sr_observed - sr_star) * (t - 1.0).sqrt()) / denom.sqrt();
    Ok(norm_cdf(psr))
}

#[derive(Debug, Clone, Serialize)]
pub struct PboResult {
    /// Probability of backtest overfitting (0..1).
    pub pbo: f64,
    /// Number of combinatorial splits evaluated (C(16,8) when T allows 16 blocks).
    pub splits: usize,
    pub blocks: usize,
    /// Mean OOS rank (relative, 0.5 = median) of the IS-best strategy.
    pub mean_oos_rank: f64,
    pub distribution_stochastic: bool,
}

/// PBO via CSCV on a returns MATRIX: `matrix[period][strategy]` (per-period
/// returns, same periods across strategies). S = 16 blocks; strategies are
/// ranked by in-sample mean return; PBO = fraction of splits where the
/// IS-best strategy's OOS relative rank ω gives logit λ = ln(ω/(1−ω)) ≤ 0.
pub fn pbo_cscv(matrix: &[Vec<f64>], s_blocks: usize) -> CoreResult<PboResult> {
    let periods = matrix.len();
    let n_strats = matrix.first().map(|r| r.len()).unwrap_or(0);
    if n_strats < 2 {
        return Err(CoreError::InvalidData(
            "PBO needs at least 2 strategies (trials)".into(),
        ));
    }
    if periods < s_blocks {
        return Err(CoreError::InvalidData(format!(
            "PBO needs at least {s_blocks} return periods, got {periods}"
        )));
    }
    // Trim to a multiple of S (drop the tail; documented).
    let usable = periods - periods % s_blocks;
    let block_len = usable / s_blocks;
    // Block means: block[s][strategy] = mean return of block s for strategy k.
    let mut block_means = vec![vec![0.0f64; n_strats]; s_blocks];
    for (s, bm) in block_means.iter_mut().enumerate() {
        for k in 0..n_strats {
            let sum: f64 = (0..block_len).map(|i| matrix[s * block_len + i][k]).sum();
            bm[k] = sum / block_len as f64;
        }
    }
    // Iterate all C(S, S/2) block combinations (deterministic order).
    let half = s_blocks / 2;
    let mut total = 0usize;
    let mut overfit = 0usize;
    let mut rank_sum = 0.0f64;
    let mut mask = (1u64 << half) - 1; // lowest `half` blocks = IS
    let limit = 1u64 << s_blocks;
    while mask < limit {
        // IS = blocks in mask, OOS = the rest.
        let mut is_means = vec![0.0f64; n_strats];
        let mut oos_means = vec![0.0f64; n_strats];
        for (s, bm) in block_means.iter().enumerate() {
            let in_is = (mask >> s) & 1 == 1;
            for (k, mean) in bm.iter().enumerate() {
                if in_is {
                    is_means[k] += mean;
                } else {
                    oos_means[k] += mean;
                }
            }
        }
        let is_n = half as f64;
        let oos_n = (s_blocks - half) as f64;
        for k in 0..n_strats {
            is_means[k] /= is_n;
            oos_means[k] /= oos_n;
        }
        // IS-best strategy (ties -> lowest index, deterministic).
        let mut best = 0usize;
        for k in 1..n_strats {
            if is_means[k] > is_means[best] {
                best = k;
            }
        }
        // OOS relative rank of the IS-best strategy, counted so that the
        // OOS-BEST strategy gets rank N (logit > 0, no overfit) and the worst
        // gets rank 1 (logit < 0, overfit). Ties count as beaten (<=).
        let mut rank = 0usize;
        for k in 0..n_strats {
            if oos_means[k] <= oos_means[best] {
                rank += 1;
            }
        }
        let omega = rank as f64 / (n_strats + 1) as f64;
        rank_sum += omega;
        total += 1;
        let logit = (omega / (1.0 - omega)).ln();
        if logit <= 0.0 {
            overfit += 1;
        }
        // Next combination (Gosper's hack).
        let c = mask & mask.wrapping_neg();
        let r = mask + c;
        mask = (((r ^ mask) / c) >> 2) | r;
    }
    if total == 0 {
        return Err(CoreError::InvalidData("no CSCV splits evaluated".into()));
    }
    Ok(PboResult {
        pbo: overfit as f64 / total as f64,
        splits: total,
        blocks: s_blocks,
        mean_oos_rank: rank_sum / total as f64,
        distribution_stochastic: false,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RobustReport {
    pub trials: usize,
    pub periods: usize,
    pub trial_sharpes: Vec<Option<f64>>,
    pub observed_sharpe: Option<f64>,
    pub deflated_sharpe: Option<f64>,
    pub pbo: Option<PboResult>,
    pub verdict: String,
}

/// Compute the full robustness report from per-run daily return series
/// (aligned by index after date intersection — the caller does alignment).
pub fn robust_report(returns: &[Vec<f64>], s_blocks: usize) -> CoreResult<RobustReport> {
    let n = returns.len();
    if n < 2 {
        return Err(CoreError::InvalidData(
            "robustness analysis needs at least 2 runs (trials)".into(),
        ));
    }
    let len = returns.iter().map(|r| r.len()).min().unwrap_or(0);
    if len == 0 {
        return Err(CoreError::InvalidData("empty return series".into()));
    }
    // Sharpe per trial (per-period; comparisons across trials are relative so
    // the annualization factor cancels out — documented).
    let sharpes: Vec<Option<f64>> = returns
        .iter()
        .map(|r| {
            match (
                bt_analytics::stats::mean(r),
                bt_analytics::stats::sample_std(r),
            ) {
                (Some(m), Some(s)) if s > 0.0 => Some(m / s),
                _ => None,
            }
        })
        .collect();
    let valid: Vec<f64> = sharpes.iter().filter_map(|s| *s).collect();
    let observed = sharpes
        .iter()
        .filter_map(|s| *s)
        .fold(f64::NEG_INFINITY, f64::max);
    let observed = if observed == f64::NEG_INFINITY {
        None
    } else {
        Some(observed)
    };
    // The "winner" trial's returns drive the DSR inputs.
    let winner_idx = sharpes
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|_| (i, *s)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let winner = &returns[winner_idx];
    let skew = bt_analytics::stats::skewness(winner).unwrap_or(0.0);
    let kurt = bt_analytics::stats::kurtosis_excess(winner).unwrap_or(0.0);
    let dsr = if valid.len() == n {
        observed.and_then(|o| deflated_sharpe(o, &valid, len, skew, kurt).ok())
    } else {
        None
    };
    // CSCV matrix: rows = periods, cols = strategies.
    let matrix: Vec<Vec<f64>> = (0..len)
        .map(|i| returns.iter().map(|r| r[i]).collect())
        .collect();
    let pbo = pbo_cscv(&matrix, s_blocks).ok();
    let verdict = match (dsr, pbo.as_ref()) {
        (Some(d), None) => {
            if d > 0.95 {
                format!("DEFLATED SHARPE SIGNIFICANT ({d:.3}) — add strategy variants for a full CSCV overfitting check")
            } else {
                format!("WEAK: deflated Sharpe {d:.3} below 0.95 — the edge does not survive trial-count deflation")
            }
        }
        (Some(d), Some(p)) => {
            if d > 0.95 && p.pbo < 0.2 {
                "ROBUST: deflated Sharpe is significant and overfitting probability is low".into()
            } else if p.pbo >= 0.5 {
                "OVERFIT RISK: the in-sample winner underperforms out-of-sample in over half the CSCV splits".into()
            } else if d <= 0.95 {
                format!("WEAK: deflated Sharpe {d:.3} below 0.95 — the edge does not survive trial-count deflation")
            } else {
                "REVIEW: mixed signals".into()
            }
        }
        (None, Some(p)) => format!(
            "INSUFFICIENT TRIALS: deflated Sharpe needs >= 3 trials; PBO = {:.3} ({} splits)",
            p.pbo, p.splits
        ),
        _ => "INSUFFICIENT DATA".into(),
    };
    let observed_sharpe = observed;
    Ok(RobustReport {
        trials: n,
        periods: len,
        observed_sharpe,
        trial_sharpes: sharpes,
        deflated_sharpe: dsr,
        pbo,
        verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_norm_reference_values() {
        // Φ⁻¹(0.975) = 1.959964, Φ⁻¹(0.5) = 0, Φ⁻¹(0.841344746) ≈ 1.0
        assert!((inv_norm_cdf(0.975) - 1.959_963_985).abs() < 1e-6);
        assert!(inv_norm_cdf(0.5).abs() < 1e-9);
        assert!((inv_norm_cdf(0.841_344_746) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn norm_cdf_reference_values() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9);
        assert!((norm_cdf(1.959_964) - 0.975).abs() < 1e-4);
        assert!((norm_cdf(-1.0) - 0.158_655_254).abs() < 1e-4);
    }

    #[test]
    fn dsr_zero_trials_edge_and_reference() {
        // Three identical trials: zero variance in trial SRs => SR* = 0,
        // so DSR reduces to the probabilistic Sharpe ratio.
        // Observed SR 2.0 over 252 periods, no skew/kurtosis:
        // PSR = Φ(2.0 * sqrt(251)) ≈ 1.0
        let d = deflated_sharpe(2.0, &[2.0, 2.0, 2.0], 252, 0.0, 0.0).unwrap();
        assert!(d > 0.999, "got {d}");
        // Weak SR with 3 trials: 0.05 over 20 periods → DSR well below 1.
        let d2 = deflated_sharpe(0.05, &[0.05, 0.04, 0.06], 20, 0.0, 0.0).unwrap();
        assert!(d2 < 0.7, "got {d2}");
    }

    #[test]
    fn pbo_zero_when_winner_always_wins_oos() {
        // Strategy 0 dominates in every block: PBO = 0.
        let matrix: Vec<Vec<f64>> = (0..32)
            .map(|i| vec![0.01 + (i % 7) as f64 * 0.0001, -0.01])
            .collect();
        let p = pbo_cscv(&matrix, 16).unwrap();
        assert_eq!(p.splits, 12_870, "C(16,8) combinations");
        assert!(p.pbo < 0.01, "got {}", p.pbo);
    }

    #[test]
    fn pbo_high_when_is_winner_loses_oos() {
        // Construct: strategy A great in even blocks, terrible in odd blocks;
        // strategy B steady. Half the IS sets are dominated by A's even blocks
        // yet A loses OOS — PBO should be substantial.
        let mut matrix = Vec::new();
        for i in 0..64 {
            let block = i / 4; // 16 blocks of 4 periods
            if block % 2 == 0 {
                matrix.push(vec![0.05, 0.001]);
            } else {
                matrix.push(vec![-0.05, 0.001]);
            }
        }
        let p = pbo_cscv(&matrix, 16).unwrap();
        assert!(p.pbo > 0.3, "got {}", p.pbo);
    }

    #[test]
    fn pbo_requires_two_strategies() {
        let matrix = vec![vec![0.01]; 20];
        assert!(pbo_cscv(&matrix, 16).is_err());
        let short = vec![vec![0.01, 0.02]; 8];
        assert!(pbo_cscv(&short, 16).is_err());
    }

    #[test]
    fn robust_report_end_to_end() {
        // 3 trials: noise-like deterministic series with slightly different
        // means (gentle enough for a finite PSR denominator).
        let good: Vec<f64> = (0..64)
            .map(|i| 0.001 + (((i * 7) % 11) as f64 - 5.0) * 0.0002)
            .collect();
        let mid: Vec<f64> = (0..64)
            .map(|i| 0.0002 + (((i * 5) % 7) as f64 - 3.0) * 0.0002)
            .collect();
        let bad: Vec<f64> = (0..64)
            .map(|i| -0.0005 + (((i * 3) % 9) as f64 - 4.0) * 0.0002)
            .collect();
        let r = robust_report(&[good, mid, bad], 16).unwrap();
        assert_eq!(r.trials, 3);
        assert!(r.deflated_sharpe.is_some());
        assert!(r.pbo.is_some());
        assert_eq!(r.pbo.as_ref().unwrap().splits, 12_870);
    }
}
