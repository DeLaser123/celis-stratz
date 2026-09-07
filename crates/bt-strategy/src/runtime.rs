//! Compiled strategy + indicator runtime.
//!
//! Two indicator computation modes with **identical outputs** (differential
//! tested, see TESTING.md):
//! - [`IndicatorMode::Precompute`]: every unique indicator's full series is
//!   computed once upfront (shares one series across duplicate nodes).
//! - [`IndicatorMode::Streaming`]: indicators are updated incrementally as
//!   bars close (reference path).

use crate::expr::{BoolOp, EvalContext, Expr, ExprType, IndicatorSpec, Value};
use crate::indicators::{source_value, Atr, IndicatorKind, Source, Streamer};
use crate::spec::{Direction, StrategySpec};
use bt_core::{CoreResult, D};
use bt_data::{Bar, BarSeries};
use bt_risk::SizingMode;
use std::collections::{BTreeMap, VecDeque};

/// Read-only view over a bar series with a visibility cursor. `cursor` =
/// number of observable bars; `field(src, offset)` reads `cursor-1-offset`.
pub struct BarAccess<'a> {
    pub bars: &'a [Bar],
    pub cursor: usize,
}

impl<'a> BarAccess<'a> {
    pub fn new(bars: &'a [Bar], cursor: usize) -> Self {
        BarAccess { bars, cursor }
    }

