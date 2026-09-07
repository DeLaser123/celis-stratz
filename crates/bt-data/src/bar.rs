//! Bar model (spec §5). A bar's OHLCV plus its open time; close time derives
//! from the (validated, constant) interval — never from the next row.

use bt_core::time::Ts;
use bt_core::D;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct Bar {
    pub open_time: Ts,
    pub open: D,
    pub high: D,
    pub low: D,
    pub close: D,
    pub volume: Option<D>,
}

impl Bar {
    /// Decision timestamp of this bar: the moment the bar is complete.
    pub fn close_time(&self, interval_secs: i64) -> Ts {
        self.open_time + chrono::Duration::seconds(interval_secs)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BarSeries {
    pub symbol: String,
    pub interval_secs: i64,
    /// Strictly increasing open times.
    pub bars: Vec<Bar>,
}

impl BarSeries {
    pub fn close_time_of(&self, i: usize) -> Ts {
        self.bars[i].close_time(self.interval_secs)
    }

    pub fn last_index(&self) -> Option<usize> {
        if self.bars.is_empty() {
            None
        } else {
            Some(self.bars.len() - 1)
        }
    }
}

/// Fully validated dataset: one series per symbol, plus provenance hashes
/// and the validation report. Series are in a BTreeMap for deterministic order.
#[derive(Debug, Clone, Serialize)]
pub struct Dataset {
    pub series: BTreeMap<String, BarSeries>,
    /// Corporate actions applied by the engine at the ex-date bar (optional;
    /// empty unless a corporate-actions CSV was loaded).
    pub actions: Vec<crate::actions::CorporateAction>,
    pub base_interval_secs: i64,
    /// SHA-256 of the raw file bytes.
    pub data_hash: String,
    /// SHA-256 of the canonical normalized content.
    pub normalized_hash: String,
    pub report: super::validate::ValidationReport,
}

impl Dataset {
    pub fn symbols(&self) -> Vec<String> {
        self.series.keys().cloned().collect()
    }

    /// Canonical content hash of the normalized bars (order-independent).
    /// Used by the harness to hash scenario-transformed datasets.
    pub fn normalized_hash(&self) -> String {
        let bars: BTreeMap<String, Vec<Bar>> = self
            .series
            .iter()
            .map(|(k, v)| (k.clone(), v.bars.clone()))
            .collect();
        crate::csv::compute_normalized_hash(&bars, self.base_interval_secs)
    }
}
