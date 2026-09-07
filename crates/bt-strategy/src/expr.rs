//! Typed expression system (spec §14, §44).
//!
//! Grammar: every node is a scalar literal, `{field: X}`, or a single-key
//! operator mapping. The parser is strict: unknown operators, wrong arities,
//! non-numeric comparison operands, and indicator sources that are not plain
//! fields are **validation errors**, never silent reinterpretations.
//!
//! Semantics:
//! - evaluation produces a value per node; `NA` = undefined (indicator warmup,
//!   missing volume, division by zero);
//! - any `NA` inside a condition makes the condition FALSE (documented,
//!   conservative: undefined data never generates a signal);
//! - `cross_above/below` compare the current evaluation point with the
//!   previous one at the node's own timeframe.

use crate::indicators::{BollComponent, IndicatorKind, MacdComponent, Source};
use bt_core::time::Ts;
use bt_core::D;
use chrono::{Datelike, Timelike};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::str::FromStr;

/// One registered indicator node: timeframe + kind + source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndicatorSpec {
    /// None = base timeframe of the run.
    pub tf: Option<String>,
    pub kind: IndicatorKind,
    pub source: Option<Source>,
}

impl IndicatorSpec {
    pub fn canonical_key(&self) -> String {
        let tf = self.tf.as_deref().unwrap_or("base");
        let src = self.source.map(|s| s.as_str()).unwrap_or("ohlc");
        format!("{tf}|{}|{src}", self.kind.key())
    }
}

/// Boolean combinator for condition lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoolOp {
    All,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionDir {
    Long,
    Short,
    Flat,
}