    pub fn field(&self, source: Source, offset: usize) -> Value {
        if self.cursor > offset && !self.bars.is_empty() {
            let idx = self.cursor - 1 - offset;
            if idx < self.bars.len() {
                let bar = &self.bars[idx];
                return source_value(source, bar.open, bar.high, bar.low, bar.close, bar.volume)
                    .map(Value::Num)
                    .unwrap_or(Value::NA);
            }
        }
        Value::NA
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndicatorMode {
    Precompute,
    Streaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSide {
    Long,
    Short,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub exprs: Vec<Expr>,
    pub op: BoolOp,
    pub side: RuleSide,
}

impl Rule {
    fn matches(&self, ctx: &EvalContext) -> bool {
        match self.op {
            BoolOp::All => self
                .exprs
                .iter()
                .all(|e| e.eval(ctx, 0).as_bool() == Some(true)),
            BoolOp::Any => self
                .exprs
                .iter()
                .any(|e| e.eval(ctx, 0).as_bool() == Some(true)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    EnterLong,
    EnterShort,
    Exit,
    None,
}

/// A fully validated, executable strategy.
#[derive(Debug, Clone)]
pub struct CompiledStrategy {
    pub name: String,
    pub symbols: Vec<String>,
    pub entry_long: Option<Rule>,
    pub entry_short: Option<Rule>,
    pub exit: Option<Rule>,
    pub entry: crate::spec::EntryOrderDef,
    pub stop_loss: crate::spec::StopLossDef,
    pub take_profit: crate::spec::TakeProfitDef,
    pub trailing_stop: Option<crate::spec::TrailingDef>,
    pub sizing: SizingMode,
    /// Deduped indicators in deterministic (sorted key) order.
    pub indicators: Vec<IndicatorSpec>,
    /// Extra timeframes: (name, seconds), sorted by name.
    pub htfs: Vec<(String, i64)>,
    /// Deepest lookback requested by any pct_change (ring sizing).
    pub max_offset: usize,
}

impl CompiledStrategy {
    pub fn htf_secs(&self, name: &str) -> Option<i64> {
        self.htfs.iter().find(|(n, _)| n == name).map(|(_, s)| *s)
    }
}

/// Compile + validate a spec. Returns ALL validation errors (spec §44:
/// `INVALID` with exact reasons, never invented logic).
pub fn compile(spec: &StrategySpec, base_secs: i64) -> Result<CompiledStrategy, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    if spec.name.trim().is_empty() {
        errors.push("strategy.name must be non-empty".into());
    }
    if spec.symbols.is_empty() {
        errors.push("strategy.symbols must be non-empty".into());
    }

    // Timeframes
    let declared: Vec<String> = spec.timeframes.clone();
    let mut referenced: Vec<String> = Vec::new();
    for rule in [
        Some(rule_from(&spec.entry.when, RuleSide::Long)),
        spec.entry_short
            .as_ref()
            .map(|e| rule_from(&e.when, RuleSide::Short)),
        spec.exit
            .as_ref()
            .map(|e| rule_from(&e.when, RuleSide::Long)),
    ]
    .into_iter()
    .flatten()
    {
        for e in &rule.exprs {
            e.collect_timeframes(&mut referenced);
        }
    }
    let mut htfs: Vec<(String, i64)> = Vec::new();
    for tf in &spec.timeframes {
        match bt_core::time::parse_interval(tf) {
            Ok(secs) => {
                if secs <= base_secs {
                    errors.push(format!(
                        "timeframe '{tf}' must be higher than the base timeframe ({base_secs}s)"
                    ));
                } else if secs % base_secs != 0 {
                    errors.push(format!(
                        "timeframe '{tf}' ({secs}s) must be an exact multiple of the base timeframe ({base_secs}s)"
                    ));
                } else {
                    htfs.push((tf.clone(), secs));
                }
            }
            Err(e) => errors.push(format!("timeframe '{tf}': {e}")),
        }
    }
    htfs.sort_by(|a, b| a.0.cmp(&b.0));
    for tf in &referenced {
        if !declared.contains(tf) {
            errors.push(format!(
                "expression references timeframe '{tf}' which is not declared in strategy.timeframes"
            ));
        }
    }

    // Direction mapping + conflict check
    let mut entry_long: Option<Rule> = None;
    let mut entry_short: Option<Rule> = None;
    match spec.entry.direction {
        Direction::Long => entry_long = Some(rule_from(&spec.entry.when, RuleSide::Long)),
        Direction::Short => entry_short = Some(rule_from(&spec.entry.when, RuleSide::Short)),
    }
    if let Some(es) = &spec.entry_short {
        if es.direction != Direction::Short {
            errors.push("entry_short.direction must be 'short'".into());
        }
        if entry_short.is_some() {
            errors.push(
                "entry declares direction 'short' AND entry_short is present: conflicting short-side rules"
                    .into(),
            );
        }
        entry_short = Some(rule_from(&es.when, RuleSide::Short));
    }

    let exit = spec
        .exit
        .as_ref()
        .map(|e| rule_from(&e.when, RuleSide::Long));

    // Cross-symbol references must name strategy symbols.
    let mut referenced_symbols: Vec<String> = Vec::new();
    for rule in [&entry_long, &entry_short, &exit].into_iter().flatten() {
        for e in &rule.exprs {
            e.collect_symbols(&mut referenced_symbols);
        }
    }
    for sym in &referenced_symbols {
        if !spec.symbols.contains(sym) {
            errors.push(format!(
                "expression references symbol '{sym}' which is not in strategy.symbols"
            ));
        }
    }

    // Collect indicators + auto ATR indicators for ATR-based protections/sizing
    let mut ind_map: BTreeMap<String, IndicatorSpec> = BTreeMap::new();
    for rule in [&entry_long, &entry_short, &exit].into_iter().flatten() {
        for e in &rule.exprs {
            e.collect_indicators(&mut ind_map);
        }
    }
    let add_atr = |period: u32, ind_map: &mut BTreeMap<String, IndicatorSpec>| {
        let s = IndicatorSpec {
            tf: None,
            kind: IndicatorKind::Atr { period },
            source: None,
        };
        ind_map.entry(s.canonical_key()).or_insert(s);
    };
    if let crate::spec::StopLossDef::AtrMultiple { period, .. } = &spec.orders.stop_loss {
        add_atr(*period, &mut ind_map);
    }
    if let Some(crate::spec::TrailingDef::AtrMultiple { period, .. }) = &spec.orders.trailing_stop {
        add_atr(*period, &mut ind_map);
    }
    if let SizingMode::AtrBased { atr_period, .. } = &spec.risk.sizing {
        add_atr(*atr_period, &mut ind_map);
    }

    for s in ind_map.values() {
        if let Err(e) = crate::expr::validate_indicator(s) {
            errors.push(format!("indicator '{}': {e}", s.canonical_key()));
        }
    }

    // Every condition must be Bool (spec §44: ambiguity is an error)
    for (rule, name) in [
        (&entry_long, "entry"),
        (&entry_short, "entry_short"),
        (&exit, "exit"),
    ] {
        if let Some(r) = rule {
            for (i, e) in r.exprs.iter().enumerate() {
                match e.check_types(&format!("{name}[{i}]")) {
                    Ok(ExprType::Bool) => {}
                    Ok(ExprType::Num) => errors.push(format!(
                        "{name}[{i}]: expected a boolean condition; a raw numeric value is \
                         ambiguous — wrap it in a comparison (gt/lt/cross_above/...)"
                    )),
                    Err(msg) => errors.push(msg),
                }
            }
        }
    }

    // Sizing needs a stop for risk-distance-based modes
    let needs_stop = matches!(
        spec.risk.sizing,
        SizingMode::PercentRisk { .. } | SizingMode::RiskAmount { .. }
    );
    if needs_stop && matches!(spec.orders.stop_loss, crate::spec::StopLossDef::None) {
        errors.push(
            "risk.sizing uses risk-distance sizing but orders.stop_loss is 'none'; \
             sizing would be undefined"
                .into(),
        );
    }
    if matches!(
        spec.orders.take_profit,
        crate::spec::TakeProfitDef::RiskMultiple { .. }
    ) && matches!(spec.orders.stop_loss, crate::spec::StopLossDef::None)
    {
        errors.push(
            "orders.take_profit risk_multiple requires orders.stop_loss to be defined \
             (TP distance = stop distance × value)"
                .into(),
        );
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let indicators = ind_map.into_values().collect();
    let max_offset = [&entry_long, &entry_short, &exit]
        .into_iter()
        .flatten()
        .flat_map(|r| r.exprs.iter())
        .map(|e| e.max_lookback())
        .max()
        .unwrap_or(0);

    Ok(CompiledStrategy {
        name: spec.name.clone(),
        symbols: spec.symbols.clone(),
        entry_long,
        entry_short,
        exit,
        entry: spec.orders.entry.clone(),
        stop_loss: spec.orders.stop_loss.clone(),
        take_profit: spec.orders.take_profit.clone(),
        trailing_stop: spec.orders.trailing_stop.clone(),
        sizing: spec.risk.sizing.clone(),
        indicators,
        htfs,
        max_offset,
    })
}

fn rule_from(when: &crate::spec::When, side: RuleSide) -> Rule {
    Rule {
        exprs: when.exprs.clone(),
        op: when.op,
        side,
    }
}

/// Indicator values for one symbol, in either mode (differential-tested).
pub struct IndicatorRuntime {
    mode: IndicatorMode,
    specs: Vec<IndicatorSpec>,
    /// Precompute mode: full series per key + consumed position.
    series: BTreeMap<String, Vec<Option<D>>>,
    pos: BTreeMap<String, usize>,
    /// Streaming mode: live streamers + value rings.
    streams: BTreeMap<String, Streamer>,
    ring: BTreeMap<String, VecDeque<Option<D>>>,
    ring_depth: usize,
}

impl IndicatorRuntime {
    pub fn build(
        compiled: &CompiledStrategy,
        mode: IndicatorMode,
        base: &BarSeries,
        htfs: &BTreeMap<String, BarSeries>,
    ) -> CoreResult<Self> {
        let ring_depth = compiled.max_offset + 2;
        let mut rt = IndicatorRuntime {
            mode,
            specs: compiled.indicators.clone(),
            series: BTreeMap::new(),
            pos: BTreeMap::new(),
            streams: BTreeMap::new(),
            ring: BTreeMap::new(),
            ring_depth,
        };
        for spec in &compiled.indicators {
            let key = spec.canonical_key();
            match mode {
                IndicatorMode::Precompute => {
                    let bars: &[Bar] = match &spec.tf {
                        None => &base.bars,
                        Some(tf) => htfs
                            .get(tf)
                            .ok_or_else(|| {
                                bt_core::CoreError::StrategyError(format!(
                                    "indicator references unknown timeframe '{tf}'"
                                ))
                            })?
                            .bars
                            .as_slice(),
                    };
                    let series = compute_series(spec, bars);
                    rt.series.insert(key.clone(), series);
                    rt.pos.insert(key, 0);
                }
                IndicatorMode::Streaming => {
                    rt.streams.insert(key.clone(), Streamer::new(&spec.kind));
                    rt.ring.insert(key, VecDeque::with_capacity(ring_depth + 1));
                }
            }
        }
        Ok(rt)
    }

    pub fn mode(&self) -> IndicatorMode {
        self.mode
    }

    /// Feed one closing bar of the given timeframe (`None` = base).
    pub fn on_bar_close(&mut self, tf: Option<&str>, bar: &Bar) {
        let tf_name = tf.unwrap_or("base");
        for spec in &self.specs {
            let key = spec.canonical_key();
            let same_tf = spec.tf.as_deref().unwrap_or("base") == tf_name;
            if !same_tf {
                continue;
            }
            let is_atr = matches!(spec.kind, IndicatorKind::Atr { .. });
            let out = if is_atr {
                match self.streams.get_mut(&key) {
                    Some(Streamer::Atr(a)) => a.push_ohlc(bar.high, bar.low, bar.close),
                    _ => None,
                }
            } else {
                let v = source_value(
                    spec.source.unwrap_or(Source::Close),
                    bar.open,
                    bar.high,
                    bar.low,
                    bar.close,
                    bar.volume,
                );
                match self.streams.get_mut(&key) {
                    Some(s) => s.push(v),
                    None => None,
                }
            };
            if let Some(ring) = self.ring.get_mut(&key) {
                ring.push_back(out);
                while ring.len() > self.ring_depth {
                    ring.pop_front();
                }
            }
            if let Some(p) = self.pos.get_mut(&key) {
                *p += 1;
            }
        }
    }

    /// Indicator value at `offset` bars back within its own timeframe.
    pub fn value(&self, key: &str, offset: usize) -> Value {
        match self.mode {
            IndicatorMode::Precompute => {
                if let (Some(series), Some(p)) = (self.series.get(key), self.pos.get(key)) {
                    if *p > offset {
                        let idx = *p - 1 - offset;
                        return series
                            .get(idx)
                            .copied()
                            .flatten()
                            .map(Value::Num)
                            .unwrap_or(Value::NA);
                    }
                }
                Value::NA
            }
            IndicatorMode::Streaming => {
                if let Some(ring) = self.ring.get(key) {
                    if ring.len() > offset {
                        return ring[ring.len() - 1 - offset]
                            .map(Value::Num)
                            .unwrap_or(Value::NA);
                    }
                }
                Value::NA
            }
        }
    }

    pub fn keys(&self) -> Vec<String> {
        self.specs.iter().map(|s| s.canonical_key()).collect()
    }
}

/// Compute a full indicator series (precompute path).
fn compute_series(spec: &IndicatorSpec, bars: &[Bar]) -> Vec<Option<D>> {
    let mut out = Vec::with_capacity(bars.len());
    match &spec.kind {
        IndicatorKind::Atr { period } => {
            let mut s = Atr::new(*period);
            for b in bars {
                out.push(s.push_ohlc(b.high, b.low, b.close));
            }
        }
        kind => {
            let mut s = Streamer::new(kind);
            for b in bars {
                let v = source_value(
                    spec.source.unwrap_or(Source::Close),
                    b.open,
                    b.high,
                    b.low,
                    b.close,
                    b.volume,
                );
                out.push(s.push(v));
            }
        }
    }
    out
}

/// Evaluate the compiled strategy at a decision point.
///
/// Order: exit conditions first (a position can be closed), then entry rules
/// regardless of the open position — a matching entry rule against a
/// same-direction position requests a *pyramiding add* (subject to
/// `max_entries_per_position`), and against an opposite-direction position a
/// *reversal*. The risk engine decides what actually happens.
pub fn evaluate(compiled: &CompiledStrategy, ctx: &EvalContext) -> Intent {
    if ctx.position.is_some() {
        if let Some(exit) = &compiled.exit {
            if exit.matches(ctx) {
                return Intent::Exit;
            }
        }
    }
    // Priority: against an open position the OPPOSITE side is checked first
    // (reversal), so an always-true same-side rule cannot mask it. Flat: the
    // long rule first (documented, deterministic).
    let candidates: [(Option<&Rule>, Intent); 2] = match ctx.position {
        Some(p) if p.is_long => [
            (compiled.entry_short.as_ref(), Intent::EnterShort),
            (compiled.entry_long.as_ref(), Intent::EnterLong),
        ],
        Some(_) => [
            (compiled.entry_long.as_ref(), Intent::EnterLong),
            (compiled.entry_short.as_ref(), Intent::EnterShort),
        ],
        None => [
            (compiled.entry_long.as_ref(), Intent::EnterLong),
            (compiled.entry_short.as_ref(), Intent::EnterShort),
        ],
    };
    for (rule, intent) in candidates {
        if let Some(r) = rule {
            if r.matches(ctx) {
                return intent;
            }
        }
    }
    Intent::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const BASE: &str = r#"
strategy:
  name: t
  symbols: [X]
  entry:
    direction: long
    when:
      all:
        - cross_above: [{field: close}, {sma: {source: {field: close}, period: 2}}]
  orders:
    stop_loss: {type: fixed_distance, value: 5}
    take_profit: {type: risk_multiple, value: 2.0}
  risk:
    sizing: {mode: percent_risk, value: 1.0}
"#;

    fn spec() -> StrategySpec {
        crate::spec::parse_spec(BASE).unwrap()
    }

    #[test]
    fn compiles_and_collects_indicators() {
        let s = spec();
        let c = compile(&s, 3600).unwrap();
        assert_eq!(c.indicators.len(), 1);
        assert_eq!(c.indicators[0].canonical_key(), "base|sma(2)|close");
        assert!(c.entry_long.is_some());
        assert!(c.entry_short.is_none());
    }

    #[test]
    fn invalid_sizing_without_stop() {
        let mut s = spec();
        s.orders.stop_loss = crate::spec::StopLossDef::None;
        let errs = compile(&s, 3600).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("stop_loss is 'none'")));
    }

    #[test]
    fn undeclared_timeframe_reference_is_invalid() {
        // Spec §44: referencing a timeframe that is not declared must be an
        // exact, named error — never a silent NA at runtime.
        let s = "
strategy:
  name: t
  symbols: [X]
  entry:
    direction: long
    when: {gt: [{timeframe: 1D, of: {field: close}}, 1]}
";
        let s: StrategySpec = crate::spec::parse_spec(s).unwrap();
        let errs = compile(&s, 3600).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("not declared in strategy.timeframes")),
            "got: {errs:?}"
        );
    }

    #[test]
    fn invalid_timeframe() {
        let s = "
strategy:
  name: t
  symbols: [X]
  timeframes: [30m]
  entry:
    direction: long
    when: {gt: [{field: close}, 1]}
";
        let s: StrategySpec = crate::spec::parse_spec(s).unwrap();
        let errs = compile(&s, 3600).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("30m")));
    }

