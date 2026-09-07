//! Indicator engine (spec §15). All indicators are **causal**: they only see
//! values pushed so far and return `None` until enough data exists.
//!
//! Conventions (documented in METRICS/STRATEGY_SPEC):
//! - SMA: mean of last `period` values.
//! - EMA: seeded with SMA of the first `period` values.
//! - WMA: linear weights 1..period (most recent = period).
//! - RSI: Wilder smoothing; first value after `period` deltas.
//! - ATR: Wilder-smoothed true range; seeded with SMA of TR over `period`.
//! - StdDev: population stddev over the window (consistent with Bollinger).
//! - MACD: EMA(fast) − EMA(slow); signal = EMA(signal) of MACD.
//! - Bollinger: SMA ± k × population stddev.

use bt_core::D;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Price field used as an indicator source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Open,
    High,
    Low,
    Close,
    Volume,
    Hl2,
    Hlc3,
    Ohlc4,
}

impl Source {
    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "open" => Some(Source::Open),
            "high" => Some(Source::High),
            "low" => Some(Source::Low),
            "close" => Some(Source::Close),
            "volume" => Some(Source::Volume),
            "hl2" => Some(Source::Hl2),
            "hlc3" => Some(Source::Hlc3),
            "ohlc4" => Some(Source::Ohlc4),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Open => "open",
            Source::High => "high",
            Source::Low => "low",
            Source::Close => "close",
            Source::Volume => "volume",
            Source::Hl2 => "hl2",
            Source::Hlc3 => "hlc3",
            Source::Ohlc4 => "ohlc4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacdComponent {
    Macd,
    Signal,
    Hist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BollComponent {
    Upper,
    Middle,
    Lower,
}

/// Full identification of one indicator node (per symbol/timeframe).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndicatorKind {
    Sma {
        period: u32,
    },
    Ema {
        period: u32,
    },
    Wma {
        period: u32,
    },
    Rsi {
        period: u32,
    },
    StdDev {
        period: u32,
    },
    Highest {
        period: u32,
    },
    Lowest {
        period: u32,
    },
    Roc {
        period: u32,
    },
    Atr {
        period: u32,
    },
    Macd {
        fast: u32,
        slow: u32,
        signal: u32,
        component: MacdComponent,
    },
    Bollinger {
        period: u32,
        k: D,
        component: BollComponent,
    },
}

impl IndicatorKind {
    /// Canonical, deterministic key fragment (used for dedup and hashing).
    pub fn key(&self) -> String {
        match self {
            IndicatorKind::Sma { period } => format!("sma({period})"),
            IndicatorKind::Ema { period } => format!("ema({period})"),
            IndicatorKind::Wma { period } => format!("wma({period})"),
            IndicatorKind::Rsi { period } => format!("rsi({period})"),
            IndicatorKind::StdDev { period } => format!("stddev({period})"),
            IndicatorKind::Highest { period } => format!("highest({period})"),
            IndicatorKind::Lowest { period } => format!("lowest({period})"),
            IndicatorKind::Roc { period } => format!("roc({period})"),
            IndicatorKind::Atr { period } => format!("atr({period})"),
            IndicatorKind::Macd {
                fast,
                slow,
                signal,
                component,
            } => format!(
                "macd({fast},{slow},{signal}).{}",
                match component {
                    MacdComponent::Macd => "macd",
                    MacdComponent::Signal => "signal",
                    MacdComponent::Hist => "hist",
                }
            ),
            IndicatorKind::Bollinger {
                period,
                k,
                component,
            } => format!(
                "boll({period},{k}).{}",
                match component {
                    BollComponent::Upper => "upper",
                    BollComponent::Middle => "middle",
                    BollComponent::Lower => "lower",
                }
            ),
        }
    }