/// A compiled expression node.
#[derive(Debug, Clone)]
pub enum Expr {
    Lit(D),
    Field {
        tf: Option<String>,
        source: Source,
    },
    /// Cross-symbol access: `{"symbol": "Y", "of": {field: close}}`.
    /// Inner must be a plain field (validated); observable bars only.
    SymbolWrap {
        symbol: String,
        inner: Box<Expr>,
    },
    Ind(Box<IndicatorSpec>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Gte(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Lte(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Abs(Box<Expr>),
    CrossAbove(Box<Expr>, Box<Expr>),
    CrossBelow(Box<Expr>, Box<Expr>),
    Between {
        value: Box<Expr>,
        min: Box<Expr>,
        max: Box<Expr>,
    },
    /// (of / value@bars - 1) * 100 — base timeframe, field/indicator sources only.
    PctChange {
        of: Box<Expr>,
        bars: u32,
    },
    TimeOfDay {
        from_min: u16,
        to_min: u16,
        tz: chrono_tz::Tz,
    },
    DayOfWeek {
        days: Vec<chrono::Weekday>,
        tz: chrono_tz::Tz,
    },
    PositionDirection(PositionDir),
    BarsInPosition,
    UnrealizedPnl,
    UnrealizedPnlPct,
    Equity,
    Balance,
    DrawdownPct,
    Signal {
        key: String,
    },
}

/// Value of an expression at one evaluation point.
#[derive(Debug, Clone, PartialEq, Copy)]
pub enum Value {
    Num(D),
    Bool(bool),
    NA,
}

impl Value {
    pub fn as_bool(self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_num(self) -> Option<D> {
        match self {
            Value::Num(n) => Some(n),
            _ => None,
        }
    }
}

/// Read-only market/account state available to expressions.
pub struct EvalContext<'a> {
    pub decision_ts: Ts,
    pub base_bars: &'a crate::runtime::BarAccess<'a>,
    /// HTF series by name.
    pub htf_bars: &'a BTreeMap<String, crate::runtime::BarAccess<'a>>,
    /// Per-symbol bar access for cross-symbol references (all strategy symbols).
    pub symbol_bars: &'a BTreeMap<String, crate::runtime::BarAccess<'a>>,
    /// Indicator value lookup: (key, offset) -> Value. offset 0 = current.
    pub ind: &'a dyn Fn(&str, usize) -> Value,
    pub position: Option<PositionView>,
    pub account: AccountView,
    pub signals: &'a BTreeMap<String, Vec<(Ts, D)>>,
}

#[derive(Debug, Clone, Copy)]
pub struct PositionView {
    pub is_long: bool,
    pub bars_held: u64,
    pub unrealized: D,
    pub unrealized_pct: D,
}

#[derive(Debug, Clone, Copy)]
pub struct AccountView {
    pub equity: D,
    pub balance: D,
    pub drawdown_pct: D,
}

impl Expr {
    /// Evaluate at `offset` base bars in the past (0 = current).
    pub fn eval(&self, ctx: &EvalContext, offset: usize) -> Value {
        match self {
            Expr::Lit(d) => Value::Num(*d),
            Expr::SymbolWrap { symbol, inner } => {
                // Resolve against the per-symbol bar access map; the engine
                // builds cursors so only bars observable at decision_ts count.
                match ctx.symbol_bars.get(symbol) {
                    Some(acc) => match inner.as_ref() {
                        Expr::Field { source, .. } => acc.field(*source, offset),
                        _ => Value::NA,
                    },
                    None => Value::NA,
                }
            }
            Expr::Field { tf: None, source } => ctx.base_bars.field(*source, offset),
            Expr::Field {
                tf: Some(tf),
                source,
            } => match ctx.htf_bars.get(tf) {
                Some(acc) => acc.field(*source, offset),
                None => Value::NA,
            },
            Expr::Ind(spec) => (ctx.ind)(&spec.canonical_key(), offset),
            Expr::And(list) => {
                for e in list {
                    match e.eval(ctx, offset) {
                        Value::Bool(false) => return Value::Bool(false),
                        Value::Bool(true) => {}
                        _ => return Value::Bool(false), // NA => false (documented)
                    }
                }
                Value::Bool(true)
            }
            Expr::Or(list) => {
                // NA operands are treated as false (documented convention).
                for e in list {
                    if let Value::Bool(true) = e.eval(ctx, offset) {
                        return Value::Bool(true);
                    }
                }
                Value::Bool(false)
            }
            Expr::Not(e) => match e.eval(ctx, offset) {
                Value::Bool(b) => Value::Bool(!b),
                _ => Value::Bool(false),
            },
            Expr::Gt(a, b) => num_cmp(ctx, offset, a, b, |x, y| x > y),
            Expr::Gte(a, b) => num_cmp(ctx, offset, a, b, |x, y| x >= y),
            Expr::Lt(a, b) => num_cmp(ctx, offset, a, b, |x, y| x < y),
            Expr::Lte(a, b) => num_cmp(ctx, offset, a, b, |x, y| x <= y),
            Expr::Eq(a, b) => num_cmp(ctx, offset, a, b, |x, y| x == y),
            Expr::Ne(a, b) => num_cmp(ctx, offset, a, b, |x, y| x != y),
            Expr::Add(a, b) => num_op(ctx, offset, a, b, |x, y| Value::Num(x + y)),
            Expr::Sub(a, b) => num_op(ctx, offset, a, b, |x, y| Value::Num(x - y)),
            Expr::Mul(a, b) => num_op(ctx, offset, a, b, |x, y| Value::Num(x * y)),
            Expr::Div(a, b) => num_op(ctx, offset, a, b, |x, y| {
                if y.is_zero() {
                    return Value::NA;
                }
                Value::Num(x / y)
            }),
            Expr::Neg(a) => match a.eval(ctx, offset).as_num() {
                Some(x) => Value::Num(-x),
                None => Value::NA,
            },
            Expr::Abs(a) => match a.eval(ctx, offset).as_num() {
                Some(x) => Value::Num(x.abs()),
                None => Value::NA,
            },
            Expr::CrossAbove(a, b) => {
                let ac = a.eval(ctx, offset).as_num();
                let bc = b.eval(ctx, offset).as_num();
                let ap = a.eval(ctx, offset + 1).as_num();
                let bp = b.eval(ctx, offset + 1).as_num();
                match (ac, bc, ap, bp) {
                    (Some(ac), Some(bc), Some(ap), Some(bp)) => Value::Bool(ac > bc && ap <= bp),
                    _ => Value::Bool(false),
                }
            }
            Expr::CrossBelow(a, b) => {
                let ac = a.eval(ctx, offset).as_num();
                let bc = b.eval(ctx, offset).as_num();
                let ap = a.eval(ctx, offset + 1).as_num();
                let bp = b.eval(ctx, offset + 1).as_num();
                match (ac, bc, ap, bp) {
                    (Some(ac), Some(bc), Some(ap), Some(bp)) => Value::Bool(ac < bc && ap >= bp),
                    _ => Value::Bool(false),
                }
            }
            Expr::Between { value, min, max } => {
                let v = value.eval(ctx, offset).as_num();
                let lo = min.eval(ctx, offset).as_num();
                let hi = max.eval(ctx, offset).as_num();
                match (v, lo, hi) {
                    (Some(v), Some(lo), Some(hi)) => Value::Bool(v >= lo && v <= hi),
                    _ => Value::Bool(false),
                }
            }
            Expr::PctChange { of, bars } => {
                if *bars == 0 {
                    return Value::NA;
                }
                let cur = of.eval(ctx, offset).as_num();
                let past = of.eval(ctx, offset + *bars as usize).as_num();
                match (cur, past) {
                    (Some(c), Some(p)) if !p.is_zero() => Value::Num((c / p - dec!(1)) * dec!(100)),
                    _ => Value::NA,
                }
            }
            Expr::TimeOfDay {
                from_min,
                to_min,
                tz,
            } => {
                let local = ctx.decision_ts.with_timezone(tz);
                let mins = local.hour() as u16 * 60 + local.minute() as u16;
                Value::Bool(if from_min <= to_min {
                    mins >= *from_min && mins < *to_min
                } else {
                    // Overnight window (e.g. 22:00 -> 06:00).
                    mins >= *from_min || mins < *to_min
                })
            }
            Expr::DayOfWeek { days, tz } => {
                let local = ctx.decision_ts.with_timezone(tz);
                Value::Bool(days.contains(&local.weekday()))
            }
            Expr::PositionDirection(dir) => {
                let is_flat = ctx.position.is_none();
                Value::Bool(match dir {
                    PositionDir::Flat => is_flat,
                    PositionDir::Long => match &ctx.position {
                        Some(p) => p.is_long,
                        None => false,
                    },
                    PositionDir::Short => match &ctx.position {
                        Some(p) => !p.is_long,
                        None => false,
                    },
                })
            }
            Expr::BarsInPosition => match &ctx.position {
                Some(p) => Value::Num(D::from(p.bars_held)),
                None => Value::NA,
            },
            Expr::UnrealizedPnl => match &ctx.position {
                Some(p) => Value::Num(p.unrealized),
                None => Value::NA,
            },
            Expr::UnrealizedPnlPct => match &ctx.position {
                Some(p) => Value::Num(p.unrealized_pct),
                None => Value::NA,
            },
            Expr::Equity => Value::Num(ctx.account.equity),
            Expr::Balance => Value::Num(ctx.account.balance),
            Expr::DrawdownPct => Value::Num(ctx.account.drawdown_pct),
            Expr::Signal { key } => {
                match ctx.signals.get(key) {
                    Some(series) if !series.is_empty() => {
                        // last observation with ts <= decision_ts (binary search;
                        // availability is enforced by construction)
                        let idx = series.partition_point(|(ts, _)| *ts <= ctx.decision_ts);
                        if idx == 0 {
                            Value::NA
                        } else {
                            Value::Num(series[idx - 1].1)
                        }
                    }
                    _ => Value::NA,
                }
            }
        }
    }

    /// Static type: Num or Bool. Used by compile-time validation (spec §44).
    pub fn check_types(&self, path: &str) -> Result<ExprType, String> {
        use ExprType::*;
        let num_pair = |a: &Expr, b: &Expr, op: &str| -> Result<ExprType, String> {
            a.check_types(&format!("{path}.{op}[0]"))?
                .must_be_num(&format!("{path}.{op}[0]"))?;
            b.check_types(&format!("{path}.{op}[1]"))?
                .must_be_num(&format!("{path}.{op}[1]"))?;
            Ok(Num)
        };
        match self {
            Expr::Lit(_) | Expr::Field { .. } | Expr::Ind(_) => Ok(Num),
            Expr::SymbolWrap { inner, .. } => {
                let t = inner.check_types(&format!("{path}.symbol"))?;
                t.must_be_num(&format!("{path}.symbol"))?;
                Ok(Num)
            }
            Expr::And(list) | Expr::Or(list) => {
                for (i, e) in list.iter().enumerate() {
                    let t = e.check_types(&format!("{path}[{i}]"))?;
                    t.must_be_bool(&format!("{path}[{i}]"))?;
                }
                Ok(Bool)
            }
            Expr::Not(e) => {
                e.check_types(path)?.must_be_bool(path)?;
                Ok(Bool)
            }
            Expr::Gt(a, b)
            | Expr::Gte(a, b)
            | Expr::Lt(a, b)
            | Expr::Lte(a, b)
            | Expr::Eq(a, b)
            | Expr::Ne(a, b) => {
                num_pair(a, b, "cmp")?;
                Ok(Bool)
            }
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                num_pair(a, b, "arith")
            }
            Expr::Neg(a) | Expr::Abs(a) => {
                a.check_types(path)?.must_be_num(path)?;
                Ok(Num)
            }
            Expr::CrossAbove(a, b) | Expr::CrossBelow(a, b) => {
                num_pair(a, b, "cross")?;
                Ok(Bool)
            }
            Expr::Between { value, min, max } => {
                value.check_types(path)?.must_be_num(path)?;
                min.check_types(path)?.must_be_num(path)?;
                max.check_types(path)?.must_be_num(path)?;
                Ok(Bool)
            }
            Expr::PctChange { of, .. } => {
                // Restriction: base-timeframe field/indicator sources only.
                match &**of {
                    Expr::Field { tf: None, .. } | Expr::Ind(_) => {}
                    other => {
                        return Err(format!(
                            "{path}.pct_change: source must be a base-timeframe field or \
                             indicator, got {other:?}"
                        ))
                    }
                }
                of.check_types(path)?.must_be_num(path)?;
                Ok(Num)
            }
            Expr::TimeOfDay { .. } | Expr::DayOfWeek { .. } | Expr::PositionDirection(_) => {
                Ok(Bool)
            }
            Expr::BarsInPosition | Expr::UnrealizedPnl | Expr::UnrealizedPnlPct => Ok(Num),
            Expr::Equity | Expr::Balance | Expr::DrawdownPct => Ok(Num),
            Expr::Signal { .. } => Ok(Num),
        }
    }

    /// Collect every explicitly referenced timeframe name (Field/Ind nodes).
    pub fn collect_timeframes(&self, out: &mut Vec<String>) {
        match self {
            Expr::Field { tf: Some(tf), .. } => {
                if !out.contains(tf) {
                    out.push(tf.clone());
                }
            }
            Expr::Ind(spec) => {
                if let Some(tf) = &spec.tf {
                    if !out.contains(tf) {
                        out.push(tf.clone());
                    }
                }
            }
            Expr::And(l) | Expr::Or(l) => l.iter().for_each(|e| e.collect_timeframes(out)),
            Expr::Not(a) | Expr::Neg(a) | Expr::Abs(a) => a.collect_timeframes(out),
            Expr::SymbolWrap { inner, .. } => inner.collect_timeframes(out),
            Expr::Between { value, min, max } => {
                value.collect_timeframes(out);
                min.collect_timeframes(out);
                max.collect_timeframes(out);
            }
            Expr::PctChange { of, .. } => of.collect_timeframes(out),
            other => {
                if let Some((a, b)) = binary_operands(other) {
                    a.collect_timeframes(out);
                    b.collect_timeframes(out);
                }
            }
        }
    }

    /// Deepest past-lookback requested anywhere in the tree (pct_change bars);
    /// used to size streaming value rings.
    pub fn max_lookback(&self) -> usize {
        let mut d = 0usize;
        fn scan(e: &Expr, d: &mut usize) {
            match e {
                Expr::PctChange { of, bars } => {
                    *d = (*d).max(*bars as usize);
                    scan(of, d);
                }
                Expr::And(l) | Expr::Or(l) => l.iter().for_each(|x| scan(x, d)),
                Expr::Not(a) | Expr::Neg(a) | Expr::Abs(a) => scan(a, d),
                Expr::SymbolWrap { inner, .. } => scan(inner, d),
                Expr::Between { value, min, max } => {
                    scan(value, d);
                    scan(min, d);
                    scan(max, d);
                }
                other => {
                    // Binary nodes
                    if let Some((a, b)) = binary_operands(other) {
                        scan(a, d);
                        scan(b, d);
                    }
                }
            }
        }
        scan(self, &mut d);
        d
    }

    /// Collect every cross-symbol reference made via SymbolWrap.
    pub fn collect_symbols(&self, out: &mut Vec<String>) {
        match self {
            Expr::SymbolWrap { symbol, inner } => {
                if !out.contains(symbol) {
                    out.push(symbol.clone());
                }
                inner.collect_symbols(out);
            }
            Expr::And(l) | Expr::Or(l) => l.iter().for_each(|e| e.collect_symbols(out)),
            Expr::Not(a) | Expr::Neg(a) | Expr::Abs(a) => a.collect_symbols(out),
            Expr::Between { value, min, max } => {
                value.collect_symbols(out);
                min.collect_symbols(out);
                max.collect_symbols(out);
            }
            Expr::PctChange { of, .. } => of.collect_symbols(out),
            other => {
                if let Some((a, b)) = binary_operands(other) {
                    a.collect_symbols(out);
                    b.collect_symbols(out);
                }
            }
        }
    }

    /// Collect (dedup) indicator specs from the tree in deterministic order.
    pub fn collect_indicators(&self, out: &mut BTreeMap<String, IndicatorSpec>) {
        match self {
            Expr::Ind(spec) => {
                out.insert(spec.canonical_key(), (**spec).clone());
            }
            Expr::And(l) | Expr::Or(l) => l.iter().for_each(|e| e.collect_indicators(out)),
            Expr::Not(a) | Expr::Neg(a) | Expr::Abs(a) => a.collect_indicators(out),
            Expr::SymbolWrap { inner, .. } => inner.collect_indicators(out),
            Expr::Gt(a, b)
            | Expr::Gte(a, b)
            | Expr::Lt(a, b)
            | Expr::Lte(a, b)
            | Expr::Eq(a, b)
            | Expr::Ne(a, b)
            | Expr::Add(a, b)
            | Expr::Sub(a, b)
            | Expr::Mul(a, b)
            | Expr::Div(a, b)
            | Expr::CrossAbove(a, b)
            | Expr::CrossBelow(a, b) => {
                a.collect_indicators(out);
                b.collect_indicators(out);
            }
            Expr::Between { value, min, max } => {
                value.collect_indicators(out);
                min.collect_indicators(out);
                max.collect_indicators(out);
            }
            Expr::PctChange { of, .. } => of.collect_indicators(out),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprType {
    Num,
    Bool,
}

impl ExprType {
    pub fn must_be_num(self, path: &str) -> Result<(), String> {
        match self {
            ExprType::Num => Ok(()),
            ExprType::Bool => Err(format!(
                "{path}: expected a numeric expression, found boolean"
            )),
        }
    }
    pub fn must_be_bool(self, path: &str) -> Result<(), String> {
        match self {
            ExprType::Bool => Ok(()),
            ExprType::Num => Err(format!(
                "{path}: expected a boolean condition; an indicator/field value alone is \
                 ambiguous — wrap it in a comparison (e.g. gt/lt/cross_above)"
            )),
        }
    }
}

fn num_cmp(
    ctx: &EvalContext,
    offset: usize,
    a: &Expr,
    b: &Expr,
    f: impl Fn(D, D) -> bool,
) -> Value {
    match (a.eval(ctx, offset).as_num(), b.eval(ctx, offset).as_num()) {
        (Some(x), Some(y)) => Value::Bool(f(x, y)),
        _ => Value::Bool(false),
    }
}

/// Return (left, right) for the binary operator nodes.
fn binary_operands(e: &Expr) -> Option<(&Expr, &Expr)> {
    match e {
        Expr::Gt(a, b)
        | Expr::Gte(a, b)
        | Expr::Lt(a, b)
        | Expr::Lte(a, b)
        | Expr::Eq(a, b)
        | Expr::Ne(a, b)
        | Expr::Add(a, b)
        | Expr::Sub(a, b)
        | Expr::Mul(a, b)
        | Expr::Div(a, b)
        | Expr::CrossAbove(a, b)
        | Expr::CrossBelow(a, b) => Some((a.as_ref(), b.as_ref())),
        _ => None,
    }
}

fn num_op(
    ctx: &EvalContext,
    offset: usize,
    a: &Expr,
    b: &Expr,
    f: impl Fn(D, D) -> Value,
) -> Value {
    match (a.eval(ctx, offset).as_num(), b.eval(ctx, offset).as_num()) {
        (Some(x), Some(y)) => f(x, y),
        _ => Value::NA,
    }
}

// ---------------------------------------------------------------------------
// YAML parsing (strict; spec §44)
// ---------------------------------------------------------------------------

pub fn yaml_num(v: &serde_yaml::Value, path: &str) -> Result<D, String> {
    match v {
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(D::from(i))
            } else if let Some(u) = n.as_u64() {
                D::from_str_exact(&u.to_string())
                    .map_err(|_| format!("{path}: integer out of decimal range"))
            } else if let Some(f) = n.as_f64() {
                bt_core::money::d_from_f64(f)
                    .map_err(|_| format!("{path}: cannot represent {f} as decimal"))
            } else {
                Err(format!("{path}: unsupported number"))
            }
        }
        serde_yaml::Value::String(s) => D::from_str_exact(s.trim())
            .or_else(|_| D::from_scientific(s.trim()))
            .map_err(|_| format!("{path}: '{s}' is not a decimal")),
        _ => Err(format!("{path}: expected a number")),
    }
}

fn yaml_u32(v: &serde_yaml::Value, path: &str) -> Result<u32, String> {
    let d = yaml_num(v, path)?;
    let f = bt_core::money::to_f64(d);
    if f < 0.0 || f.fract() != 0.0 {
        return Err(format!("{path}: expected a non-negative integer"));
    }
    Ok(f as u32)
}

fn get_key<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    map.get(serde_yaml::Value::String(key.to_string()))
}

fn expect_mapping<'a>(
    v: &'a serde_yaml::Value,
    path: &str,
) -> Result<&'a serde_yaml::Mapping, String> {
    v.as_mapping()
        .ok_or_else(|| format!("{path}: expected a mapping"))
}

