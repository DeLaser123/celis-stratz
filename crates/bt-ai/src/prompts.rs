//! Prompt construction: the spec grammar handed to the model, the
//! mechanization system prompt, and grounded-context builders for
//! review/explain/ask. The grammar is derived from STRATEGY_SPEC.md — the
//! model proposes, the engine's compiler disposes.

use bt_core::D;
use serde::Serialize;

/// Compact spec grammar for the prompt (must stay in sync with
/// STRATEGY_SPEC.md — the compiler is the source of truth, this is a map).
pub const SPEC_GRAMMAR: &str = r#"
STRATEGY SPEC GRAMMAR (the engine validates strictly; unknown fields/keys are errors):

Document shape (YAML/JSON):
{
  "strategy": {
    "name": "<string>",
    "symbols": ["<symbol>", ...],
    "timeframes": ["1D", ...],          // optional; extra HIGHER timeframes only, declared here
    "entry":   { "direction": "long"|"short", "when": <condition> },
    "entry_short": { "direction": "short", "when": <condition> },   // optional
    "exit":    { "when": <condition> },                              // optional
    "orders": {
      "entry": {"type": "market"},
      "stop_loss":    {"type": "none"} | {"type": "fixed_distance", "value": <num>} | {"type": "atr_multiple", "period": <int>, "multiple": <num>},
      "take_profit":  {"type": "none"} | {"type": "fixed_distance", "value": <num>} | {"type": "risk_multiple", "value": <num>},
      "trailing_stop": null | {"type": "fixed_distance", "value": <num>} | {"type": "atr_multiple", "period": <int>, "multiple": <num>}
    },
    "risk": { "sizing":
        {"mode": "fixed_quantity", "qty": <num>} |
        {"mode": "fixed_amount", "amount": <num>} |
        {"mode": "percent_equity", "value": <pct>} |
        {"mode": "percent_risk", "value": <pct>} |        // requires stop_loss
        {"mode": "risk_amount", "amount": <num>} |        // requires stop_loss
        {"mode": "atr_based", "value": <pct>, "atr_period": <int>, "multiple": <num>}
    }
  }
}

<condition> is ONE of:
  scalar number                              e.g. 1.10
  {"field": "open"|"high"|"low"|"close"|"volume"|"hl2"|"hlc3"|"ohlc4"}
  {"timeframe": "<tf>", "of": <condition>}   // reads a HIGHER timeframe bar (must be declared in timeframes)
  {"and": [<condition>, ...]} | {"or": [<condition>, ...]} | {"not": <condition>}
  {"gt": [<a>, <b>]} | {"gte": [..]} | {"lt": [..]} | {"lte": [..]} | {"eq": [..]} | {"ne": [..]}   // numeric comparisons
  {"add": [a,b]} | {"sub": [a,b]} | {"mul": [a,b]} | {"div": [a,b]} | {"neg": a} | {"abs": a}
  {"cross_above": [<a>, <b>]} | {"cross_below": [<a>, <b>]}   // a vs b now and at the previous bar
  {"between": {"value": v, "min": lo, "max": hi}}
  {"pct_change": {"of": <field or indicator>, "bars": <int>}}  // base timeframe only
  {"time_of_day": {"from": "HH:MM", "to": "HH:MM", "tz": "Area/City"}}   // inclusive from, exclusive to
  {"day_of_week": ["mon", ...], "tz": "..."} or {"day_of_week": {"days": [...], "tz": "..."}}
  {"position_direction": "long"|"short"|"flat"} | {"bars_in_position": {}} | {"unrealized_pnl": {}} | {"unrealized_pnl_pct": {}}
  {"equity": {}} | {"balance": {}} | {"drawdown_pct": {}}
  {"signal": {"key": "<name>"}}      // value of an external timestamped signal (from Trades/signals_*.csv)