    #[test]
    fn htf_compiles() {
        let s = "
strategy:
  name: t
  symbols: [X]
  timeframes: [1D]
  entry:
    direction: long
    when:
      gt: [{timeframe: 1D, of: {field: close}}, {field: close}]
";
        let s: StrategySpec = crate::spec::parse_spec(s).unwrap();
        let c = compile(&s, 3600).unwrap();
        assert_eq!(c.htfs.len(), 1);
        assert_eq!(c.htfs[0].1, 86_400);
    }

    fn bar(t: &str, c: D) -> Bar {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        Bar {
            open_time: bt_core::time::parse_timestamp(t, utc, "t").unwrap(),
            open: c,
            high: c + dec!(1),
            low: c - dec!(1),
            close: c,
            volume: Some(dec!(10)),
        }
    }

    fn series(bars: Vec<Bar>) -> BarSeries {
        BarSeries {
            symbol: "X".into(),
            interval_secs: 3600,
            bars,
        }
    }

    #[test]
    fn precompute_and_streaming_agree() {
        // Differential test of the two indicator paths.
        let s = spec();
        let c = compile(&s, 3600).unwrap();
        let bars = series(vec![
            bar("2024-01-01T00:00:00Z", dec!(10)),
            bar("2024-01-01T01:00:00Z", dec!(11)),
            bar("2024-01-01T02:00:00Z", dec!(12)),
            bar("2024-01-01T03:00:00Z", dec!(11)),
            bar("2024-01-01T04:00:00Z", dec!(13)),
            bar("2024-01-01T05:00:00Z", dec!(14)),
        ]);
        let htfs = BTreeMap::new();
        let mut pre = IndicatorRuntime::build(&c, IndicatorMode::Precompute, &bars, &htfs).unwrap();
        let mut stm = IndicatorRuntime::build(&c, IndicatorMode::Streaming, &bars, &htfs).unwrap();
        for b in &bars.bars {
            pre.on_bar_close(None, b);
            stm.on_bar_close(None, b);
            for key in pre.keys() {
                assert_eq!(
                    pre.value(&key, 0),
                    stm.value(&key, 0),
                    "mismatch at {} for {key}",
                    b.open_time
                );
                assert_eq!(
                    pre.value(&key, 1),
                    stm.value(&key, 1),
                    "prev mismatch at {} for {key}",
                    b.open_time
                );
            }
        }
        let key = "base|sma(2)|close";
        assert_eq!(
            stm.value(key, 0),
            Value::Num(dec!(13.5)),
            "sma(13,14) = 13.5"
        );
        assert_eq!(stm.value(key, 1), Value::Num(dec!(12)), "sma(11,13) = 12");
    }