    /// Validate parameters (compile-time; spec §44: reject, don't invent).
    pub fn validate(&self) -> Result<(), String> {
        let pos = |p: u32, name: &str| {
            if p == 0 {
                Err(format!("{name} period must be >= 1"))
            } else {
                Ok(())
            }
        };
        match self {
            IndicatorKind::Sma { period }
            | IndicatorKind::Ema { period }
            | IndicatorKind::Wma { period }
            | IndicatorKind::Rsi { period }
            | IndicatorKind::StdDev { period }
            | IndicatorKind::Highest { period }
            | IndicatorKind::Lowest { period }
            | IndicatorKind::Roc { period }
            | IndicatorKind::Atr { period } => pos(*period, "indicator"),
            IndicatorKind::Macd {
                fast, slow, signal, ..
            } => {
                pos(*fast, "macd fast")?;
                pos(*slow, "macd slow")?;
                pos(*signal, "macd signal")?;
                if fast >= slow {
                    return Err("macd fast must be < slow".into());
                }
                Ok(())
            }
            IndicatorKind::Bollinger { period, k, .. } => {
                pos(*period, "bollinger")?;
                if *k <= dec!(0) {
                    return Err("bollinger k must be > 0".into());
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming implementations
// ---------------------------------------------------------------------------

pub struct Sma {
    period: usize,
    window: VecDeque<D>,
    sum: D,
}

impl Sma {
    pub fn new(period: u32) -> Self {
        Sma {
            period: period as usize,
            window: VecDeque::new(),
            sum: dec!(0),
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        match v {
            Some(x) => {
                self.window.push_back(x);
                self.sum += x;
                if self.window.len() > self.period {
                    if let Some(old) = self.window.pop_front() {
                        self.sum -= old;
                    }
                }
                if self.window.len() == self.period {
                    Some(self.sum / D::from(self.period))
                } else {
                    None
                }
            }
            None => {
                // Missing source values break the window (documented).
                self.window.clear();
                self.sum = dec!(0);
                None
            }
        }
    }
}

pub struct Ema {
    period: usize,
    alpha: D,
    seed_sum: D,
    seed_count: usize,
    prev: Option<D>,
}

impl Ema {
    pub fn new(period: u32) -> Self {
        let p = D::from(period);
        Ema {
            period: period as usize,
            alpha: dec!(2) / (p + dec!(1)),
            seed_sum: dec!(0),
            seed_count: 0,
            prev: None,
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            // Missing source values break the window (documented, conservative:
            // averages are never computed across data gaps).
            self.reset();
            return None;
        };
        match self.prev {
            None => {
                self.seed_sum += x;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    let seed = self.seed_sum / D::from(self.period);
                    self.prev = Some(seed);
                    Some(seed)
                } else {
                    None
                }
            }
            Some(p) => {
                let e = p + self.alpha * (x - p);
                self.prev = Some(e);
                Some(e)
            }
        }
    }

    fn reset(&mut self) {
        self.seed_sum = dec!(0);
        self.seed_count = 0;
        self.prev = None;
    }
}

pub struct Wma {
    period: usize,
    window: VecDeque<D>,
    weight_sum: D,
}

impl Wma {
    pub fn new(period: u32) -> Self {
        let p = period as usize;
        let weight_sum = (1..=p).map(D::from).sum();
        Wma {
            period: p,
            window: VecDeque::new(),
            weight_sum,
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.window.clear();
            return None;
        };
        self.window.push_back(x);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        if self.window.len() < self.period {
            return None;
        }
        let n = self.window.len();
        let acc: D = self
            .window
            .iter()
            .enumerate()
            .map(|(i, v)| v * D::from(i + 1))
            .sum();
        let _ = n;
        Some(acc / self.weight_sum)
    }
}

pub struct Rsi {
    period: usize,
    prev_close: Option<D>,
    deltas_seen: usize,
    seed_gain: D,
    seed_loss: D,
    avg_gain: Option<D>,
    avg_loss: Option<D>,
}

impl Rsi {
    pub fn new(period: u32) -> Self {
        Rsi {
            period: period as usize,
            prev_close: None,
            deltas_seen: 0,
            seed_gain: dec!(0),
            seed_loss: dec!(0),
            avg_gain: None,
            avg_loss: None,
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.reset();
            return None;
        };
        let Some(prev) = self.prev_close else {
            self.prev_close = Some(x);
            return None;
        };
        self.prev_close = Some(x);
        let delta = x - prev;
        let gain = delta.max(dec!(0));
        let loss = (-delta).max(dec!(0));
        match (self.avg_gain, self.avg_loss) {
            (None, None) => {
                self.seed_gain += gain;
                self.seed_loss += loss;
                self.deltas_seen += 1;
                if self.deltas_seen == self.period {
                    let g = self.seed_gain / D::from(self.period);
                    let l = self.seed_loss / D::from(self.period);
                    self.avg_gain = Some(g);
                    self.avg_loss = Some(l);
                    Some(rsi_from(g, l))
                } else {
                    None
                }
            }
            (Some(g0), Some(l0)) => {
                let p = D::from(self.period);
                let g = (g0 * (p - dec!(1)) + gain) / p;
                let l = (l0 * (p - dec!(1)) + loss) / p;
                self.avg_gain = Some(g);
                self.avg_loss = Some(l);
                Some(rsi_from(g, l))
            }
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.prev_close = None;
        self.deltas_seen = 0;
        self.seed_gain = dec!(0);
        self.seed_loss = dec!(0);
        self.avg_gain = None;
        self.avg_loss = None;
    }
}

fn rsi_from(avg_gain: D, avg_loss: D) -> D {
    let denom = avg_gain + avg_loss;
    if denom.is_zero() {
        // No movement at all: conventionally neutral-high (no losses).
        dec!(100)
    } else {
        dec!(100) * avg_gain / denom
    }
}

/// Wilder-smoothed average (used by ATR).
pub struct WilderAverage {
    period: usize,
    seed_sum: D,
    count: usize,
    current: Option<D>,
}

impl WilderAverage {
    pub fn new(period: u32) -> Self {
        WilderAverage {
            period: period as usize,
            seed_sum: dec!(0),
            count: 0,
            current: None,
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.reset();
            return None;
        };
        match self.current {
            None => {
                self.seed_sum += x;
                self.count += 1;
                if self.count == self.period {
                    let seed = self.seed_sum / D::from(self.period);
                    self.current = Some(seed);
                    Some(seed)
                } else {
                    None
                }
            }
            Some(c) => {
                let p = D::from(self.period);
                let nxt = (c * (p - dec!(1)) + x) / p;
                self.current = Some(nxt);
                Some(nxt)
            }
        }
    }

    fn reset(&mut self) {
        self.seed_sum = dec!(0);
        self.count = 0;
        self.current = None;
    }
}

pub struct Atr {
    wilder: WilderAverage,
    prev_close: Option<D>,
}

impl Atr {
    pub fn new(period: u32) -> Self {
        Atr {
            wilder: WilderAverage::new(period),
            prev_close: None,
        }
    }
    /// True range from OHLC, then Wilder smoothing.
    pub fn push_ohlc(&mut self, high: D, low: D, close: D) -> Option<D> {
        let tr = match self.prev_close {
            None => high - low,
            Some(pc) => {
                let a = high - low;
                let b = (high - pc).abs();
                let c = (low - pc).abs();
                a.max(b).max(c)
            }
        };
        self.prev_close = Some(close);
        self.wilder.push(Some(tr))
    }
}

pub struct StdDev {
    period: usize,
    window: VecDeque<D>,
}

impl StdDev {
    pub fn new(period: u32) -> Self {
        StdDev {
            period: period as usize,
            window: VecDeque::new(),
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.window.clear();
            return None;
        };
        self.window.push_back(x);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        if self.window.len() < self.period {
            return None;
        }
        let n = D::from(self.window.len());
        let mean = self.window.iter().copied().sum::<D>() / n;
        // Population variance; sqrt crosses to f64 (documented boundary).
        let var = self
            .window
            .iter()
            .map(|x| (*x - mean) * (*x - mean))
            .sum::<D>()
            / n;
        let sd = bt_core::money::d_from_f64(bt_core::money::to_f64(var).sqrt()).unwrap_or(dec!(0));
        Some(sd)
    }
}

pub struct Highest {
    period: usize,
    window: VecDeque<D>,
}

impl Highest {
    pub fn new(period: u32) -> Self {
        Highest {
            period: period as usize,
            window: VecDeque::new(),
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.window.clear();
            return None;
        };
        self.window.push_back(x);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        if self.window.len() < self.period {
            return None;
        }
        Some(self.window.iter().copied().max().unwrap())
    }
}

pub struct Lowest {
    period: usize,
    window: VecDeque<D>,
}

impl Lowest {
    pub fn new(period: u32) -> Self {
        Lowest {
            period: period as usize,
            window: VecDeque::new(),
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.window.clear();
            return None;
        };
        self.window.push_back(x);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        if self.window.len() < self.period {
            return None;
        }
        Some(self.window.iter().copied().min().unwrap())
    }
}

pub struct Roc {
    period: usize,
    history: VecDeque<D>,
}

impl Roc {
    pub fn new(period: u32) -> Self {
        Roc {
            period: period as usize,
            history: VecDeque::new(),
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let Some(x) = v else {
            self.history.clear();
            return None;
        };
        self.history.push_back(x);
        if self.history.len() > self.period + 1 {
            self.history.pop_front();
        }
        if self.history.len() < self.period + 1 {
            return None;
        }
        let past = self.history.front().unwrap();
        if past.is_zero() {
            return None;
        }
        Some((x / past - dec!(1)) * dec!(100))
    }
}

pub struct Macd {
    fast: Ema,
    slow: Ema,
    signal: Ema,
    component: MacdComponent,
}

impl Macd {
    pub fn new(fast: u32, slow: u32, signal: u32, component: MacdComponent) -> Self {
        Macd {
            fast: Ema::new(fast),
            slow: Ema::new(slow),
            signal: Ema::new(signal),
            component,
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let f = self.fast.push(v);
        let s = self.slow.push(v);
        let macd = match (f, s) {
            (Some(f), Some(s)) => Some(f - s),
            _ => None,
        };
        let sig = self.signal.push(macd);
        match self.component {
            MacdComponent::Macd => macd,
            MacdComponent::Signal => sig,
            MacdComponent::Hist => match (macd, sig) {
                (Some(m), Some(s)) => Some(m - s),
                _ => None,
            },
        }
    }
}

pub struct Bollinger {
    sma: Sma,
    sd: StdDev,
    k: D,
    component: BollComponent,
}

impl Bollinger {
    pub fn new(period: u32, k: D, component: BollComponent) -> Self {
        Bollinger {
            sma: Sma::new(period),
            sd: StdDev::new(period),
            k,
            component,
        }
    }
    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        let mid = self.sma.push(v);
        let sd = self.sd.push(v);
        match (mid, sd) {
            (Some(m), Some(s)) => match self.component {
                BollComponent::Middle => Some(m),
                BollComponent::Upper => Some(m + self.k * s),
                BollComponent::Lower => Some(m - self.k * s),
            },
            _ => None,
        }
    }
}

/// The streaming form of an [`IndicatorKind`]. `push` consumes the source
/// value; ATR consumes OHLC via [`Streamer::push_ohlc`].
pub enum Streamer {
    Value(Box<dyn ValueStreamer>),
    Atr(Atr),
}

pub trait ValueStreamer {
    fn push(&mut self, v: Option<D>) -> Option<D>;
}

impl ValueStreamer for Sma {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Sma::push(self, v)
    }
}
impl ValueStreamer for Ema {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Ema::push(self, v)
    }
}
impl ValueStreamer for Wma {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Wma::push(self, v)
    }
}
impl ValueStreamer for Rsi {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Rsi::push(self, v)
    }
}
impl ValueStreamer for StdDev {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        StdDev::push(self, v)
    }
}
impl ValueStreamer for Highest {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Highest::push(self, v)
    }
}
impl ValueStreamer for Lowest {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Lowest::push(self, v)
    }
}
impl ValueStreamer for Roc {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Roc::push(self, v)
    }
}
impl ValueStreamer for Macd {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Macd::push(self, v)
    }
}
impl ValueStreamer for Bollinger {
    fn push(&mut self, v: Option<D>) -> Option<D> {
        Bollinger::push(self, v)
    }
}

