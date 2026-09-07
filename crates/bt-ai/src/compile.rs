//! `ai compile`: natural-language strategy.md → machine spec, with the
//! ENGINE as the sole validator. The model proposes; the compiler disposes.
//! On INVALID, the exact compiler errors go back to the model for repair
//! (bounded rounds). Every call is gateway-audited.

use crate::gateway::{Gateway, GatewayOutcome};
use crate::prompts::{compile_system_prompt, compile_user_prompt, repair_user_prompt, DataContext};
use bt_core::error::{CoreError, CoreResult};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CompileOutcome {
    pub strategy_name: String,
    /// YAML text of the validated spec (with strategy: wrapper).
    pub spec_yaml: String,
    pub assumptions: Vec<String>,
    pub rounds: usize,
    pub ledger_ids: Vec<u64>,
    pub cache_hits: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DryRunReport {
    pub prompt_chars: usize,
    pub prompt_hash: String,
    pub system_chars: usize,
    pub max_repair_rounds: u32,
    pub model: String,
    pub preview: String,
}

struct Candidate {
    strategy_name: String,
    spec_json: serde_json::Value,
    assumptions: Vec<String>,
}

/// Parse the model's JSON response into a candidate (name, spec, assumptions).
fn parse_candidate(response_text: &str) -> CoreResult<Candidate> {
    let json_text = crate::provider::extract_json_object(response_text)?;
    let v: serde_json::Value = serde_json::from_str(&json_text)
        .map_err(|e| CoreError::InvalidData(format!("response JSON: {e}")))?;
    let name = v
        .get("strategy_name")
        .and_then(|x| x.as_str())
        .unwrap_or("unnamed")
        .to_string();
    let spec = v
        .get("spec")
        .cloned()
        .ok_or_else(|| CoreError::InvalidData("response JSON missing 'spec' object".into()))?;
    let assumptions = v
        .get("assumptions")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(Candidate {
        strategy_name: name,
        spec_json: spec,
        assumptions,
    })
}

/// Serialize a spec JSON value to YAML text for parse_spec.
fn spec_json_to_yaml(spec: &serde_json::Value) -> CoreResult<String> {
    let json_text = serde_json::to_string(spec)
        .map_err(|e| CoreError::InvalidData(format!("spec json serialize: {e}")))?;
    let yaml_value: serde_yaml::Value = serde_yaml::from_str(&json_text)
        .map_err(|e| CoreError::InvalidData(format!("spec json->yaml: {e}")))?;
    serde_yaml::to_string(&yaml_value)
        .map_err(|e| CoreError::InvalidData(format!("spec yaml serialize: {e}")))
}

/// Validate a candidate spec against the kernel compiler.
fn validate_candidate(spec_yaml: &str, base_secs: i64) -> Result<(), Vec<String>> {
    let spec = bt_strategy::spec::parse_spec(spec_yaml).map_err(|e| vec![e])?;
    bt_strategy::runtime::compile(&spec, base_secs).map(|_| ())
}

fn one_round(
    gateway: &mut Gateway,
    purpose: &str,
    system: &str,
    user: &str,
    json_mode: bool,
) -> CoreResult<(GatewayOutcome, Candidate)> {
    let outcome = gateway.run(purpose, system, user, json_mode, false)?;
    let candidate = parse_candidate(&outcome.text).map_err(|e| {
        CoreError::InvalidData(format!("round failed (ledger #{}): {e}", outcome.ledger_id))
    })?;
    Ok((outcome, candidate))
}

pub struct CompileInput<'a> {
    pub strategy_md: &'a str,
    pub notes: &'a str,
    pub context: &'a DataContext,
    pub base_secs: i64,
    pub max_repair_rounds: u32,
    pub strategy_version: &'a str,
}