Indicators (as <condition> values; "source" must be a plain price field):
  {"sma": {"source": {field}, "period": N}} | {"ema": ...} | {"wma": ...} | {"rsi": ...} | {"stddev": ...} | {"roc": ...}
  {"highest": {..}} | {"lowest": {..}} | {"rolling_high": {..}} | {"rolling_low": {..}}   // aliases
  {"atr": {"period": N}}                       // no source
  {"macd": {"source": {field}, "fast": 12, "slow": 26, "signal": 9, "component": "macd"|"signal"|"hist"}}
  {"bollinger": {"source": {field}, "period": 20, "k": 2, "component": "upper"|"middle"|"lower"}}

HARD RULES:
- A bare indicator/field value is NOT a condition: wrap it in gt/lt/cross_above/... (the compiler rejects booleans-as-numbers and numbers-as-booleans).
- Indicator "source" must be a plain field — nested indicators are rejected.
- Any timeframe referenced via {"timeframe": ...} or an indicator source MUST be declared in "timeframes" (and be a higher timeframe than the base).
- percent_risk / risk_amount sizing REQUIRE stop_loss; risk_multiple take_profit REQUIRES stop_loss.
- Undefined data (indicator warmup) makes a condition FALSE; missing source values reset indicator windows.
- Sessions use the venue timezone explicitly (tz). Never assume a timezone.
"#;

pub fn compile_system_prompt() -> String {
    format!(
        r#"You are the Celis strategy mechanization compiler. You convert a trader's
natural-language strategy description into the Celis machine-readable spec.

{grammar}

RESPONSE CONTRACT (strict):
- Output ONLY one JSON object, no prose, no code fences.
- Shape:
  {{
    "strategy_name": "<short name>",
    "spec": {{ "strategy": {{ ...full spec document as per grammar... }} }},
    "assumptions": ["<every decision you had to make because the text was ambiguous>", ...]
  }}
- NEVER invent logic: if the text does not specify something the grammar needs,
  make the most conservative standard choice and record it in "assumptions".
  E.g. if no stop-loss is mentioned, use {{"type": "none"}} and add an assumption.
- Prefer conservative defaults: stops before targets, sessions closed outside
  stated hours, symmetric handling of long/short.
- Symbols must use the symbols given in the CONTEXT block. If the text names an
  unknown instrument, use the context's symbol and note the mapping in assumptions.
"#,
        grammar = SPEC_GRAMMAR
    )
}

/// What the compiler knows about the data, kept small and factual.
#[derive(Debug, Clone, Serialize)]
pub struct DataContext {
    pub symbols: Vec<String>,
    pub base_timeframe: String,
    pub bars: usize,
    pub start: String,
    pub end: String,
    pub timezone: String,
}

impl DataContext {
    pub fn render(&self) -> String {
        format!(
            "CONTEXT:\n- symbols: {}\n- base timeframe: {}\n- bars: {}\n- data span: {} .. {}\n- timestamp timezone: {}\n",
            self.symbols.join(", "),
            self.base_timeframe,
            self.bars,
            self.start,
            self.end,
            self.timezone,
        )
    }
}

/// Bound long text for prompts (chars).
pub fn bound_text(text: &str, max_chars: usize, label: &str) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut cut = max_chars;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}…\n[{label} truncated at {max_chars} chars]",
        &text[..cut]
    )
}

/// Compile user prompt: context + strategy markdown (+ bounded notes).
pub fn compile_user_prompt(strategy_md: &str, notes: &str, ctx: &DataContext) -> String {
    format!(
        "{}\nSTRATEGY DESCRIPTION (from Strategy/<version>/strategy.md):\n{}\n{}\nNow produce the JSON object per the response contract.",
        ctx.render(),
        bound_text(strategy_md, 20_000, "strategy.md"),
        if notes.trim().is_empty() {
            String::new()
        } else {
            format!("\nPROJECT NOTES (trader-provided context):\n{}\n", bound_text(notes, 8_000, "notes"))
        }
    )
}