impl Streamer {
    pub fn new(kind: &IndicatorKind) -> Streamer {
        match kind {
            IndicatorKind::Sma { period } => Streamer::Value(Box::new(Sma::new(*period))),
            IndicatorKind::Ema { period } => Streamer::Value(Box::new(Ema::new(*period))),
            IndicatorKind::Wma { period } => Streamer::Value(Box::new(Wma::new(*period))),
            IndicatorKind::Rsi { period } => Streamer::Value(Box::new(Rsi::new(*period))),
            IndicatorKind::StdDev { period } => Streamer::Value(Box::new(StdDev::new(*period))),
            IndicatorKind::Highest { period } => Streamer::Value(Box::new(Highest::new(*period))),
            IndicatorKind::Lowest { period } => Streamer::Value(Box::new(Lowest::new(*period))),
            IndicatorKind::Roc { period } => Streamer::Value(Box::new(Roc::new(*period))),
            IndicatorKind::Atr { period } => Streamer::Atr(Atr::new(*period)),
            IndicatorKind::Macd {
                fast,
                slow,
                signal,
                component,
            } => Streamer::Value(Box::new(Macd::new(*fast, *slow, *signal, *component))),
            IndicatorKind::Bollinger {
                period,
                k,
                component,
            } => Streamer::Value(Box::new(Bollinger::new(*period, *k, *component))),
        }
    }