/// Parse an expression. `tf_ctx` is the timeframe context from an enclosing
/// `{timeframe: ..., of: ...}` wrapper.
pub fn parse_expr(v: &serde_yaml::Value, tf_ctx: Option<&str>, path: &str) -> Result<Expr, String> {
    match v {
        serde_yaml::Value::Number(_) | serde_yaml::Value::String(_) => {
            Ok(Expr::Lit(yaml_num(v, path)?))
        }
        serde_yaml::Value::Mapping(map) => {
            // `{symbol: "Y", of: <expr>}` cross-symbol wrapper.
            if map.len() == 2 && get_key(map, "symbol").is_some() && get_key(map, "of").is_some() {
                let symbol = get_key(map, "symbol")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| format!("{path}: expected 'symbol' string"))?
                    .to_string();
                let inner = get_key(map, "of").unwrap();
                let inner_expr = parse_expr(inner, None, &format!("{path}.of"))?;
                // Restrict inner to a plain field (documented v1 scope).
                if !matches!(inner_expr, Expr::Field { .. }) {
                    return Err(format!(
                        "{path}.of: cross-symbol references support plain price fields only"
                    ));
                }
                return Ok(Expr::SymbolWrap {
                    symbol,
                    inner: Box::new(inner_expr),
                });
            }
            // `{timeframe: "1D", of: <expr>}` wrapper (2 keys by definition).
            if map.len() == 2 && get_key(map, "timeframe").is_some() && get_key(map, "of").is_some()
            {
                let tf = get_key(map, "timeframe")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| format!("{path}: expected 'timeframe' string"))?
                    .to_string();
                let inner = get_key(map, "of").unwrap();
                return parse_expr(inner, Some(&tf), path);
            }
            if map.len() != 1 {
                return Err(format!(
                    "{path}: an expression mapping must have exactly one operator key, got {} keys",
                    map.len()
                ));
            }
            let (k, val) = map.iter().next().unwrap();
            let op = k
                .as_str()
                .ok_or_else(|| format!("{path}: operator key must be a string"))?;
            let sub = format!("{path}.{op}");
            match op {
                "field" => parse_field(val, tf_ctx, &sub),
                "timeframe" => {
                    let m = expect_mapping(val, &sub)?;
                    let tf = get_key(m, "timeframe")
                        .and_then(|t| t.as_str())
                        .ok_or_else(|| format!("{sub}: expected 'timeframe' string"))?
                        .to_string();
                    let inner = get_key(m, "of")
                        .ok_or_else(|| format!("{sub}: expected 'of' expression"))?;
                    parse_expr(inner, Some(&tf), &sub)
                }
                "and" => Ok(Expr::And(parse_list(val, tf_ctx, &sub)?)),
                "or" => Ok(Expr::Or(parse_list(val, tf_ctx, &sub)?)),
                "not" => Ok(Expr::Not(Box::new(parse_expr(val, tf_ctx, &sub)?))),
                "gt" | "gte" | "lt" | "lte" | "eq" | "ne" => {
                    let [a, b] = parse_pair(val, tf_ctx, &sub)?;
                    Ok(match op {
                        "gt" => Expr::Gt(Box::new(a), Box::new(b)),
                        "gte" => Expr::Gte(Box::new(a), Box::new(b)),
                        "lt" => Expr::Lt(Box::new(a), Box::new(b)),
                        "lte" => Expr::Lte(Box::new(a), Box::new(b)),
                        "eq" => Expr::Eq(Box::new(a), Box::new(b)),
                        _ => Expr::Ne(Box::new(a), Box::new(b)),
                    })
                }
                "add" | "sub" | "mul" | "div" => {
                    let [a, b] = parse_pair(val, tf_ctx, &sub)?;
                    Ok(match op {
                        "add" => Expr::Add(Box::new(a), Box::new(b)),
                        "sub" => Expr::Sub(Box::new(a), Box::new(b)),
                        "mul" => Expr::Mul(Box::new(a), Box::new(b)),
                        _ => Expr::Div(Box::new(a), Box::new(b)),
                    })
                }
                "neg" => Ok(Expr::Neg(Box::new(parse_expr(val, tf_ctx, &sub)?))),
                "abs" => Ok(Expr::Abs(Box::new(parse_expr(val, tf_ctx, &sub)?))),
                "cross_above" => {
                    let [a, b] = parse_pair(val, tf_ctx, &sub)?;
                    Ok(Expr::CrossAbove(Box::new(a), Box::new(b)))
                }
                "cross_below" => {
                    let [a, b] = parse_pair(val, tf_ctx, &sub)?;
                    Ok(Expr::CrossBelow(Box::new(a), Box::new(b)))
                }
                "between" => {
                    let m = expect_mapping(val, &sub)?;
                    let value = parse_expr(
                        get_key(m, "value").ok_or_else(|| format!("{sub}: missing 'value'"))?,
                        tf_ctx,
                        &format!("{sub}.value"),
                    )?;
                    let min = parse_expr(
                        get_key(m, "min").ok_or_else(|| format!("{sub}: missing 'min'"))?,
                        tf_ctx,
                        &format!("{sub}.min"),
                    )?;
                    let max = parse_expr(
                        get_key(m, "max").ok_or_else(|| format!("{sub}: missing 'max'"))?,
                        tf_ctx,
                        &format!("{sub}.max"),
                    )?;
                    Ok(Expr::Between {
                        value: Box::new(value),
                        min: Box::new(min),
                        max: Box::new(max),
                    })
                }
                "pct_change" => {
                    let m = expect_mapping(val, &sub)?;
                    let of = parse_expr(
                        get_key(m, "of").ok_or_else(|| format!("{sub}: missing 'of'"))?,
                        tf_ctx,
                        &format!("{sub}.of"),
                    )?;
                    let bars = yaml_u32(
                        get_key(m, "bars").ok_or_else(|| format!("{sub}: missing 'bars'"))?,
                        &format!("{sub}.bars"),
                    )?;
                    Ok(Expr::PctChange { of: Box::new(of), bars })
                }
                "time_of_day" => {
                    let m = expect_mapping(val, &sub)?;
                    let from = parse_hhmm(
                        get_key(m, "from").and_then(|t| t.as_str())
                            .ok_or_else(|| format!("{sub}: missing 'from' (HH:MM)"))?,
                        &format!("{sub}.from"),
                    )?;
                    let to = parse_hhmm(
                        get_key(m, "to").and_then(|t| t.as_str())
                            .ok_or_else(|| format!("{sub}: missing 'to' (HH:MM)"))?,
                        &format!("{sub}.to"),
                    )?;
                    let tz = parse_tz(
                        get_key(m, "tz").and_then(|t| t.as_str()).unwrap_or("UTC"),
                        &format!("{sub}.tz"),
                    )?;
                    Ok(Expr::TimeOfDay { from_min: from, to_min: to, tz })
                }
                "day_of_week" => {
                    let (days, tz) = parse_days(val, &sub)?;
                    Ok(Expr::DayOfWeek { days, tz })
                }
                "position_direction" => {
                    let s = val
                        .as_str()
                        .ok_or_else(|| format!("{sub}: expected long|short|flat"))?;
                    match s {
                        "long" => Ok(Expr::PositionDirection(PositionDir::Long)),
                        "short" => Ok(Expr::PositionDirection(PositionDir::Short)),
                        "flat" => Ok(Expr::PositionDirection(PositionDir::Flat)),
                        _ => Err(format!("{sub}: expected long|short|flat, got '{s}'")),
                    }
                }
                "bars_in_position" => Ok(Expr::BarsInPosition),
                "unrealized_pnl" => Ok(Expr::UnrealizedPnl),
                "unrealized_pnl_pct" => Ok(Expr::UnrealizedPnlPct),
                "equity" => Ok(Expr::Equity),
                "balance" => Ok(Expr::Balance),
                "drawdown_pct" => Ok(Expr::DrawdownPct),
                "signal" => {
                    let m = expect_mapping(val, &sub)?;
                    let key = get_key(m, "key")
                        .and_then(|k| k.as_str())
                        .ok_or_else(|| format!("{sub}: missing 'key'"))?
                        .to_string();
                    Ok(Expr::Signal { key })
                }
                "sma" | "ema" | "wma" | "rsi" | "stddev" | "roc" | "highest" | "lowest" => {
                    let m = expect_mapping(val, &sub)?;
                    let period = yaml_u32(
                        get_key(m, "period").ok_or_else(|| format!("{sub}: missing 'period'"))?,
                        &format!("{sub}.period"),
                    )?;
                    let (source, src_tf) = parse_indicator_source(m, tf_ctx, &sub)?;
                    let kind = match op {
                        "sma" => IndicatorKind::Sma { period },
                        "ema" => IndicatorKind::Ema { period },
                        "wma" => IndicatorKind::Wma { period },
                        "rsi" => IndicatorKind::Rsi { period },
                        "stddev" => IndicatorKind::StdDev { period },
                        "roc" => IndicatorKind::Roc { period },
                        "highest" | "rolling_high" => IndicatorKind::Highest { period },
                        _ => IndicatorKind::Lowest { period },
                    };
                    Ok(Expr::Ind(Box::new(IndicatorSpec { tf: src_tf, kind, source: Some(source) })))
                }
                "rolling_high" | "rolling_low" => {
                    // Alias of highest/lowest (documented).
                    let m = expect_mapping(val, &sub)?;
                    let period = yaml_u32(
                        get_key(m, "period").ok_or_else(|| format!("{sub}: missing 'period'"))?,
                        &format!("{sub}.period"),
                    )?;
                    let (source, src_tf) = parse_indicator_source(m, tf_ctx, &sub)?;
                    let kind = if op == "rolling_high" {
                        IndicatorKind::Highest { period }
                    } else {
                        IndicatorKind::Lowest { period }
                    };
                    Ok(Expr::Ind(Box::new(IndicatorSpec { tf: src_tf, kind, source: Some(source) })))
                }
                "atr" => {
                    let m = expect_mapping(val, &sub)?;
                    let period = yaml_u32(
                        get_key(m, "period").ok_or_else(|| format!("{sub}: missing 'period'"))?,
                        &format!("{sub}.period"),
                    )?;
                    Ok(Expr::Ind(Box::new(IndicatorSpec {
                        tf: tf_ctx.map(|s| s.to_string()),
                        kind: IndicatorKind::Atr { period },
                        source: None,
                    })))
                }
                "macd" => {
                    let m = expect_mapping(val, &sub)?;
                    let fast = yaml_u32(get_key(m, "fast").ok_or_else(|| format!("{sub}: missing 'fast'"))?, &format!("{sub}.fast"))?;
                    let slow = yaml_u32(get_key(m, "slow").ok_or_else(|| format!("{sub}: missing 'slow'"))?, &format!("{sub}.slow"))?;
                    let signal = yaml_u32(get_key(m, "signal").ok_or_else(|| format!("{sub}: missing 'signal'"))?, &format!("{sub}.signal"))?;
                    let component = get_key(m, "component")
                        .and_then(|c| c.as_str())
                        .unwrap_or("macd");
                    let component = match component {
                        "macd" => MacdComponent::Macd,
                        "signal" => MacdComponent::Signal,
                        "hist" | "histogram" => MacdComponent::Hist,
                        other => return Err(format!("{sub}.component: unknown '{other}'")),
                    };
                    let (source, src_tf) = parse_indicator_source(m, tf_ctx, &sub)?;
                    Ok(Expr::Ind(Box::new(IndicatorSpec {
                        tf: src_tf,
                        kind: IndicatorKind::Macd { fast, slow, signal, component },
                        source: Some(source),
                    })))
                }
                "bollinger" => {
                    let m = expect_mapping(val, &sub)?;
                    let period = yaml_u32(get_key(m, "period").ok_or_else(|| format!("{sub}: missing 'period'"))?, &format!("{sub}.period"))?;
                    let k = yaml_num(get_key(m, "k").unwrap_or(&serde_yaml::Value::Number(2.into())), &format!("{sub}.k"))?;
                    let component = get_key(m, "component")
                        .and_then(|c| c.as_str())
                        .unwrap_or("upper");
                    let component = match component {
                        "upper" => BollComponent::Upper,
                        "middle" => BollComponent::Middle,
                        "lower" => BollComponent::Lower,
                        other => return Err(format!("{sub}.component: unknown '{other}'")),
                    };
                    let (source, src_tf) = parse_indicator_source(m, tf_ctx, &sub)?;
                    Ok(Expr::Ind(Box::new(IndicatorSpec {
                        tf: src_tf,
                        kind: IndicatorKind::Bollinger { period, k, component },
                        source: Some(source),
                    })))
                }
                other => Err(format!(
                    "{path}: unknown operator '{other}' (see STRATEGY_SPEC.md for the operator list)"
                )),
            }
        }
        other => Err(format!(
            "{path}: expected an expression (literal, {{field: ..}} or operator mapping), got {other:?}"
        )),
    }
}

