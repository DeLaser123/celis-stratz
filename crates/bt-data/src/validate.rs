//! Validation report (spec §5). Every issue is reported; nothing is repaired
//! silently. Strict mode fails; permissive mode fixes in a documented order
//! and keeps the full issue list.

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    SchemaError,
    UnparseableTimestamp,
    AmbiguousLocalTime,
    MissingValue,
    InvalidNumber,
    NonPositivePrice,
    NegativeVolume,
    OhlcViolation,
    DuplicateTimestamp,
    OutOfOrder,
    MixedInterval,
    RowLimitExceeded,
}

impl IssueCode {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueCode::SchemaError => "SCHEMA_ERROR",
            IssueCode::UnparseableTimestamp => "UNPARSEABLE_TIMESTAMP",
            IssueCode::AmbiguousLocalTime => "AMBIGUOUS_LOCAL_TIME",
            IssueCode::MissingValue => "MISSING_VALUE",
            IssueCode::InvalidNumber => "INVALID_NUMBER",
            IssueCode::NonPositivePrice => "NON_POSITIVE_PRICE",
            IssueCode::NegativeVolume => "NEGATIVE_VOLUME",
            IssueCode::OhlcViolation => "OHLC_VIOLATION",
            IssueCode::DuplicateTimestamp => "DUPLICATE_TIMESTAMP",
            IssueCode::OutOfOrder => "OUT_OF_ORDER",
            IssueCode::MixedInterval => "MIXED_INTERVAL",
            IssueCode::RowLimitExceeded => "ROW_LIMIT_EXCEEDED",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationIssue {
    pub row: usize,
    pub ts: Option<String>,
    pub code: IssueCode,
    pub detail: String,
    /// True if the issue invalidates the row (dropped) or aborts strict mode.
    pub fatal: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
    pub rows_read: usize,
    pub rows_kept: usize,
}

impl ValidationReport {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for i in &self.issues {
            *m.entry(i.code.as_str()).or_insert(0) += 1;
        }
        m
    }

    /// Human-readable summary used by CLI errors and experiment metadata.
    pub fn summary(&self) -> String {
        if self.is_clean() {
            return format!("{} rows validated, no issues", self.rows_kept);
        }
        let counts = self
            .counts()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} rows read, {} kept, issues: {counts} (first: {})",
            self.rows_read,
            self.rows_kept,
            self.issues
                .first()
                .map(|i| format!("{} at row {}", i.code.as_str(), i.row))
                .unwrap_or_default()
        )
    }
}

/// Check OHLC invariants (spec §5). Returns a detail string on violation.
pub fn check_ohlc(
    open: rust_decimal::Decimal,
    high: rust_decimal::Decimal,
    low: rust_decimal::Decimal,
    close: rust_decimal::Decimal,
) -> Result<(), String> {
    if high < open || high < close || high < low {
        return Err(format!("high {high} < max(open, close, low)"));
    }
    if low > open || low > close || low > high {
        return Err(format!("low {low} > min(open, close, high)"));
    }
    Ok(())
}
