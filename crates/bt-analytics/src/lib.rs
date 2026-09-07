//! bt-analytics — performance metrics, distributions, and Monte Carlo.
pub mod mc;
pub mod metrics;
pub mod stats;

pub use metrics::{AnalyticsConfig, MetricsReport};