fn parse_field(v: &serde_yaml::Value, tf_ctx: Option<&str>, path: &str) -> Result<Expr, String> {
    // {field: close} | {field: {timeframe: "1D", of: close}}
    match v {
        serde_yaml::Value::String(s) => {
            let source = Source::parse(s).ok_or_else(|| {
                format!("{path}: unknown field '{s}' (open|high|low|close|volume|hl2|hlc3|ohlc4)")
            })?;
            Ok(Expr::Field {
                tf: tf_ctx.map(|t| t.to_string()),
                source,
            })
        }
        serde_yaml::Value::Mapping(m) => {
            if m.len() == 2 {
                let tf = get_key(m, "timeframe")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| format!("{path}: expected 'timeframe' string"))?
                    .to_string();
                let of = get_key(m, "of").ok_or_else(|| format!("{path}: expected 'of'"))?;
                let inner = parse_field(of, None, &format!("{path}.of"))?;
                match inner {
                    Expr::Field { source, .. } => Ok(Expr::Field {
                        tf: Some(tf),
                        source,
                    }),
                    _ => unreachable!("parse_field returns Field only"),
                }
            } else {
                Err(format!("{path}: field mapping must be {{timeframe, of}}"))
            }
        }
        _ => Err(format!("{path}: expected a field name string")),
    }
}

