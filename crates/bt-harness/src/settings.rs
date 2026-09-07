//! Harness settings (.stratz/config.toml). Holds AI defaults and run defaults.
//! The API key is NEVER stored here — only a key SOURCE may be referenced
//! indirectly; actual keys live in flags, environment, or the OS keyring.

use bt_core::error::{CoreError, CoreResult};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HarnessSettings {
    #[serde(default)]
    pub ai: AiSettings,
    #[serde(default)]
    pub runs: RunSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiSettings {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default = "default_repair_rounds")]
    pub max_repair_rounds: u32,
    #[serde(default = "default_budget")]
    pub budget_tokens_per_command: u64,
}

fn default_provider() -> String {
    "zai".into()
}
fn default_model() -> String {
    "glm-5.3-flash".into()
}
fn default_max_tokens() -> u32 {
    4096
}
fn default_temperature() -> f64 {
    0.0
}
fn default_repair_rounds() -> u32 {
    3
}
fn default_budget() -> u64 {
    200_000
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            provider: default_provider(),
            model: default_model(),
            base_url: None,
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
            max_repair_rounds: default_repair_rounds(),
            budget_tokens_per_command: default_budget(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSettings {
    #[serde(default = "default_capital")]
    pub default_starting_capital: String,
}

fn default_capital() -> String {
    "100000".into()
}

impl Default for RunSettings {
    fn default() -> Self {
        RunSettings {
            default_starting_capital: default_capital(),
        }
    }
}

impl HarnessSettings {
    pub fn load(path: &Path) -> CoreResult<HarnessSettings> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CoreError::InvalidData(format!("read {}: {e}", path.display())))?;
        if text.trim().is_empty() {
            return Ok(HarnessSettings::default());
        }
        toml::from_str(&text)
            .map_err(|e| CoreError::ConfigError(format!("settings {}: {e}", path.display())))
    }

    pub fn to_gateway_config(&self) -> bt_ai::GatewayConfig {
        bt_ai::GatewayConfig {
            provider: self.ai.provider.clone(),
            model: self.ai.model.clone(),
            base_url: self.ai.base_url.clone(),
            temperature: self.ai.temperature,
            max_tokens: self.ai.max_tokens,
            budget_tokens_per_command: self.ai.budget_tokens_per_command,
            max_repair_rounds: self.ai.max_repair_rounds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_template_defaults() {
        let s: HarnessSettings = toml::from_str(
            r#"
[ai]
provider = "zai"
model = "glm-5.3-flash"
base_url = ""
max_tokens = 4096
temperature = 0.0
max_repair_rounds = 3
budget_tokens_per_command = 200000

[runs]
default_starting_capital = "100000"
"#,
        )
        .unwrap();
        assert_eq!(s.ai.model, "glm-5.3-flash");
        assert_eq!(s.ai.max_repair_rounds, 3);
        let gw = s.to_gateway_config();
        assert_eq!(gw.effective_base_url(), bt_ai::provider::ZAI_BASE_URL);
    }

    #[test]
    fn rejects_unknown_fields() {
        let r: Result<HarnessSettings, _> = toml::from_str("[ai]\ntypo_key = 1\n");
        assert!(r.is_err());
    }
}
