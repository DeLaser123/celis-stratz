//! Provider abstraction + the OpenAI-compatible adapter.
//!
//! Z.ai's GLM API is OpenAI-compatible (`POST {base}/chat/completions`), so a
//! single adapter serves Z.ai (default), OpenAI, OpenRouter, vLLM, Ollama and
//! any custom enterprise gateway via `base_url`.

use bt_core::error::{CoreError, CoreResult};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Message {
        Message {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Message {
        Message {
            role: "user".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub temperature: f64,
    pub max_tokens: u32,
    /// Ask the endpoint for JSON-constrained output (best effort; our own
    /// validation is the real gate).
    pub json_mode: bool,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub text: String,
    pub usage: Usage,
}

/// A chat-completion provider. Implementations must be Send + Sync.
pub trait Provider: Send + Sync {
    fn complete(&self, req: &CompletionRequest) -> CoreResult<CompletionResponse>;
    /// Human-readable identifier for the ledger (e.g. "zai").
    fn name(&self) -> &'static str;
}

/// Z.ai default endpoint (GLM family).
pub const ZAI_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

pub struct OpenAiCompatible {
    pub base_url: String,
    pub api_key: String,
    pub timeout: Duration,
}

impl OpenAiCompatible {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        OpenAiCompatible {
            base_url: base_url.into(),
            api_key: api_key.into(),
            timeout: Duration::from_secs(180),
        }
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }
}

#[derive(serde::Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    temperature: f64,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(serde::Deserialize)]
struct WireChoice {
    message: WireMessage,
}

#[derive(serde::Deserialize)]
struct WireMessage {
    content: Option<String>,
}

#[derive(serde::Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

impl Provider for OpenAiCompatible {
    fn name(&self) -> &'static str {
        "openai-compatible"
    }

    fn complete(&self, req: &CompletionRequest) -> CoreResult<CompletionResponse> {
        let wire = WireRequest {
            model: &req.model,
            messages: &req.messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            response_format: if req.json_mode {
                Some(serde_json::json!({ "type": "json_object" }))
            } else {
                None
            },
        };
        let body = serde_json::to_string(&wire)
            .map_err(|e| CoreError::InvalidData(format!("serialize request: {e}")))?;

        let agent: ureq::Agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let resp = agent
            .post(&self.endpoint())
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_string(&body)
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let body = resp.into_string().unwrap_or_default();
                    let brief: String = body.chars().take(500).collect();
                    CoreError::InvalidData(format!(
                        "provider HTTP {code} from {}: {brief}",
                        self.endpoint()
                    ))
                }
                other => CoreError::InvalidData(format!(
                    "provider transport error calling {}: {other}",
                    self.endpoint()
                )),
            })?;

        let text = resp
            .into_string()
            .map_err(|e| CoreError::InvalidData(format!("provider response read: {e}")))?;
        let parsed: WireResponse = serde_json::from_str(&text).map_err(|e| {
            CoreError::InvalidData(format!(
                "provider response is not valid chat-completion JSON: {e}"
            ))
        })?;
        let content = parsed
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| {
                CoreError::InvalidData("provider returned no choices/message content".into())
            })?;
        Ok(CompletionResponse {
            text: content,
            usage: Usage {
                prompt_tokens: parsed
                    .usage
                    .as_ref()
                    .and_then(|u| u.prompt_tokens)
                    .unwrap_or(0),
                completion_tokens: parsed
                    .usage
                    .as_ref()
                    .and_then(|u| u.completion_tokens)
                    .unwrap_or(0),
            },
        })
    }
}

/// Extract the first JSON object from a model response, tolerating code
/// fences and surrounding prose.
pub fn extract_json_object(text: &str) -> CoreResult<String> {
    let cleaned = text.trim();
    let stripped = if let Some(start) = cleaned.find("```") {
        // strip fences: keep the segment between the first fence and the next
        let after = &cleaned[start..];
        let inner_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let inner = &after[inner_start..];
        let end = inner.rfind("```").unwrap_or(inner.len());
        inner[..end].trim()
    } else {
        cleaned
    };
    let start = stripped
        .find('{')
        .ok_or_else(|| CoreError::InvalidData("model response contains no JSON object".into()))?;
    let end = stripped
        .rfind('}')
        .ok_or_else(|| CoreError::InvalidData("model response contains no JSON object".into()))?;
    if end < start {
        return Err(CoreError::InvalidData(
            "model JSON extraction failed".into(),
        ));
    }
    let candidate = &stripped[start..=end];
    // Validate it parses before returning.
    serde_json::from_str::<serde_json::Value>(candidate)
        .map_err(|e| CoreError::InvalidData(format!("model JSON is malformed: {e}")))?;
    Ok(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_fenced_response() {
        let r = "Sure, here is the spec:\n```json\n{\"a\": {\"b\": 1}}\n```\nDone.";
        assert_eq!(extract_json_object(r).unwrap(), r#"{"a": {"b": 1}}"#);
    }

    #[test]
    fn extracts_json_from_prose() {
        let r = "The spec follows: {\"name\": \"x\", \"nested\": {\"v\": [1, 2]}} hope that helps";
        assert!(extract_json_object(r).unwrap().contains("\"nested\""));
    }

    #[test]
    fn rejects_non_json() {
        assert!(extract_json_object("no json here").is_err());
        assert!(extract_json_object("{\"broken\": }").is_err());
    }

    #[test]
    fn endpoint_construction() {
        let p = OpenAiCompatible::new("https://api.z.ai/api/paas/v4/", "k");
        assert_eq!(
            p.endpoint(),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
    }
}
