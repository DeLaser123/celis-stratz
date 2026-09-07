//! Performance metrics (spec §20-22). Every number's convention is explicit
//! in the report itself, and the intermediate series (returns, drawdowns,
//! R distribution) are exported so each metric can be independently recomputed.

use crate::stats;
use bt_core::ledger::TradeRecord;
use bt_core::time::Ts;
use bt_core::D;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Equity curve point recorded by the engine at each bar close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts: Ts,
    pub equity: D,
    pub balance: D,
    pub unrealized: D,
    pub drawdown: D,
    pub drawdown_pct: D,
    pub in_position: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReturnsFreq {
    #[default]
    Daily,
    PerBar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StdMode {
    #[default]
    Sample,
    Population,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RConvention {
    #[default]
    Net,
    Gross,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McMethod {
    #[default]
    Reshuffle,
    Bootstrap,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonteCarloConfig {
    #[serde(default = "mc_paths")]
    pub paths: u32,
    #[serde(default = "mc_seed")]
    pub seed: u64,
    #[serde(default)]
    pub method: McMethod,
    /// Ruin threshold as % of initial capital (equity <= threshold = ruined).
    #[serde(default = "mc_ruin")]
    pub ruin_threshold_pct: D,
}

fn mc_paths() -> u32 {
    1000
}
fn mc_seed() -> u64 {
    42
}
fn mc_ruin() -> D {
    dec!(50)
}

impl Default for MonteCarloConfig {
    fn default() -> Self {
        MonteCarloConfig {
            paths: mc_paths(),
            seed: mc_seed(),
            method: McMethod::Reshuffle,
            ruin_threshold_pct: mc_ruin(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyticsConfig {
    #[serde(default)]
    pub returns_freq: ReturnsFreq,
    /// Annual risk-free rate (0.03 = 3%/yr).
    #[serde(default)]
    pub risk_free_rate: f64,
    #[serde(default)]
    pub std_mode: StdMode,
    #[serde(default = "trading_days")]
    pub trading_days_per_year: u32,
    /// Override the auto annualization factor (periods per year).
    #[serde(default)]
    pub annualization_factor: Option<f64>,
    #[serde(default)]
    pub r_convention: RConvention,
    #[serde(default)]
    pub profit_factor_basis: RConvention,
    #[serde(default = "rolling_window")]
    pub rolling_sharpe_window_days: u32,
    #[serde(default)]
    pub exclude_zero_returns: bool,
    #[serde(default)]
    pub monte_carlo: MonteCarloConfig,
}

fn trading_days() -> u32 {
    252
}
fn rolling_window() -> u32 {
    126
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        AnalyticsConfig {
            returns_freq: ReturnsFreq::Daily,
            risk_free_rate: 0.0,
            std_mode: StdMode::Sample,
            trading_days_per_year: 252,
            annualization_factor: None,
            r_convention: RConvention::Net,
            profit_factor_basis: RConvention::Net,
            rolling_sharpe_window_days: 126,
            exclude_zero_returns: false,
            monte_carlo: MonteCarloConfig::default(),
        }
    }
}

/// The complete metrics report. Money-metric values are f64 conversions of
/// exact Decimal ledger values (documented boundary); equity/trades remain
/// exact in their own CSV/JSON exports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsReport {
    pub conventions: Conventions,
    pub overview: Overview,
    pub returns: ReturnStats,
    pub risk: RiskStats,
    pub trades: TradeStats,
    pub r_stats: RStats,
    pub distributions: Distributions,
    pub monthly_returns: Vec<PeriodReturn>,
    pub yearly_returns: Vec<PeriodReturn>,
    pub rolling_sharpe: Vec<RollingPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conventions {
    pub returns_freq: ReturnsFreq,
    pub annualization_factor: f64,
    pub risk_free_rate_annual: f64,
    pub risk_free_rate_per_period: f64,
    pub std_mode: StdMode,
    pub r_convention: RConvention,
    pub profit_factor_basis: RConvention,
    pub periods_per_year: f64,
    pub cagr_year_basis: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overview {
    pub initial_capital: f64,
    pub final_equity: f64,
    pub total_return_pct: Option<f64>,
    pub cagr_pct: Option<f64>,
    pub start_ts: Option<Ts>,
    pub end_ts: Option<Ts>,
    pub total_bars: usize,
    pub exposure_pct: f64,
    pub time_in_market_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReturnStats {
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub calmar: Option<f64>,
    pub volatility_annualized_pct: Option<f64>,
    pub mean_return_per_period_pct: Option<f64>,
    pub skewness: Option<f64>,
    pub kurtosis_excess: Option<f64>,
    pub best_period_pct: Option<f64>,
    pub worst_period_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskStats {
    pub max_drawdown_amount: f64,
    pub max_drawdown_pct: f64,
    pub max_drawdown_start: Option<Ts>,
    pub max_drawdown_end: Option<Ts>,
    pub average_drawdown_pct: Option<f64>,
    pub ulcer_index: Option<f64>,
    pub recovery_factor: Option<f64>,
    pub risk_of_ruin_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradeStats {
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub breakeven_trades: usize,
    pub win_rate_pct: Option<f64>,
    pub loss_rate_pct: Option<f64>,
    pub profit_factor: Option<f64>,
    pub avg_win: Option<f64>,
    pub avg_loss: Option<f64>,
    pub payoff_ratio: Option<f64>,
    pub expectancy_amount: Option<f64>,
    pub total_net_pnl: f64,
    pub total_gross_pnl: f64,
    pub total_commission: f64,
    pub total_spread_cost: f64,
    pub total_slippage_cost: f64,
    pub total_financing: f64,
    pub avg_holding_bars: Option<f64>,
    pub avg_holding_time_hours: Option<f64>,
    pub longest_winning_streak: usize,
    pub longest_losing_streak: usize,
    pub trades_without_stop: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RStats {
    pub avg_r: Option<f64>,
    pub median_r: Option<f64>,
    pub max_r: Option<f64>,
    pub min_r: Option<f64>,
    pub std_r: Option<f64>,
    pub positive_r_count: usize,
    pub negative_r_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distributions {
    pub r_multiples: Vec<f64>,
    pub mae: Vec<f64>,
    pub mfe: Vec<f64>,
    pub returns_pct: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeriodReturn {
    pub year: i32,
    /// 1-12; absent for yearly returns.
    pub month: Option<u32>,
    pub return_pct: f64,
    pub end_equity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollingPoint {
    pub ts: Ts,
    pub rolling_sharpe: Option<f64>,
}

/// Computed inputs: exact equity curve + trade ledger + configuration.
pub struct MetricsInput<'a> {
    pub equity_curve: &'a [EquityPoint],
    pub trades: &'a [TradeRecord],
    pub initial_capital: D,
    pub bar_secs: i64,
    pub config: &'a AnalyticsConfig,
    /// Risk of ruin from Monte Carlo (engine passes this in after running MC).
    pub risk_of_ruin_pct: f64,
}

/// Last equity per UTC day (the daily return basis).
pub fn daily_equity_series(curve: &[EquityPoint], _initial: D) -> Vec<(Ts, D)> {
    let mut daily: BTreeMap<(i32, u32), D> = BTreeMap::new();
    for p in curve {
        daily.insert((p.ts.year(), p.ts.ordinal()), p.equity);
    }
    daily
        .into_iter()
        .map(|((y, ord), eq)| {
            let t = Utc
                .with_ymd_and_hms(y, 1, 1, 0, 0, 0)
                .single()
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
                + chrono::Duration::days((ord - 1) as i64);
            (t, eq)
        })
        .collect()
}

pub fn compute_metrics(input: &MetricsInput) -> MetricsReport {
    let cfg = input.config;
    let curve = input.equity_curve;

    // --- periods per year ------------------------------------------------
    let bars_per_day = if input.bar_secs > 0 {
        86_400.0 / input.bar_secs as f64
    } else {
        1.0
    };
    let ppy = match cfg.annualization_factor {
        Some(f) => f,
        None => match cfg.returns_freq {
            ReturnsFreq::Daily => cfg.trading_days_per_year as f64,
            ReturnsFreq::PerBar => cfg.trading_days_per_year as f64 * bars_per_day,
        },
    };
    let rf_period = (1.0 + cfg.risk_free_rate).powf(1.0 / ppy) - 1.0;

    // --- return series -----------------------------------------------------
    let (ret_ts, returns): (Vec<Ts>, Vec<f64>) = match cfg.returns_freq {
        ReturnsFreq::PerBar => {
            let mut r = Vec::with_capacity(curve.len());
            let mut ts = Vec::with_capacity(curve.len());
            let mut prev = bt_core::money::to_f64(input.initial_capital);
            for p in curve {
                let e = bt_core::money::to_f64(p.equity);
                if prev != 0.0 {
                    ts.push(p.ts);
                    r.push(e / prev - 1.0);
                }
                prev = e;
            }
            (ts, r)
        }
        ReturnsFreq::Daily => {
            // last equity per UTC day; first return vs initial capital
            let mut daily: BTreeMap<(i32, u32), D> = BTreeMap::new();
            for p in curve {
                let key = (p.ts.year(), p.ts.ordinal());
                daily.insert(key, p.equity);
            }
            let mut r = Vec::new();
            let mut tsv = Vec::new();
            let mut prev = bt_core::money::to_f64(input.initial_capital);
            for ((y, ord), eq) in &daily {
                // Reconstruct the day's date from (year, ordinal). Keys derive
                // from real timestamps, so this always resolves; the fallback
                // is a FIXED epoch value — never the wall clock (determinism).
                let t = Utc.with_ymd_and_hms(*y, 1, 1, 0, 0, 0).single().unwrap_or(
                    DateTime::<Utc>::from_timestamp(0, 0).unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
                ) + chrono::Duration::days((*ord - 1) as i64);
                let e = bt_core::money::to_f64(*eq);
                if prev != 0.0 {
                    tsv.push(t);
                    r.push(e / prev - 1.0);
                }
                prev = e;
            }
            (tsv, r)
        }
    };
    let returns = if cfg.exclude_zero_returns {
        returns
            .into_iter()
            .filter(|r| *r != 0.0)
            .collect::<Vec<_>>()
    } else {
        returns
    };

    let std_fn = match cfg.std_mode {
        StdMode::Sample => stats::sample_std,
        StdMode::Population => stats::population_std,
    };
    let mean_r = stats::mean(&returns);
    let std_r = std_fn(&returns);
    let sharpe = match (mean_r, std_r) {
        (Some(m), Some(s)) if s > 0.0 => Some((m - rf_period) / s * ppy.sqrt()),
        _ => None,
    };
    let dd_dev = stats::downside_dev(&returns, rf_period);
    let sortino = match (mean_r, dd_dev) {
        (Some(m), Some(d)) if d > 0.0 => Some((m - rf_period) / d * ppy.sqrt()),
        _ => None,
    };

    // --- drawdowns (exact Decimal high-water mark) -------------------------
    let mut peak = input.initial_capital;
    let mut max_dd = dec!(0);
    let mut max_dd_pct = dec!(0);
    let mut dd_start: Option<Ts> = None;
    let mut max_dd_start: Option<Ts> = None;
    let mut max_dd_end: Option<Ts> = None;
    let mut in_dd = false;
    let mut dd_values_pct: Vec<f64> = Vec::new();
    for p in curve {
        if p.equity > peak {
            peak = p.equity;
            in_dd = false;
        }
        let dd = peak - p.equity;
        let dd_pct = if peak.is_zero() {
            dec!(0)
        } else {
            dd / peak * dec!(100)
        };
        if dd > dec!(0) {
            dd_values_pct.push(bt_core::money::to_f64(dd_pct));
            if !in_dd {
                dd_start = Some(p.ts);
                in_dd = true;
            }
            if dd > max_dd {
                max_dd = dd;
                max_dd_pct = dd_pct;
                max_dd_start = dd_start;
                max_dd_end = Some(p.ts);
            }
        }
    }
    let avg_dd = if dd_values_pct.is_empty() {
        None
    } else {
        stats::mean(&dd_values_pct)
    };
    let ulcer = if dd_values_pct.is_empty() {
        None
    } else {
        stats::mean(&dd_values_pct.iter().map(|d| d * d).collect::<Vec<_>>()).map(|m| m.sqrt())
    };

    // --- overview ----------------------------------------------------------
    let final_equity = curve
        .last()
        .map(|p| p.equity)
        .unwrap_or(input.initial_capital);
    let total_return = if !input.initial_capital.is_zero() {
        Some(bt_core::money::to_f64(
            (final_equity / input.initial_capital - dec!(1)) * dec!(100),
        ))
    } else {
        None
    };
    let start_ts = curve.first().map(|p| p.ts);
    let end_ts = curve.last().map(|p| p.ts);
    let years = match (start_ts, end_ts) {
        (Some(s), Some(e)) => {
            let secs = (e - s).num_seconds() as f64 + input.bar_secs as f64;
            secs / (365.25 * 86_400.0)
        }
        _ => 0.0,
    };
    let cagr = match total_return {
        Some(tr) if years > 0.0 => {
            let multiple = 1.0 + tr / 100.0;
            if multiple > 0.0 {
                Some((multiple.powf(1.0 / years) - 1.0) * 100.0)
            } else {
                None
            }
        }
        _ => None,
    };
    let exposure_count = curve.iter().filter(|p| p.in_position).count();
    let exposure_pct = if curve.is_empty() {
        0.0
    } else {
        exposure_count as f64 / curve.len() as f64 * 100.0
    };

    // --- trade stats ---------------------------------------------------------
    let _pnl = |t: &TradeRecord| -> f64 {
        match cfg.profit_factor_basis {
            RConvention::Net => bt_core::money::to_f64(t.net_pnl),
            RConvention::Gross => bt_core::money::to_f64(t.gross_pnl),
        }
    };
    let net_pnls: Vec<f64> = input
        .trades
        .iter()
        .map(|t| bt_core::money::to_f64(t.net_pnl))
        .collect();
    let wins: Vec<f64> = net_pnls.iter().copied().filter(|p| *p > 0.0).collect();
    let losses: Vec<f64> = net_pnls.iter().copied().filter(|p| *p < 0.0).collect();
    let gross_wins: f64 = match cfg.profit_factor_basis {
        RConvention::Net => wins.iter().sum(),
        RConvention::Gross => input
            .trades
            .iter()
            .filter(|t| t.net_pnl > dec!(0))
            .map(|t| bt_core::money::to_f64(t.gross_pnl))
            .sum::<f64>(),
    };
    let gross_losses: f64 = match cfg.profit_factor_basis {
        RConvention::Net => losses.iter().map(|l| -l).sum(),
        RConvention::Gross => -input
            .trades
            .iter()
            .filter(|t| t.net_pnl < dec!(0))
            .map(|t| bt_core::money::to_f64(t.gross_pnl))
            .sum::<f64>(),
    };
    let profit_factor = if gross_losses > 0.0 {
        Some(gross_wins / gross_losses)
    } else if gross_wins > 0.0 {
        None // infinite — report null with full gross data available
    } else {
        Some(0.0)
    };
    let avg_win = stats::mean(&wins);
    let avg_loss = stats::mean(&losses);
    let payoff = match (avg_win, avg_loss) {
        (Some(w), Some(l)) if l != 0.0 => Some(w / -l),
        _ => None,
    };
    let expectancy_amount = stats::mean(&net_pnls);

    // streaks (exit-ordered = ledger order)
    let mut wstreak = 0usize;
    let mut lstreak = 0usize;
    let mut max_w = 0usize;
    let mut max_l = 0usize;
    for p in &net_pnls {
        if *p > 0.0 {
            wstreak += 1;
            lstreak = 0;
        } else if *p < 0.0 {
            lstreak += 1;
            wstreak = 0;
        }
        max_w = max_w.max(wstreak);
        max_l = max_l.max(lstreak);
    }

    // --- R stats -------------------------------------------------------------
    let rs: Vec<f64> = input
        .trades
        .iter()
        .filter_map(|t| t.r_multiple.map(bt_core::money::to_f64))
        .collect();
    let r_stats = RStats {
        avg_r: stats::mean(&rs),
        median_r: stats::median(&rs),
        max_r: rs.iter().copied().reduce(f64::max),
        min_r: rs.iter().copied().reduce(f64::min),
        std_r: std_fn(&rs),
        positive_r_count: rs.iter().filter(|r| **r > 0.0).count(),
        negative_r_count: rs.iter().filter(|r| **r < 0.0).count(),
    };

    // --- monthly / yearly returns (chained from exact equity) ------------------
    let (monthly_returns, yearly_returns) = period_returns(curve, input.initial_capital);

    // --- rolling sharpe ---------------------------------------------------------
    let window = cfg.rolling_sharpe_window_days as usize;
    let rolling_sharpe = if cfg.returns_freq == ReturnsFreq::Daily && ret_ts.len() >= window {
        ret_ts
            .iter()
            .skip(window - 1)
            .enumerate()
            .map(|(i, ts)| {
                let w = &returns[i..i + window];
                let s = match (stats::mean(w), std_fn(w)) {
                    (Some(m), Some(sd)) if sd > 0.0 => Some((m - rf_period) / sd * ppy.sqrt()),
                    _ => None,
                };
                RollingPoint {
                    ts: *ts,
                    rolling_sharpe: s,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let total_net: f64 = net_pnls.iter().sum();
    let total_gross: f64 = input
        .trades
        .iter()
        .map(|t| bt_core::money::to_f64(t.gross_pnl))
        .sum();
    let recovery = match (max_dd > dec!(0), total_return) {
        (true, Some(_)) => {
            let dd_amt = bt_core::money::to_f64(max_dd);
            Some((total_gross - total_commission(input.trades)) / dd_amt)
        }
        _ => None,
    };

    let conventions = Conventions {
        returns_freq: cfg.returns_freq,
        annualization_factor: ppy,
        risk_free_rate_annual: cfg.risk_free_rate,
        risk_free_rate_per_period: rf_period,
        std_mode: cfg.std_mode,
        r_convention: cfg.r_convention,
        profit_factor_basis: cfg.profit_factor_basis,
        periods_per_year: ppy,
        cagr_year_basis: 365.25,
    };

    MetricsReport {
        conventions,
        overview: Overview {
            initial_capital: bt_core::money::to_f64(input.initial_capital),
            final_equity: bt_core::money::to_f64(final_equity),
            total_return_pct: total_return,
            cagr_pct: cagr,
            start_ts,
            end_ts,
            total_bars: curve.len(),
            exposure_pct,
            time_in_market_pct: exposure_pct,
        },
        returns: ReturnStats {
            sharpe,
            sortino,
            calmar: match (cagr, max_dd_pct > dec!(0)) {
                (Some(c), true) => Some(c / bt_core::money::to_f64(max_dd_pct)),
                (Some(_), false) => None,
                _ => None,
            },
            volatility_annualized_pct: std_r.map(|s| s * ppy.sqrt() * 100.0),
            mean_return_per_period_pct: mean_r.map(|m| m * 100.0),
            skewness: stats::skewness(&returns),
            kurtosis_excess: stats::kurtosis_excess(&returns),
            best_period_pct: returns.iter().copied().reduce(f64::max).map(|r| r * 100.0),
            worst_period_pct: returns.iter().copied().reduce(f64::min).map(|r| r * 100.0),
        },
        risk: RiskStats {
            max_drawdown_amount: bt_core::money::to_f64(max_dd),
            max_drawdown_pct: bt_core::money::to_f64(max_dd_pct),
            max_drawdown_start: max_dd_start,
            max_drawdown_end: max_dd_end,
            average_drawdown_pct: avg_dd,
            ulcer_index: ulcer,
            recovery_factor: recovery,
            risk_of_ruin_pct: input.risk_of_ruin_pct,
        },
        trades: TradeStats {
            total_trades: input.trades.len(),
            winning_trades: wins.len(),
            losing_trades: losses.len(),
            breakeven_trades: input.trades.len() - wins.len() - losses.len(),
            win_rate_pct: if input.trades.is_empty() {
                None
            } else {
                Some(wins.len() as f64 / input.trades.len() as f64 * 100.0)
            },
            loss_rate_pct: if input.trades.is_empty() {
                None
            } else {
                Some(losses.len() as f64 / input.trades.len() as f64 * 100.0)
            },
            profit_factor,
            avg_win,
            avg_loss,
            payoff_ratio: payoff,
            expectancy_amount,
            total_net_pnl: total_net,
            total_gross_pnl: total_gross,
            total_commission: total_commission(input.trades),
            total_spread_cost: input
                .trades
                .iter()
                .map(|t| bt_core::money::to_f64(t.spread_cost))
                .sum(),
            total_slippage_cost: input
                .trades
                .iter()
                .map(|t| bt_core::money::to_f64(t.slippage_cost))
                .sum(),
            total_financing: input
                .trades
                .iter()
                .map(|t| bt_core::money::to_f64(t.financing))
                .sum(),
            avg_holding_bars: stats::mean(
                &input
                    .trades
                    .iter()
                    .map(|t| t.holding_bars as f64)
                    .collect::<Vec<_>>(),
            ),
            avg_holding_time_hours: stats::mean(
                &input
                    .trades
                    .iter()
                    .map(|t| t.holding_time_secs as f64 / 3600.0)
                    .collect::<Vec<_>>(),
            ),
            longest_winning_streak: max_w,
            longest_losing_streak: max_l,
            trades_without_stop: input
                .trades
                .iter()
                .filter(|t| t.initial_risk.is_none())
                .count(),
        },
        r_stats,
        distributions: Distributions {
            r_multiples: rs,
            mae: input
                .trades
                .iter()
                .map(|t| bt_core::money::to_f64(t.mae))
                .collect(),
            mfe: input
                .trades
                .iter()
                .map(|t| bt_core::money::to_f64(t.mfe))
                .collect(),
            returns_pct: returns.iter().map(|r| r * 100.0).collect(),
        },
        monthly_returns,
        yearly_returns,
        rolling_sharpe,
    }
}

fn total_commission(trades: &[TradeRecord]) -> f64 {
    trades
        .iter()
        .map(|t| bt_core::money::to_f64(t.commission))
        .sum()
}

fn period_returns(curve: &[EquityPoint], initial: D) -> (Vec<PeriodReturn>, Vec<PeriodReturn>) {
    // Last equity per month / per year, chained.
    let mut monthly: BTreeMap<(i32, u32), D> = BTreeMap::new();
    let mut yearly: BTreeMap<i32, D> = BTreeMap::new();
    for p in curve {
        monthly.insert((p.ts.year(), p.ts.month()), p.equity);
        yearly.insert(p.ts.year(), p.equity);
    }
    let mut prev = initial;
    let mut mout = Vec::new();
    for ((y, m), eq) in &monthly {
        let r = if prev.is_zero() {
            0.0
        } else {
            bt_core::money::to_f64((*eq / prev - dec!(1)) * dec!(100))
        };
        mout.push(PeriodReturn {
            year: *y,
            month: Some(*m),
            return_pct: r,
            end_equity: bt_core::money::to_f64(*eq),
        });
        prev = *eq;
    }
    let mut prev = initial;
    let mut yout = Vec::new();
    for (y, eq) in &yearly {
        let r = if prev.is_zero() {
            0.0
        } else {
            bt_core::money::to_f64((*eq / prev - dec!(1)) * dec!(100))
        };
        yout.push(PeriodReturn {
            year: *y,
            month: None,
            return_pct: r,
            end_equity: bt_core::money::to_f64(*eq),
        });
        prev = *eq;
    }
    (mout, yout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn ts(s: &str) -> Ts {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        bt_core::time::parse_timestamp(s, utc, "t").unwrap()
    }

    fn point(t: &str, equity: D) -> EquityPoint {
        let peak = equity.max(dec!(100000));
        let dd = peak - equity;
        EquityPoint {
            ts: ts(t),
            equity,
            balance: equity,
            unrealized: dec!(0),
            drawdown: dd,
            drawdown_pct: if peak.is_zero() {
                dec!(0)
            } else {
                dd / peak * dec!(100)
            },
            in_position: false,
        }
    }

    fn cfg() -> AnalyticsConfig {
        AnalyticsConfig {
            annualization_factor: Some(252.0),
            ..Default::default()
        }
    }

    #[test]
    fn max_drawdown_reference() {
        // Spec test 16: 100k -> 120k -> 90k -> 110k: max dd = 30k (25%).
        let curve = vec![
            point("2024-01-01T00:00:00Z", dec!(100000)),
            point("2024-01-02T00:00:00Z", dec!(120000)),
            point("2024-01-03T00:00:00Z", dec!(90000)),
            point("2024-01-04T00:00:00Z", dec!(110000)),
        ];
        let m = compute_metrics(&MetricsInput {
            equity_curve: &curve,
            trades: &[],
            initial_capital: dec!(100000),
            bar_secs: 86_400,
            config: &cfg(),
            risk_of_ruin_pct: 0.0,
        });
        assert!((m.risk.max_drawdown_amount - 30000.0).abs() < 1e-6);
        assert!((m.risk.max_drawdown_pct - 25.0).abs() < 1e-9);
    }

    #[test]
    fn sharpe_manual_reference() {
        // Returns exactly [1.0, 1.0] in f64 (exact powers of two) => std 0 => null Sharpe.
        let curve = vec![
            point("2024-01-02T00:00:00Z", dec!(200000)),
            point("2024-01-03T00:00:00Z", dec!(400000)),
        ];
        let m = compute_metrics(&MetricsInput {
            equity_curve: &curve,
            trades: &[],
            initial_capital: dec!(100000),
            bar_secs: 86_400,
            config: &cfg(),
            risk_of_ruin_pct: 0.0,
        });
        assert!(
            m.returns.sharpe.is_none(),
            "zero dispersion must yield null Sharpe"
        );
    }

    #[test]
    fn sharpe_nonzero_reference() {
        // returns [0.1, -0.05, 0.08]: mean=0.043333, sample std=0.079644...
        let curve = vec![
            point("2024-01-02T00:00:00Z", dec!(110000)),
            point("2024-01-03T00:00:00Z", dec!(104500)),
            point("2024-01-04T00:00:00Z", dec!(112860)),
        ];
        let m = compute_metrics(&MetricsInput {
            equity_curve: &curve,
            trades: &[],
            initial_capital: dec!(100000),
            bar_secs: 86_400,
            config: &cfg(),
            risk_of_ruin_pct: 0.0,
        });
        let mean_r: f64 = (0.1 - 0.05 + 0.08) / 3.0;
        let var =
            ((0.1f64 - mean_r).powi(2) + (-0.05f64 - mean_r).powi(2) + (0.08f64 - mean_r).powi(2))
                / 2.0;
        let expected = mean_r / var.sqrt() * 252f64.sqrt();
        let got = m.returns.sharpe.unwrap();
        assert!(
            (got - expected).abs() < 1e-9,
            "got {got}, expected {expected}"
        );
    }

    #[test]
    fn trade_stats_reference() {
        let t = |net: D| TradeRecord {
            trade_id: 1,
            symbol: "X".into(),
            direction: "long".into(),
            entry_timestamp: ts("2024-01-01T00:00:00Z"),
            entry_price: dec!(100),
            exit_timestamp: ts("2024-01-02T00:00:00Z"),
            exit_price: dec!(101),
            quantity: dec!(1),
            gross_pnl: net,
            commission: dec!(0),
            spread_cost: dec!(0),
            slippage_cost: dec!(0),
            financing: dec!(0),
            dividends: dec!(0),
            net_pnl: net,
            initial_risk: Some(dec!(10)),
            r_multiple: Some(net / dec!(10)),
            holding_bars: 1,
            holding_time_secs: 86_400,
            mae: dec!(1),
            mfe: dec!(2),
            entry_reason: "entry".into(),
            exit_reason: "exit".into(),
        };
        let trades = vec![t(dec!(20)), t(dec!(-10)), t(dec!(5))];
        let m = compute_metrics(&MetricsInput {
            equity_curve: &[],
            trades: &trades,
            initial_capital: dec!(100000),
            bar_secs: 86_400,
            config: &cfg(),
            risk_of_ruin_pct: 0.0,
        });
        assert_eq!(m.trades.total_trades, 3);
        assert_eq!(m.trades.winning_trades, 2);
        assert_eq!(m.trades.losing_trades, 1);
        assert!((m.trades.win_rate_pct.unwrap() - 200.0 / 3.0).abs() < 1e-9);
        assert!((m.r_stats.avg_r.unwrap() - (2.0 - 1.0 + 0.5) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn monthly_returns_chain() {
        let curve = vec![
            point("2024-01-15T00:00:00Z", dec!(110000)),
            point("2024-01-31T00:00:00Z", dec!(120000)),
            point("2024-02-20T00:00:00Z", dec!(96000)),
        ];
        let (monthly, _yearly) = period_returns(&curve, dec!(100000));
        assert_eq!(monthly.len(), 2);
        assert!((monthly[0].return_pct - 20.0).abs() < 1e-9);
        assert!((monthly[1].return_pct + 20.0).abs() < 1e-9);
    }
}
