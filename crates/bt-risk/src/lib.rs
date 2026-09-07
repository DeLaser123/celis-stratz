//! bt-risk — position sizing and pre-trade risk validation (spec §11).
pub mod engine;

pub use engine::{RiskConfig, RiskContext, RiskDecision, RiskEngine, SizingMode};