/// Indicators consume a plain field series (v1 restriction, documented).
fn parse_indicator_source(
    m: &serde_yaml::Mapping,
    tf_ctx: Option<&str>,
    path: &str,
) -> Result<(Source, Option<String>), String> {
    let src_val = get_key(m, "source").ok_or_else(|| format!("{path}: missing 'source'"))?;
    match parse_expr(src_val, tf_ctx, &format!("{path}.source"))? {
        Expr::Field { tf, source } => Ok((source, tf)),
        _ => Err(format!(
            "{path}.source: indicator sources must be plain price fields \
             (open|high|low|close|volume|hl2|hlc3|ohlc4); nested indicators are not supported"
        )),
    }
}

fn parse_list(
    v: &serde_yaml::Value,
    tf_ctx: Option<&str>,
    path: &str,
) -> Result<Vec<Expr>, String> {
    let seq = v
        .as_sequence()
        .ok_or_else(|| format!("{path}: expected a list of expressions"))?;
    seq.iter()
        .enumerate()
        .map(|(i, e)| parse_expr(e, tf_ctx, &format!("{path}[{i}]")))
        .collect()
}

fn parse_pair(
    v: &serde_yaml::Value,
    tf_ctx: Option<&str>,
    path: &str,
) -> Result<[Expr; 2], String> {
    let seq = v
        .as_sequence()
        .ok_or_else(|| format!("{path}: expected [a, b]"))?;
    if seq.len() != 2 {
        return Err(format!(
            "{path}: expected exactly 2 operands, got {}",
            seq.len()
        ));
    }
    Ok([
        parse_expr(&seq[0], tf_ctx, &format!("{path}[0]"))?,
        parse_expr(&seq[1], tf_ctx, &format!("{path}[1]"))?,
    ])
}

