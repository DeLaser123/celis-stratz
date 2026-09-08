//! bt-data — market data ingestion, validation, and resampling.
pub mod actions;
pub mod bar;
pub mod csv;
pub mod dukascopy;
pub mod parquet_io;
pub mod resample;
pub mod validate;

pub use actions::{load_corporate_actions_csv, CorporateAction, CorporateActionKind};
pub use bar::{Bar, BarSeries, Dataset};
pub use csv::{LoadLimits, TimestampConvention, ValidationMode};
pub use validate::{IssueCode, ValidationIssue, ValidationReport};
