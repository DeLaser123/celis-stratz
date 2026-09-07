//! bt-harness — the enterprise orchestration layer.
//!
//! The bt-* kernel computes deterministic, auditable runs. THIS crate owns
//! everything that turns runs into an enterprise workflow, and it is the only
//! place these exist: the folder-native project contract, the experiment
//! registry, parameter sweeps, walk-forward analysis, overfitting robustness
//! statistics (Deflated Sharpe, PBO/CSCV), the query engine, and HTML
//! reporting. Without the harness you get a correct backtest; with it you get
//! the research workflow.
//!
//! Boundary rule: nothing here ever mutates the kernel's simulation
//! semantics. Sweeps and scenarios are deterministic input variants.

pub mod html;
pub mod project;
pub mod query;
pub mod registry;
pub mod robust;
pub mod runner;
pub mod settings;
pub mod stress;
pub mod sweep;
pub mod walkforward;

pub use project::{DataFile, DoctorCheck, DoctorReport, Project, StrategyVersion};

/// Convert a serde_yaml value into a serde_json value (params_json storage).
pub fn yaml_to_json_value(v: &serde_yaml::Value) -> serde_json::Value {
    match v {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::json!(i)
            } else if let Some(u) = n.as_u64() {
                serde_json::json!(u)
            } else {
                n.as_f64()
                    .map(|f| serde_json::json!(f))
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            serde_json::Value::Array(seq.iter().map(yaml_to_json_value).collect())
        }
        serde_yaml::Value::Mapping(m) => {
            let mut map = serde_json::Map::new();
            for (k, v) in m {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => yaml_to_json_value(other).to_string(),
                };
                map.insert(key, yaml_to_json_value(v));
            }
            serde_json::Value::Object(map)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json_value(&t.value),
    }
}
pub use registry::{Registry, RegistryRun, RunKind};
pub use settings::HarnessSettings;
pub use stress::{ScenarioSpec, StressReport, StressRow};
pub use sweep::{SweepConfig, SweepReport, SweepRow};
pub use walkforward::{WalkForwardConfig, WalkForwardReport};