/// Full compile loop. `dry_run=true` returns the prompt report without any
/// network call.
pub fn compile_strategy(
    gateway: &mut Gateway,
    input: &CompileInput<'_>,
    dry_run: bool,
) -> CoreResult<Result<CompileOutcome, DryRunReport>> {
    let system = compile_system_prompt();
    let user = compile_user_prompt(input.strategy_md, input.notes, input.context);

    if dry_run {
        // Build the exact round-0 request and report it without network.
        let messages_len = system.len() + user.len();
        let probe = gateway.run("compile:dry-run", &system, &user, true, true)?;
        return Ok(Err(DryRunReport {
            prompt_chars: messages_len,
            prompt_hash: probe.prompt_hash,
            system_chars: system.len(),
            max_repair_rounds: input.max_repair_rounds,
            model: gateway.config().model.clone(),
            preview: crate::prompts::bound_text(&user, 2_500, "user prompt preview"),
        }));
    }

    let mut ledger_ids = Vec::new();
    let mut cache_hits = 0usize;
    let mut user_prompt = user;
    let mut rounds = 0usize;

    loop {
        rounds += 1;
        let (outcome, candidate) = one_round(gateway, "compile", &system, &user_prompt, true)?;
        ledger_ids.push(outcome.ledger_id);
        if outcome.cache_hit {
            cache_hits += 1;
        }
        let previous_response = outcome.text.clone();

        let spec_yaml = spec_json_to_yaml(&candidate.spec_json)?;
        match validate_candidate(&spec_yaml, input.base_secs) {
            Ok(()) => {
                return Ok(Ok(CompileOutcome {
                    strategy_name: candidate.strategy_name,
                    spec_yaml,
                    assumptions: candidate.assumptions,
                    rounds,
                    ledger_ids,
                    cache_hits,
                }))
            }
            Err(errors) => {
                if rounds > input.max_repair_rounds as usize {
                    return Err(CoreError::StrategyError(format!(
                        "strategy INVALID after {} round(s) ({} repair round(s) allowed). \
                         Compiler errors:\n- {}\nFull audit: ledger entries {:?}",
                        rounds,
                        input.max_repair_rounds,
                        errors.join("\n- "),
                        ledger_ids
                    )));
                }
                user_prompt = repair_user_prompt(&previous_response, &errors);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{Gateway, GatewayConfig};
    use crate::ledger::LedgerWriter;
    use std::sync::atomic::{AtomicU64, Ordering};

    const GOOD_SPEC: &str = r#"{
        "strategy_name": "sma_touch",
        "spec": {
            "strategy": {
                "name": "sma_touch",
                "symbols": ["X"],
                "entry": {
                    "direction": "long",
                    "when": {"gt": [{"field": "close"}, {"sma": {"source": {"field": "close"}, "period": 3}}]}
                },
                "orders": {"stop_loss": {"type": "fixed_distance", "value": 2}},
                "risk": {"sizing": {"mode": "fixed_quantity", "qty": 1}}
            }
        },
        "assumptions": ["No exit mentioned: exit only via stop-loss."]
    }"#;

    const BAD_SPEC: &str = r#"{
        "strategy_name": "broken",
        "spec": {
            "strategy": {
                "name": "broken",
                "symbols": ["X"],
                "entry": {"direction": "long", "when": {"sma": {"source": {"field": "close"}, "period": 3}}}
            }
        },
        "assumptions": []
    }"#;

    /// Fake provider: returns queued responses; records call count.
    struct MockProvider {
        queue: std::sync::Mutex<Vec<String>>,
        calls: AtomicU64,
    }
    impl MockProvider {
        fn new(responses: Vec<String>) -> Self {
            MockProvider {
                queue: std::sync::Mutex::new(responses),
                calls: AtomicU64::new(0),
            }
        }
    }
    impl crate::provider::Provider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn complete(
            &self,
            _req: &crate::provider::CompletionRequest,
        ) -> CoreResult<crate::provider::CompletionResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut q = self.queue.lock().unwrap();
            if q.is_empty() {
                return Err(CoreError::InvalidData("mock queue empty".into()));
            }
            Ok(crate::provider::CompletionResponse {
                text: q.remove(0),
                usage: crate::provider::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                },
            })
        }
    }

    fn ctx() -> DataContext {
        DataContext {
            symbols: vec!["X".into()],
            base_timeframe: "1h".into(),
            bars: 100,
            start: "2024-01-01".into(),
            end: "2024-01-05".into(),
            timezone: "UTC".into(),
        }
    }

    fn input<'a>(md: &'a str, ctx: &'a DataContext) -> CompileInput<'a> {
        CompileInput {
            strategy_md: md,
            notes: "",
            context: ctx,
            base_secs: 3600,
            max_repair_rounds: 2,
            strategy_version: "v1",
        }
    }

    fn fresh_gateway(provider: Box<dyn crate::provider::Provider>) -> Gateway {
        let dir = std::env::temp_dir().join(format!(
            "bt_ai_gw_{}_{}",
            std::process::id(),
            AtomicU64::new(1).fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let ledger = LedgerWriter::new(dir.join("ledger.jsonl")).unwrap();
        Gateway::with_provider(GatewayConfig::default(), provider, None, Some(ledger))
    }

    #[test]
    fn dry_run_builds_prompt_without_network() {
        let mut g = fresh_gateway(Box::new(MockProvider::new(vec![])));
        let result =
            compile_strategy(&mut g, &input("Buy when price above SMA.", &ctx()), true).unwrap();
        let report = result.unwrap_err();
        assert!(report.prompt_chars > 1000, "grammar + context included");
        assert!(!report.prompt_hash.is_empty());
        assert!(report.preview.contains("Buy when price above SMA"));
        assert_eq!(g.tokens_used(), 0, "dry run never calls the provider");
    }

    #[test]
    fn compile_succeeds_first_round() {
        let provider = MockProvider::new(vec![GOOD_SPEC.to_string()]);
        let mut g = fresh_gateway(Box::new(provider));
        let result = compile_strategy(&mut g, &input("Buy above SMA3, stop 2.", &ctx()), false)
            .unwrap()
            .unwrap();
        assert_eq!(result.rounds, 1);
        assert_eq!(result.strategy_name, "sma_touch");
        assert!(!result.assumptions.is_empty());
        assert!(result.spec_yaml.contains("sma_touch"));
        assert!(result.ledger_ids.len() == 1);
    }

    #[test]
    fn repair_loop_recovers_from_invalid_spec() {
        // Round 1: bare-indicator condition (compiler rejects); round 2: fixed.
        let provider = MockProvider::new(vec![BAD_SPEC.to_string(), GOOD_SPEC.to_string()]);
        let mut g = fresh_gateway(Box::new(provider));
        let result = compile_strategy(&mut g, &input("Buy above SMA3, stop 2.", &ctx()), false)
            .unwrap()
            .unwrap();
        assert_eq!(result.rounds, 2, "one repair round used");
        assert_eq!(result.strategy_name, "sma_touch");
    }

    #[test]
    fn repair_loop_gives_up_with_exact_errors() {
        let provider = MockProvider::new(vec![BAD_SPEC.to_string(); 5]);
        let mut g = fresh_gateway(Box::new(provider));
        let err = compile_strategy(&mut g, &input("broken", &ctx()), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("INVALID after"), "{msg}");
        assert!(msg.contains("expected a boolean condition"), "{msg}");
    }

    #[test]
    fn ledger_records_every_round() {
        let provider = MockProvider::new(vec![BAD_SPEC.to_string(), GOOD_SPEC.to_string()]);
        let dir = std::env::temp_dir().join(format!("bt_ai_led_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ledger = LedgerWriter::new(dir.join("ledger.jsonl")).unwrap();
        let mut g = Gateway::with_provider(
            GatewayConfig::default(),
            Box::new(provider),
            None,
            Some(ledger),
        );
        let _ = compile_strategy(&mut g, &input("x", &ctx()), false)
            .unwrap()
            .unwrap();
        let w = LedgerWriter::new(dir.join("ledger.jsonl")).unwrap();
        assert_eq!(w.entry_count(), 2, "both rounds audited");
        let entries = w.read_last(2).unwrap();
        for (_, line) in entries {
            assert!(!line.contains("api_key"), "no secrets in ledger");
            assert!(line.contains("\"purpose\":\"compile\""));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_stops_runaway_loops() {
        let provider = MockProvider::new(vec![BAD_SPEC.to_string(); 50]);
        let cfg = GatewayConfig {
            max_tokens: 100,
            budget_tokens_per_command: 250, // ~2 calls then stop
            ..Default::default()
        };
        let dir = std::env::temp_dir().join(format!("bt_ai_bud_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ledger = LedgerWriter::new(dir.join("ledger.jsonl")).unwrap();
        let mut g = Gateway::with_provider(cfg, Box::new(provider), None, Some(ledger));
        let err = compile_strategy(&mut g, &input("x", &ctx()), false).unwrap_err();
        assert!(
            err.to_string().contains("budget exhausted")
                || err.to_string().contains("INVALID after"),
            "budget or repair cap ends the loop: {err}"
        );
        assert!(g.tokens_used() <= 250 + 100, "budget respected");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