    pub fn push(&mut self, v: Option<D>) -> Option<D> {
        match self {
            Streamer::Value(s) => s.push(v),
            Streamer::Atr(_) => None, // ATR needs push_ohlc
        }
    }

    pub fn push_ohlc(&mut self, high: D, low: D, close: D) -> Option<D> {
        match self {
            Streamer::Atr(a) => a.push_ohlc(high, low, close),
            Streamer::Value(_) => None,
        }
    }
}

/// Source value of one bar.
pub fn source_value(source: Source, o: D, h: D, l: D, c: D, v: Option<D>) -> Option<D> {
    match source {
        Source::Open => Some(o),
        Source::High => Some(h),
        Source::Low => Some(l),
        Source::Close => Some(c),
        Source::Volume => v,
        Source::Hl2 => Some((h + l) / dec!(2)),
        Source::Hlc3 => Some((h + l + c) / dec!(3)),
        Source::Ohlc4 => Some((o + h + l + c) / dec!(4)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(values: &[Option<D>], ind: &mut dyn ValueStreamer) -> Vec<Option<D>> {
        values.iter().map(|v| ind.push(*v)).collect()
    }

    #[test]
    fn sma_reference_values() {
        // 1..=5, period 3 => [None, None, 2, 3, 4]
        let vals: Vec<Option<D>> = [1, 2, 3, 4, 5].iter().map(|i| Some(D::from(*i))).collect();
        let out = series(&vals, &mut Sma::new(3));
        assert_eq!(out[2], Some(D::from(2)));
        assert_eq!(out[3], Some(D::from(3)));
        assert_eq!(out[4], Some(D::from(4)));
    }

    #[test]
    fn ema_seeded_with_sma() {
        // period 3 over 1..=6: seed = 2 at index 2; alpha = 0.5
        let vals: Vec<Option<D>> = [1, 2, 3, 4, 5, 6]
            .iter()
            .map(|i| Some(D::from(*i)))
            .collect();
        let out = series(&vals, &mut Ema::new(3));
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(dec!(2)));
        // e3 = 2 + 0.5*(4-2) = 3
        assert_eq!(out[3], Some(dec!(3)));
        // e4 = 3 + 0.5*(5-3) = 4
        assert_eq!(out[4], Some(dec!(4)));
        // e5 = 4 + 0.5*(6-4) = 5
        assert_eq!(out[5], Some(dec!(5)));
    }

    #[test]
    fn wma_weights() {
        // period 3 over 1..=5: wma(3,4,5) = (3*1+4*2+5*3)/6 = 26/6
        let vals: Vec<Option<D>> = [1, 2, 3, 4, 5].iter().map(|i| Some(D::from(*i))).collect();
        let out = series(&vals, &mut Wma::new(3));
        assert_eq!(out[4], Some(dec!(26) / dec!(6)));
    }

    #[test]
    fn rsi_wilder_reference() {
        // Classic 14-period RSI is hard to hand-check; use period 1:
        // period 1 => avg = last gain/loss directly.
        // closes: 10, 11, 10, 9, 10 => gains: 1,0,0,1; losses: 0,1,1,0
        let vals: Vec<Option<D>> = [10, 11, 10, 9, 10]
            .iter()
            .map(|i| Some(D::from(*i)))
            .collect();
        let out = series(&vals, &mut Rsi::new(1));
        // deltas: +1,-1,-1,+1 => after each delta rsi = 100*g/(g+l)
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some(dec!(100))); // gain 1, loss 0
        assert_eq!(out[2], Some(dec!(0))); // gain 0, loss 1
        assert_eq!(out[3], Some(dec!(0)));
        assert_eq!(out[4], Some(dec!(100)));
    }

    #[test]
    fn atr_true_range_and_wilder() {
        // WilderAverage(1) seeds immediately, so ATR(1) is defined from bar 1.
        let mut a = Atr::new(1);
        let r1 = a.push_ohlc(dec!(12), dec!(10), dec!(11));
        assert_eq!(r1, Some(dec!(2))); // tr = high - low
        let r2 = a.push_ohlc(dec!(15), dec!(13), dec!(14));
        // tr = max(15-13, |15-11|, |13-11|) = 4; avg = (0*prev + 4)/1 = 4
        assert_eq!(r2, Some(dec!(4)));
    }

    #[test]
    fn stddev_population() {
        // [1,2,3] population sd = sqrt(2/3)
        let vals: Vec<Option<D>> = [1, 2, 3].iter().map(|i| Some(D::from(*i))).collect();
        let out = series(&vals, &mut StdDev::new(3));
        let expected = (dec!(2) / dec!(3)).to_string();
        // compare via f64 because of the sqrt boundary
        let got = out[2].unwrap();
        assert!(
            (bt_core::money::to_f64(got) - 0.816496580927726).abs() < 1e-12,
            "got {got} (expected sqrt(2/3)≈{expected})"
        );
    }

    #[test]
    fn highest_lowest_including_current() {
        let vals: Vec<Option<D>> = [3, 1, 4, 1, 5].iter().map(|i| Some(D::from(*i))).collect();
        let hi = series(&vals, &mut Highest::new(3));
        assert_eq!(hi[2], Some(D::from(4)));
        assert_eq!(hi[3], Some(D::from(4)));
        assert_eq!(hi[4], Some(D::from(5)));
        let lo_vals: Vec<Option<D>> = [3, 1, 4].iter().map(|i| Some(D::from(*i))).collect();
        let lo = series(&lo_vals, &mut Lowest::new(3));
        assert_eq!(lo[2], Some(D::from(1)));
    }

    #[test]
    fn roc_reference() {
        // close 100 then 110: roc = 10%
        let vals: Vec<Option<D>> = [100, 110].iter().map(|i| Some(D::from(*i))).collect();
        let out = series(&vals, &mut Roc::new(1));
        assert_eq!(out[1], Some(dec!(10)));
    }

    #[test]
    fn macd_components() {
        // fast=2 (seeds at idx 1), slow=4 (seeds at idx 3), signal=2 of macd.
        let vals: Vec<Option<D>> = [1, 2, 3, 4, 5, 6]
            .iter()
            .map(|i| Some(D::from(*i)))
            .collect();
        let mut m = Macd::new(2, 4, 2, MacdComponent::Macd);
        let out: Vec<Option<D>> = vals.iter().map(|v| m.push(*v)).collect();
        assert!(out[2].is_none(), "macd needs the slow EMA seed");
        assert!(out[3].is_some(), "macd defined once the slow EMA seeds");
        let mut m2 = Macd::new(2, 4, 2, MacdComponent::Signal);
        let out2: Vec<Option<D>> = vals.iter().map(|v| m2.push(*v)).collect();
        assert!(out2[3].is_none(), "signal needs two macd values");
        assert!(out2[4].is_some());
        let mut m3 = Macd::new(2, 4, 2, MacdComponent::Hist);
        let out3: Vec<Option<D>> = vals.iter().map(|v| m3.push(*v)).collect();
        assert!(out3[4].is_some());
    }

    #[test]
    fn bollinger_middle_is_sma() {
        let vals: Vec<Option<D>> = [1, 2, 3, 4].iter().map(|i| Some(D::from(*i))).collect();
        let mut b = Bollinger::new(3, dec!(2), BollComponent::Middle);
        let out: Vec<Option<D>> = vals.iter().map(|v| b.push(*v)).collect();
        assert_eq!(out[3], Some(D::from(3)));
        let mut b2 = Bollinger::new(3, dec!(2), BollComponent::Upper);
        let out2: Vec<Option<D>> = vals.iter().map(|v| b2.push(*v)).collect();
        // sd([2,3,4]) = sqrt(2/3); upper = 3 + 2*sqrt(2/3)
        let expected = 3.0 + 2.0 * 0.816496580927726;
        assert!((bt_core::money::to_f64(out2[3].unwrap()) - expected).abs() < 1e-12);
    }

    #[test]
    fn none_source_propagates() {
        let out = series(
            &[Some(D::from(1)), None, Some(D::from(3))],
            &mut Sma::new(2),
        );
        assert_eq!(out[1], None);
        assert_eq!(
            out[2], None,
            "None must break the window (no partial averages)"
        );
    }
}
