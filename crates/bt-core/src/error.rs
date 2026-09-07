//! Strongly typed errors (spec §34). Never silently continue on a broken
//! financial invariant; every variant is actionable.

use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("invalid timestamp at {context}: {detail}")]
    InvalidTimestamp { context: String, detail: String },
    #[error("duplicate timestamp {0}")]
    DuplicateTimestamp(String),
    #[error("invalid OHLC at {ts}: {detail}")]
    InvalidOhlc { ts: String, detail: String },
    #[error("unknown symbol: {0}")]
    UnknownSymbol(String),
    #[error("invalid order: {0}")]
    InvalidOrder(String),
    #[error("insufficient capital: need {need}, have {have}")]
    InsufficientCapital { need: String, have: String },
    #[error("margin violation: {0}")]
    MarginViolation(String),
    #[error("strategy error: {0}")]
    StrategyError(String),
    #[error("execution ambiguity: {0}")]
    ExecutionAmbiguity(String),
    #[error("accounting invariant violated: {0}")]
    AccountingInvariantViolation(String),
    #[error("risk violation: {0}")]
    RiskViolation(String),
    #[error("unsupported configuration: {0}")]
    UnsupportedConfiguration(String),
    #[error("decimal overflow in {context}")]
    DecimalOverflow { context: String },
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
