//! bt-execution — cost models, fill construction, and intrabar trigger logic.
pub mod costs;
pub mod fills;
pub mod intrabar;

pub use costs::{CommissionModel, CostModels, FinancingModel, SlippageModel, SpreadModel};
pub use intrabar::{AmbiguityPolicy, TriggerPriority};