fn parse_hhmm(s: &str, path: &str) -> Result<u16, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(format!("{path}: expected HH:MM, got '{s}'"));
    }
    let h: u16 = parts[0]
        .parse()
        .map_err(|_| format!("{path}: bad hour in '{s}'"))?;
    let m: u16 = parts[1]
        .parse()
        .map_err(|_| format!("{path}: bad minute in '{s}'"))?;
    if h > 23 || m > 59 {
        return Err(format!("{path}: time out of range in '{s}'"));
    }
    Ok(h * 60 + m)
}

fn parse_tz(s: &str, path: &str) -> Result<chrono_tz::Tz, String> {
    chrono_tz::Tz::from_str(s).map_err(|_| format!("{path}: unknown timezone '{s}'"))
}

fn parse_days(
    v: &serde_yaml::Value,
    path: &str,
) -> Result<(Vec<chrono::Weekday>, chrono_tz::Tz), String> {
    let (day_vals, tz_str): (Vec<&serde_yaml::Value>, &str) = match v {
        serde_yaml::Value::Sequence(seq) => (seq.iter().collect(), "UTC"),
        serde_yaml::Value::Mapping(m) => {
            let tz = get_key(m, "tz").and_then(|t| t.as_str()).unwrap_or("UTC");
            let days = get_key(m, "days")
                .and_then(|d| d.as_sequence())
                .ok_or_else(|| format!("{path}: expected 'days' list"))?;
            (days.iter().collect(), tz)
        }
        _ => return Err(format!("{path}: expected a day list or {{days, tz}}")),
    };
    let mut days = Vec::new();
    for d in day_vals {
        let s = d
            .as_str()
            .ok_or_else(|| format!("{path}: day must be a string"))?;
        let wd = match s.to_lowercase().as_str() {
            "mon" | "monday" => chrono::Weekday::Mon,
            "tue" | "tuesday" => chrono::Weekday::Tue,
            "wed" | "wednesday" => chrono::Weekday::Wed,
            "thu" | "thursday" => chrono::Weekday::Thu,
            "fri" | "friday" => chrono::Weekday::Fri,
            "sat" | "saturday" => chrono::Weekday::Sat,
            "sun" | "sunday" => chrono::Weekday::Sun,
            other => return Err(format!("{path}: unknown day '{other}'")),
        };
        days.push(wd);
    }
    if days.is_empty() {
        return Err(format!("{path}: day list is empty"));
    }
    Ok((days, parse_tz(tz_str, &format!("{path}.tz"))?))
}

