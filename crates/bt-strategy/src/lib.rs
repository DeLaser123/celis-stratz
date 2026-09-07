//! bt-strategy — indicator engine, typed expression system, and the
//! machine-readable strategy specification (parse, compile, validate).
pub mod expr;
pub mod indicators;
pub mod runtime;
pub mod spec;

pub use expr::{EvalContext, Value};
pub use runtime::{CompiledStrategy, IndicatorMode, IndicatorRuntime, Intent};
pub use spec::StrategySpec;