/// Repair prompt: previous response + compiler errors.
pub fn repair_user_prompt(previous_response: &str, errors: &[String]) -> String {
    let listed = errors
        .iter()
        .map(|e| format!("- {e}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Your previous response FAILED the engine's compiler validation with these exact errors:\n{}\n\nPrevious response:\n{}\n\nReturn the corrected JSON object per the same contract. Fix ONLY what the errors require; keep everything else identical.",
        listed,
        bound_text(previous_response, 12_000, "previous response")
    )
}

pub fn review_system_prompt() -> String {
    r#"You are a rigorous quantitative reviewer. You are given a machine-readable
trading strategy spec plus a factual data profile. Review it for:
1) look-ahead or information-leakage risks, 2) ambiguity in the mechanical rules,
3) overfitting smells (too many parameters for the rule complexity, magic numbers),
4) risk-control gaps (no stop, unlimited size, missing session filters),
5) unrealistic assumptions about data (gaps, warmup, costs).

Rules: cite ONLY numbers that appear in the provided material. If you cannot
verify something, say so explicitly. Be terse and concrete. Output markdown."#
        .to_string()
}

pub fn explain_system_prompt() -> String {
    r#"You are a quantitative performance analyst. You are given the exact metrics
JSON, headline summary and a bounded sample of trades from a deterministic
backtest. Analyze performance and risk.

Rules: every number you quote MUST come verbatim from the provided artifacts —
the engine re-checks quoted numbers and flags any that do not appear in the
context. If something is not in the artifacts, say "not in the provided data".
Output markdown with short sections: Performance, Risk, Trade quality,
Caveats, Questions for the researcher."#
        .to_string()
}

pub fn ask_system_prompt() -> String {
    r#"You are the Celis project assistant. Answer the researcher's question using
ONLY the provided project material (notes, data profile, registry summaries).
If the answer is not in the material, say so. Output markdown."#
        .to_string()
}

/// Quoted-number verification: numbers the model cited that do not appear in
/// the provided context (lightweight anti-hallucination check — it flags,
/// never silently drops).
pub fn unverified_numbers(response: &str, context: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut token = String::new();
    let flush = |tok: &mut String, found: &mut Vec<String>| {
        if !tok.is_empty() {
            let t = tok.clone();
            if !context.contains(&t) && !found.contains(&t) {
                found.push(t);
            }
            tok.clear();
        }
    };
    for ch in response.chars() {
        if ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == '%' {
            if ch == '%' {
                flush(&mut token, &mut found);
                // treat "12.3%" as "12.3" for matching (context holds the raw number)
                continue;
            }
            token.push(ch);
        } else {
            flush(&mut token, &mut found);
        }
    }
    flush(&mut token, &mut found);
    // Ignore trivial numbers that add no audit value.
    found.retain(|t| {
        let cleaned = t.trim_end_matches('.');
        !(cleaned.is_empty()
            || cleaned == "-"
            || cleaned
                .parse::<D>()
                .map(|v| v.abs() < D::from(10))
                .unwrap_or(true))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_contains_hard_rules() {
        assert!(SPEC_GRAMMAR.contains("cross_above"));
        assert!(SPEC_GRAMMAR.contains("MUST be declared"));
        assert!(SPEC_GRAMMAR.contains("percent_risk"));
    }

    #[test]
    fn context_renders_facts() {
        let ctx = DataContext {
            symbols: vec!["EURUSD".into()],
            base_timeframe: "1h".into(),
            bars: 300,
            start: "2024-01-01".into(),
            end: "2024-01-13".into(),
            timezone: "UTC".into(),
        };
        let r = ctx.render();
        assert!(r.contains("EURUSD"));
        assert!(r.contains("300"));
    }

    #[test]
    fn unverified_numbers_flag_invented_values() {
        let ctx = "final_equity: 101516.18, sharpe: 3.72";
        let ok = "Equity ended at 101516.18 with sharpe 3.72.";
        assert!(unverified_numbers(ok, ctx).is_empty());
        let bad = "Equity ended at 101516.18 and the strategy made 999.99 per trade.";
        let flagged = unverified_numbers(bad, ctx);
        assert!(flagged.iter().any(|t| t.contains("999.99")), "{flagged:?}");
    }
}