/// Validate an indicator spec's parameters.
pub fn validate_indicator(spec: &IndicatorSpec) -> Result<(), String> {
    spec.kind.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn parses_nested_operators() {
        let e = parse_expr(
            &yaml("{cross_above: [{field: close}, {sma: {source: {field: close}, period: 20}}]}"),
            None,
            "root",
        )
        .unwrap();
        let t = e.check_types("root").unwrap();
        assert_eq!(t, ExprType::Bool);
        let mut inds = BTreeMap::new();
        e.collect_indicators(&mut inds);
        assert_eq!(inds.len(), 1);
        let key = inds.keys().next().unwrap();
        assert_eq!(key, "base|sma(20)|close");
    }

    #[test]
    fn rejects_bool_as_comparison_operand() {
        let e = parse_expr(&yaml("{gt: [{position_direction: long}, 1]}"), None, "root").unwrap();
        assert!(
            e.check_types("root").is_err(),
            "position_direction is Bool; gt needs Num"
        );
    }

    #[test]
    fn rejects_bare_indicator_as_condition() {
        // Spec: an indicator value alone is NOT a condition. It type-checks as
        // Num here; the COMPILER rejects Num-typed rule conditions (see
        // runtime::tests). This test pins the typing behavior.
        let e = parse_expr(
            &yaml("{sma: {source: {field: close}, period: 5}}"),
            None,
            "root",
        )
        .unwrap();
        assert_eq!(e.check_types("root").unwrap(), ExprType::Num);
    }

    #[test]
    fn rejects_unknown_operator() {
        let err = parse_expr(&yaml("{frobnicate: [1, 2]}"), None, "root").unwrap_err();
        assert!(err.contains("unknown operator"));
    }

    #[test]
    fn rejects_multi_key_mapping() {
        let err = parse_expr(&yaml("{gt: [1, 2], lt: [3, 4]}"), None, "root").unwrap_err();
        assert!(err.contains("exactly one"));
    }

    #[test]
    fn timeframe_wrapper() {
        let e = parse_expr(
            &yaml("{gt: [{timeframe: \"1D\", of: {field: close}}, {field: close}]}"),
            None,
            "root",
        )
        .unwrap();
        let mut inds = BTreeMap::new();
        e.collect_indicators(&mut inds);
        assert_eq!(inds.len(), 0);
        match &e {
            Expr::Gt(a, _) => match &**a {
                Expr::Field { tf, .. } => assert_eq!(tf.as_deref(), Some("1D")),
                other => panic!("expected htf field, got {other:?}"),
            },
            other => panic!("expected gt, got {other:?}"),
        }
    }

    #[test]
    fn htf_indicator() {
        let e = parse_expr(
            &yaml("{sma: {source: {timeframe: \"1D\", of: {field: close}}, period: 10}}"),
            None,
            "root",
        )
        .unwrap();
        let mut inds = BTreeMap::new();
        e.collect_indicators(&mut inds);
        let key = inds.keys().next().unwrap().clone();
        assert_eq!(key, "1D|sma(10)|close");
    }

    #[test]
    fn rejects_nested_indicator_source() {
        let err = parse_expr(
            &yaml("{sma: {source: {sma: {source: {field: close}, period: 5}}, period: 10}}"),
            None,
            "root",
        )
        .unwrap_err();
        assert!(err.contains("plain price fields"), "got: {err}");
    }

    #[test]
    fn pct_change_type() {
        let e = parse_expr(
            &yaml("{gt: [{pct_change: {of: {field: close}, bars: 5}}, 2.0]}"),
            None,
            "root",
        )
        .unwrap();
        assert!(e.check_types("root").is_ok());
        // HTF source rejected for pct_change (documented restriction)
        let bad = parse_expr(
            &yaml("{pct_change: {of: {timeframe: \"1D\", of: {field: close}}, bars: 5}}"),
            None,
            "root",
        )
        .unwrap();
        assert!(bad.check_types("root").is_err());
    }

    #[test]
    fn signal_lookup_respects_time() {
        use std::collections::BTreeMap;
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let t1 = bt_core::time::parse_timestamp("2024-01-01T00:00:00Z", utc, "t").unwrap();
        let t2 = bt_core::time::parse_timestamp("2024-01-03T00:00:00Z", utc, "t").unwrap();
        let mut signals = BTreeMap::new();
        signals.insert(
            "bias".to_string(),
            vec![(t1, D::from(1)), (t2, D::from(-1))],
        );
        let empty_bars: Vec<bt_data::Bar> = Vec::new();
        let bars = crate::runtime::BarAccess::new(&empty_bars, 0);
        let empty_htf: BTreeMap<String, crate::runtime::BarAccess> = BTreeMap::new();
        let empty_syms: BTreeMap<String, crate::runtime::BarAccess> = BTreeMap::new();
        let ind = |_k: &str, _o: usize| Value::NA;
        let decision = bt_core::time::parse_timestamp("2024-01-02T00:00:00Z", utc, "t").unwrap();
        let ctx = EvalContext {
            decision_ts: decision,
            base_bars: &bars,
            htf_bars: &empty_htf,
            symbol_bars: &empty_syms,
            ind: &ind,
            position: None,
            account: AccountView {
                equity: D::ZERO,
                balance: D::ZERO,
                drawdown_pct: D::ZERO,
            },
            signals: &signals,
        };
        let e = parse_expr(&yaml("{signal: {key: bias}}"), None, "root").unwrap();
        assert_eq!(
            e.eval(&ctx, 0),
            Value::Num(D::from(1)),
            "later observation must be invisible"
        );
    }
}