    #[test]
    fn entry_signal_via_cross() {
        let s = spec();
        let c = compile(&s, 3600).unwrap();
        // closes 10, 11, 9: sma(2): [NA, 10.5, 10]; cross_above(close, sma):
        // idx1: 11 > 10.5 && prev 10 <= NA(prev sma) -> false (prev undefined)
        // idx2: 9 > 10 false.
        // closes 10, 9, 12: sma: [NA, 9.5, 10.5]; idx2: 12 > 10.5 && 9 <= 9.5 => TRUE
        let bars = series(vec![
            bar("2024-01-01T00:00:00Z", dec!(10)),
            bar("2024-01-01T01:00:00Z", dec!(9)),
            bar("2024-01-01T02:00:00Z", dec!(12)),
        ]);
        let mut rt =
            IndicatorRuntime::build(&c, IndicatorMode::Streaming, &bars, &BTreeMap::new()).unwrap();
        let mut intent = Intent::None;
        for (i, b) in bars.bars.iter().enumerate() {
            rt.on_bar_close(None, b);
            let ind_fn = |k: &str, o: usize| rt.value(k, o);
            let ctx = EvalContext {
                decision_ts: b.close_time(3600),
                base_bars: &BarAccess::new(&bars.bars, i + 1),
                htf_bars: &BTreeMap::new(),
                symbol_bars: &BTreeMap::new(),
                ind: &ind_fn,
                position: None,
                account: crate::expr::AccountView {
                    equity: dec!(100000),
                    balance: dec!(100000),
                    drawdown_pct: dec!(0),
                },
                signals: &BTreeMap::new(),
            };
            intent = evaluate(&c, &ctx);
        }
        assert_eq!(intent, Intent::EnterLong);
    }
}
