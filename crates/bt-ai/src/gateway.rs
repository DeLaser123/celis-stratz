//! The gateway: single funnel for every AI call. Enforces token budgets,
//! consults the cache, records the audit ledger, and normalizes provider
//! errors. Dry-run mode builds and hashes the request without any network.

use crate::cache::{CachedResponse, ResponseCache};
use crate::ledger::{LedgerEntry, LedgerWriter};
use crate::provider::{CompletionRequest, CompletionResponse, Message, Provider};
use bt_core::error::{CoreError, CoreResult};
use bt_core::time::format_ts;
use std::time::Instant;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_budget")]
    pub budget_tokens_per_command: u64,
    #[serde(default = "default_repair_rounds")]
    pub max_repair_rounds: u32,
}

fn default_provider() -> String {
    "zai".into()
}
fn default_model() -> String {
    "glm-5.3-flash".into()
}
fn default_temperature() -> f64 {
    0.0
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_budget() -> u64 {
    200_000
}
fn default_repair_rounds() -> u32 {
    3
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            provider: default_provider(),
            model: default_model(),
            base_url: None,
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            budget_tokens_per_command: default_budget(),
            max_repair_rounds: default_repair_rounds(),
        }
    }
}

impl GatewayConfig {
    /// Effective base URL for the chosen provider.
    pub fn effective_base_url(&self) -> String {
        match self.base_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => url.to_string(),
            _ => crate::provider::ZAI_BASE_URL.to_string(),
        }
    }
}

pub struct GatewayOutcome {
    pub text: String,
    pub usage: crate::provider::Usage,
    pub cache_hit: bool,
    pub ledger_id: u64,
    pub prompt_hash: String,
    /// Total prompt chars (for dry-run reporting).
    pub prompt_chars: usize,
}

pub struct Gateway {
    provider: Box<dyn Provider>,
    cfg: GatewayConfig,
    cache: Option<ResponseCache>,
    ledger: Option<LedgerWriter>,
    tokens_used: u64,
}

impl Gateway {
    pub fn new(
        cfg: GatewayConfig,
        api_key: &str,
        cache: Option<ResponseCache>,
        ledger: Option<LedgerWriter>,
    ) -> Gateway {
        let provider: Box<dyn Provider> = Box::new(crate::provider::OpenAiCompatible::new(
            cfg.effective_base_url(),
            api_key.to_string(),
        ));
        Gateway {
            provider,
            cfg,
            cache,
            ledger,
            tokens_used: 0,
        }
    }

    /// Construct from a mock provider (tests / offline tools).
    pub fn with_provider(
        cfg: GatewayConfig,
        provider: Box<dyn Provider>,
        cache: Option<ResponseCache>,
        ledger: Option<LedgerWriter>,
    ) -> Gateway {
        Gateway {
            provider,
            cfg,
            cache,
            ledger,
            tokens_used: 0,
        }
    }

    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    pub fn config(&self) -> &GatewayConfig {
        &self.cfg
    }

    /// One completion. `purpose` names the audit reason (compile/review/...).
    pub fn run(
        &mut self,
        purpose: &str,
        system: &str,
        user: &str,
        json_mode: bool,
        dry_run: bool,
    ) -> CoreResult<GatewayOutcome> {
        let messages = vec![Message::system(system), Message::user(user)];
        let payload = serde_json::to_string(&messages)
            .map_err(|e| CoreError::InvalidData(format!("payload serialize: {e}")))?;
        let prompt_hash = ResponseCache::key(&self.cfg.model, self.provider.name(), &payload);
        let prompt_chars = messages.iter().map(|m| m.content.len()).sum();

        if dry_run {
            return Ok(GatewayOutcome {
                text: String::new(),
                usage: Default::default(),
                cache_hit: false,
                ledger_id: 0,
                prompt_hash,
                prompt_chars,
            });
        }

        // Budget: worst-case charge is max_tokens for this call.
        if self.tokens_used + self.cfg.max_tokens as u64 > self.cfg.budget_tokens_per_command {
            return Err(CoreError::InvalidData(format!(
                "AI token budget exhausted for this command (used {}, cap {})",
                self.tokens_used, self.cfg.budget_tokens_per_command
            )));
        }

        // Cache consult.
        let cache_key =
            ResponseCache::key(&self.cfg.model, &self.cfg.effective_base_url(), &payload);
        if let Some(cache) = &self.cache {
            if let Some(hit) = cache.get(&cache_key) {
                let entry = self.make_entry(
                    purpose,
                    &prompt_hash,
                    true,
                    hit.prompt_tokens,
                    hit.completion_tokens,
                    0,
                    &hit.text,
                    "ok",
                    None,
                );
                let ledger_id = self.record(entry);
                return Ok(GatewayOutcome {
                    text: hit.text,
                    usage: crate::provider::Usage {
                        prompt_tokens: hit.prompt_tokens,
                        completion_tokens: hit.completion_tokens,
                    },
                    cache_hit: true,
                    ledger_id,
                    prompt_hash,
                    prompt_chars,
                });
            }
        }

        let req = CompletionRequest {
            model: self.cfg.model.clone(),
            messages: messages.clone(),
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
            json_mode,
        };
        let started = Instant::now();
        let resp: CompletionResponse = self.provider.complete(&req).inspect_err(|e| {
            let entry = self.make_entry(
                purpose,
                &prompt_hash,
                false,
                0,
                0,
                started.elapsed().as_millis() as u64,
                "",
                "error",
                Some(e.to_string()),
            );
            let _ = self.record(entry);
        })?;
        let latency = started.elapsed().as_millis() as u64;
        self.tokens_used += resp.usage.total();

        if let Some(cache) = &self.cache {
            let _ = cache.put(
                &cache_key,
                &CachedResponse {
                    text: resp.text.clone(),
                    prompt_tokens: resp.usage.prompt_tokens,
                    completion_tokens: resp.usage.completion_tokens,
                    created_at: format_ts(bt_core::time::Ts::UNIX_EPOCH),
                },
            );
        }

        let entry = self.make_entry(
            purpose,
            &prompt_hash,
            false,
            resp.usage.prompt_tokens,
            resp.usage.completion_tokens,
            latency,
            &resp.text,
            "ok",
            None,
        );
        let ledger_id = self.record(entry);

        Ok(GatewayOutcome {
            text: resp.text,
            usage: resp.usage,
            cache_hit: false,
            ledger_id,
            prompt_hash,
            prompt_chars,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn make_entry(
        &self,
        purpose: &str,
        prompt_hash: &str,
        cache_hit: bool,
        prompt_tokens: u64,
        completion_tokens: u64,
        latency_ms: u64,
        response_text: &str,
        status: &str,
        error: Option<String>,
    ) -> LedgerEntry {
        LedgerEntry {
            id: 0,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            purpose: purpose.to_string(),
            provider: self.provider.name().to_string(),
            model: self.cfg.model.clone(),
            prompt_hash: prompt_hash.to_string(),
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
            cache_hit,
            prompt_tokens,
            completion_tokens,
            latency_ms,
            response_hash: bt_core::hash::sha256_hex(response_text.as_bytes()),
            status: status.to_string(),
            error,
        }
    }

    fn record(&self, entry: LedgerEntry) -> u64 {
        match &self.ledger {
            Some(w) => w.append(entry).unwrap_or(0),
            None => 0,
        }
    }
}
