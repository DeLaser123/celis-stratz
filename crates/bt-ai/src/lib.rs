//! bt-ai — the LLM gateway layer.
//!
//! Hard boundary rule: AI NEVER enters the simulation loop. AI participates
//! ONLY at the boundaries — before runs (mechanizing natural-language
//! strategies into specs that the kernel's own compiler validates) and after
//! runs (grounded analysis of artifacts). Every call is recorded in an audit
//! ledger; no secrets ever reach artifacts or logs.

pub mod cache;
pub mod compile;
pub mod gateway;
pub mod keys;
pub mod ledger;
pub mod prompts;
pub mod provider;

pub use cache::ResponseCache;
pub use compile::{compile_strategy, CompileOutcome, DryRunReport};
pub use gateway::{Gateway, GatewayConfig, GatewayOutcome};
pub use keys::{clear_key, key_source_label, resolve_api_key, store_key, KEYRING_SERVICE};
pub use ledger::{LedgerEntry, LedgerWriter};
pub use provider::{CompletionRequest, CompletionResponse, Message, Provider, Usage};
