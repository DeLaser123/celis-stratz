//! Intrabar trigger evaluation and ambiguity policy (spec §7).
//!
//! OHLC bars cannot reveal the true intrabar path. When a bar's range touches
//! both the stop-loss and the take-profit of the same position, the configured
//! policy decides — and `Reject` refuses to guess (the exit is deferred and
//! the ambiguity is reported). The chosen policy appears in the final report.

use bt_core::D;
use serde::{Deserialize, Serialize};

/// Intrabar price extremes of one bar (decoupled from the data layer).
#[derive(Debug, Clone, Copy)]
pub struct BarSpan {
    pub open: D,
    pub high: D,
    pub low: D,
    pub close: D,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerPriority {
    StopLoss,
    TakeProfit,
    TrailingStop,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbiguityPolicy {
    /// Stop-loss assumed first (worst case). Default.
    #[default]
    Conservative,
    /// Take-profit assumed first (best case).
    Optimistic,
    /// Up bar (close >= open) ⇒ take-profit first; down bar ⇒ stop first;
    /// doji ⇒ conservative.
    OhlcPath,
    /// Refuse to guess: defer the exit, log an `AmbiguityDeferred` event.
    Reject,
    /// User-specified priority list.
    Explicit(Vec<TriggerPriority>),
}

/// Protective levels attached to an open position (one stop level max:
/// trailing replaces the fixed stop once active).
#[derive(Debug, Clone, Copy, Default)]
pub struct ExitTriggers {
    pub stop_loss: Option<D>,
    pub take_profit: Option<D>,
    pub trailing_stop: Option<D>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitKind {
    StopLoss,
    TakeProfit,
    TrailingStop,
}

impl ExitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitKind::StopLoss => "stop_loss",
            ExitKind::TakeProfit => "take_profit",
            ExitKind::TrailingStop => "trailing_stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitDecision {
    None,
    /// Triggered exit: kind + raw fill price (gap-aware).
    Exit(ExitKind, D),
    /// Both a stop-like and the target triggered in this bar and the policy
    /// is `Reject`: no fill this bar.
    Ambiguous,
}

/// Would a stop order at `level` trigger inside this bar, and at what raw
/// price? `long_side` = the position being protected is long (sell stop).
/// Gap rule: if the bar opens beyond the level, fill at the open (worse).
pub fn stop_trigger(long_side: bool, level: D, bar: BarSpan, strict: bool) -> Option<D> {
    let touched = if long_side {
        if strict {
            bar.low < level
        } else {
            bar.low <= level
        }
    } else if strict {
        bar.high > level
    } else {
        bar.high >= level
    };
    if !touched {
        return None;
    }
    Some(if long_side {
        // sell stop: gap down through level => fill at open
        if bar.open <= level {
            bar.open
        } else {
            level
        }
    } else {
        // buy stop: gap up through level => fill at open
        if bar.open >= level {
            bar.open
        } else {
            level
        }
    })
}

/// Would a limit order at `level` trigger inside this bar? Gap rule: if the
/// bar opens through the level (favorable gap), fill at the open (better).
pub fn limit_trigger(long_side: bool, level: D, bar: BarSpan, strict: bool) -> Option<D> {
    let touched = if long_side {
        if strict {
            bar.low < level
        } else {
            bar.low <= level
        }
    } else if strict {
        bar.high > level
    } else {
        bar.high >= level
    };
    if !touched {
        return None;
    }
    Some(if long_side {
        // buy limit: gap below level => fill at open (better)
        if bar.open < level {
            bar.open
        } else {
            level
        }
    } else {
        // sell limit: gap above level => fill at open (better)
        if bar.open > level {
            bar.open
        } else {
            level
        }
    })
}

/// Evaluate the protective triggers of an open position against one bar.
/// `long` = position direction. Trailing (when set) supersedes the fixed stop.
pub fn evaluate_exit(
    long: bool,
    triggers: &ExitTriggers,
    bar: BarSpan,
    strict: bool,
    policy: &AmbiguityPolicy,
) -> ExitDecision {
    // Effective stop level: trailing (when set) supersedes the fixed stop.
    let stop_level = match (triggers.trailing_stop, triggers.stop_loss) {
        (Some(t), Some(s)) => Some(if long { t.max(s) } else { t.min(s) }),
        (Some(t), None) => Some(t),
        (None, s) => s,
    };
    let stop_kind = if triggers.trailing_stop.is_some() {
        ExitKind::TrailingStop
    } else {
        ExitKind::StopLoss
    };

    let mut hits: Vec<(ExitKind, D)> = Vec::new();
    if let Some(level) = stop_level {
        if let Some(price) = stop_trigger(long, level, bar, strict) {
            hits.push((stop_kind, price));
        }
    }
    if let Some(level) = triggers.take_profit {
        // Position long => TP is a SELL limit; position short => BUY limit.
        if let Some(price) = limit_trigger(!long, level, bar, strict) {
            hits.push((ExitKind::TakeProfit, price));
        }
    }
    if hits.is_empty() {
        return ExitDecision::None;
    }
    if hits.len() == 1 {
        let (kind, price) = hits[0];
        return ExitDecision::Exit(kind, price);
    }

    // Ambiguity: stop-like AND target both touched.
    let stop_hit = hits
        .iter()
        .copied()
        .find(|(k, _)| *k != ExitKind::TakeProfit);
    let target_hit = hits
        .iter()
        .copied()
        .find(|(k, _)| *k == ExitKind::TakeProfit);
    let (stop_hit, target_hit) = match (stop_hit, target_hit) {
        (Some(s), Some(t)) => (s, t),
        _ => {
            // Same kind twice — impossible by construction; conservative pick.
            let (kind, price) = hits[0];
            return ExitDecision::Exit(kind, price);
        }
    };
    let up_bar = bar.close >= bar.open;
    let resolved: Option<(ExitKind, D)> = match policy {
        AmbiguityPolicy::Conservative => Some(stop_hit),
        AmbiguityPolicy::Optimistic => Some(target_hit),
        AmbiguityPolicy::OhlcPath => {
            if up_bar {
                Some(target_hit)
            } else {
                Some(stop_hit) // down bar or doji => conservative
            }
        }
        AmbiguityPolicy::Reject => None,
        AmbiguityPolicy::Explicit(list) => {
            let mut chosen = None;
            for p in list {
                let hit = match p {
                    TriggerPriority::StopLoss => Some(stop_hit),
                    TriggerPriority::TrailingStop => {
                        if stop_hit.0 == ExitKind::TrailingStop {
                            Some(stop_hit)
                        } else {
                            None
                        }
                    }
                    TriggerPriority::TakeProfit => Some(target_hit),
                };
                if let Some(h) = hit {
                    chosen = Some(h);
                    break;
                }
            }
            chosen.or(Some(stop_hit)) // fallback conservative if list incomplete
        }
    };
    match resolved {
        Some((kind, price)) => ExitDecision::Exit(kind, price),
        None => ExitDecision::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn span(o: D, h: D, l: D, c: D) -> BarSpan {
        BarSpan {
            open: o,
            high: h,
            low: l,
            close: c,
        }
    }

    #[test]
    fn long_stop_triggers_on_low_with_gap_rule() {
        let bar = span(dec!(100), dec!(102), dec!(94), dec!(101));
        // Open above the stop: fill at the stop level.
        assert_eq!(stop_trigger(true, dec!(95), bar, false), Some(dec!(95)));
        assert_eq!(stop_trigger(true, dec!(99), bar, false), Some(dec!(99)));
        assert_eq!(stop_trigger(true, dec!(93), bar, false), None);
        // Open gapped BELOW the stop: fill at the (worse) open.
        let gap = span(dec!(94), dec!(102), dec!(92), dec!(101));
        assert_eq!(stop_trigger(true, dec!(99), gap, false), Some(dec!(94)));
        // strict requires trade-through
        let exact = span(dec!(100), dec!(100), dec!(95), dec!(100));
        assert_eq!(stop_trigger(true, dec!(95), exact, false), Some(dec!(95)));
        assert_eq!(stop_trigger(true, dec!(95), exact, true), None);
    }

    #[test]
    fn short_stop_triggers_on_high() {
        // BUY stop (protecting a short): open below level => fill at level.
        let bar = span(dec!(100), dec!(106), dec!(99), dec!(105));
        assert_eq!(stop_trigger(false, dec!(105), bar, false), Some(dec!(105)));
        assert_eq!(stop_trigger(false, dec!(103), bar, false), Some(dec!(103)));
        // Open gapped ABOVE the stop => fill at the (worse) open.
        let gap = span(dec!(107), dec!(109), dec!(99), dec!(108));
        assert_eq!(stop_trigger(false, dec!(103), gap, false), Some(dec!(107)));
    }

    #[test]
    fn take_profit_limit_gap_rule() {
        // SELL limit (TP of a long): bar reaches 111, target 110 => fill 110.
        let bar = span(dec!(100), dec!(111), dec!(99), dec!(110));
        assert_eq!(limit_trigger(false, dec!(110), bar, false), Some(dec!(110)));
        // Gap above target at open => fill at open (better for the seller).
        let gap = span(dec!(112), dec!(115), dec!(108), dec!(111));
        assert_eq!(limit_trigger(false, dec!(110), gap, false), Some(dec!(112)));
        // BUY limit (TP of a short): bar reaches down to 88, target 90 => fill 90.
        let dn = span(dec!(95), dec!(96), dec!(88), dec!(90));
        assert_eq!(limit_trigger(true, dec!(90), dn, false), Some(dec!(90)));
        // Buy limit with favorable gap at open => fill at open (better).
        let gap_dn = span(dec!(85), dec!(96), dec!(80), dec!(90));
        assert_eq!(limit_trigger(true, dec!(90), gap_dn, false), Some(dec!(85)));
    }

    #[test]
    fn ambiguity_policies() {
        // One bar touches both SL 95 and TP 110.
        let bar = span(dec!(100), dec!(111), dec!(94), dec!(105));
        let trig = ExitTriggers {
            stop_loss: Some(dec!(95)),
            take_profit: Some(dec!(110)),
            trailing_stop: None,
        };
        match evaluate_exit(true, &trig, bar, false, &AmbiguityPolicy::Conservative) {
            ExitDecision::Exit(ExitKind::StopLoss, p) => assert_eq!(p, dec!(95)),
            other => panic!("expected conservative stop, got {other:?}"),
        }
        match evaluate_exit(true, &trig, bar, false, &AmbiguityPolicy::Optimistic) {
            ExitDecision::Exit(ExitKind::TakeProfit, p) => assert_eq!(p, dec!(110)),
            other => panic!("expected optimistic tp, got {other:?}"),
        }
        // Down bar with OhlcPath => stop first
        let down = span(dec!(105), dec!(111), dec!(94), dec!(96));
        match evaluate_exit(true, &trig, down, false, &AmbiguityPolicy::OhlcPath) {
            ExitDecision::Exit(ExitKind::StopLoss, _) => {}
            other => panic!("expected ohlc-path stop, got {other:?}"),
        }
        // Up bar with OhlcPath => tp first
        let up = span(dec!(95), dec!(111), dec!(94), dec!(108));
        match evaluate_exit(true, &trig, up, false, &AmbiguityPolicy::OhlcPath) {
            ExitDecision::Exit(ExitKind::TakeProfit, _) => {}
            other => panic!("expected ohlc-path tp, got {other:?}"),
        }
        // Reject policy refuses to guess.
        assert_eq!(
            evaluate_exit(true, &trig, bar, false, &AmbiguityPolicy::Reject),
            ExitDecision::Ambiguous
        );
        // Explicit priority: target before stop.
        let expl =
            AmbiguityPolicy::Explicit(vec![TriggerPriority::TakeProfit, TriggerPriority::StopLoss]);
        match evaluate_exit(true, &trig, bar, false, &expl) {
            ExitDecision::Exit(ExitKind::TakeProfit, _) => {}
            other => panic!("expected explicit tp, got {other:?}"),
        }
        // Unambiguous single hit ignores the policy.
        let only_sl = ExitTriggers {
            stop_loss: Some(dec!(95)),
            take_profit: None,
            trailing_stop: None,
        };
        match evaluate_exit(true, &only_sl, bar, false, &AmbiguityPolicy::Reject) {
            ExitDecision::Exit(ExitKind::StopLoss, _) => {}
            other => panic!("expected stop, got {other:?}"),
        }
    }

    #[test]
    fn trailing_supersedes_fixed_stop_tighter() {
        // long: fixed stop 95, trailing 98 => effective 98, kind = trailing
        let trig = ExitTriggers {
            stop_loss: Some(dec!(95)),
            take_profit: None,
            trailing_stop: Some(dec!(98)),
        };
        let bar = span(dec!(100), dec!(100), dec!(97), dec!(100));
        match evaluate_exit(true, &trig, bar, false, &AmbiguityPolicy::Conservative) {
            ExitDecision::Exit(ExitKind::TrailingStop, p) => assert_eq!(p, dec!(98)),
            other => panic!("expected trailing stop, got {other:?}"),
        }
    }
}
