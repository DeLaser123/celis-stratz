//! bt-simulation — the deterministic event-loop engine (reference
//! implementation), experiment metadata, and output writers.
pub mod config;
pub mod engine;
pub mod experiment;
pub mod outputs;
pub mod report;

pub use config::EngineConfig;
pub use engine::{RunResult, SimulationEngine};
pub use outputs::write_outputs;
