//! Event model (spec §35): the complete, ordered audit trail.
//!
//! Every observable engine transition becomes an `Event` with a strictly
//! increasing `seq` and a timestamp. The JSONL event log lets a user trace any
//! final number backwards through the causal chain.

use crate::error::CoreResult;
use crate::money::D;
use crate::time::Ts;
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
    RunStarted {
        engine_version: String,
        strategy: String,
        symbols: Vec<String>,
        timeframe: String,
        starting_capital: D,
        execution_model: String,
        intrabar_policy: String,
    },
    DataWarning {
        code: String,
        detail: String,
    },
    MarketEvent {
        symbol: String,
        open: D,
        high: D,
        low: D,
        close: D,
        volume: Option<D>,
    },
    StrategySignal {
        action: String,
        detail: String,
    },
    OrderIntent {
        action: String,
        symbol: String,
        quantity: D,
        estimated_price: D,
    },
    OrderCreated {
        order_id: u64,
        symbol: String,
        side: String,
        order_type: String,
        quantity: D,
        reason: String,
    },
    OrderAccepted {
        order_id: u64,
    },
    OrderRejected {
        order_id: u64,
        reason: String,
    },
    OrderActivated {
        order_id: u64,
        ts: Ts,
    },
    OrderFilled {
        order_id: u64,
        fill_id: u64,
        price: D,
        raw_price: D,
        quantity: D,
        commission: D,
        spread_cost: D,
        slippage_cost: D,
    },
    OrderCancelled {
        order_id: u64,
        reason: String,
    },
    PositionOpened {
        symbol: String,
        quantity: D,
        avg_price: D,
        stop: Option<D>,
        target: Option<D>,
    },
    PositionIncreased {
        symbol: String,
        quantity: D,
        avg_price: D,
    },
    PositionReduced {
        symbol: String,
        realized_pnl: D,
    },
    PositionReversed {
        symbol: String,
        new_quantity: D,
    },
    PositionClosed {
        symbol: String,
        realized_pnl: D,
        reason: String,
    },
    AmbiguityDeferred {
        symbol: String,
        detail: String,
    },
    FinancingApplied {
        symbol: String,
        amount: D,
    },
    TrailingStopUpdated {
        symbol: String,
        level: D,
    },
    RiskRejected {
        reason: String,
        detail: String,
    },
    MarginViolation {
        detail: String,
    },
    AccountUpdated {
        balance: D,
        equity: D,
        margin_used: D,
        free_margin: D,
    },
    EquitySnapshot {
        equity: D,
        drawdown: D,
        drawdown_pct: D,
        in_position: bool,
    },
    RunFinished {
        final_equity: D,
        total_events: u64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    pub seq: u64,
    pub ts: Ts,
    #[serde(flatten)]
    pub event: Event,
}

/// Sink for events. The engine pushes; sinks never reorder or filter.
pub trait EventSink {
    fn record(&mut self, rec: EventRecord);
    fn finish(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

pub struct VecSink {
    pub events: Vec<EventRecord>,
}

impl VecSink {
    pub fn new() -> Self {
        VecSink { events: Vec::new() }
    }
}

impl Default for VecSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for VecSink {
    fn record(&mut self, rec: EventRecord) {
        self.events.push(rec);
    }
}

/// Append-only JSONL file sink. Deterministic content for deterministic runs.
pub struct JsonlSink {
    writer: std::io::BufWriter<std::fs::File>,
}

impl JsonlSink {
    pub fn create(path: &Path) -> CoreResult<Self> {
        let file = std::fs::File::create(path)?;
        Ok(JsonlSink {
            writer: std::io::BufWriter::new(file),
        })
    }
}

impl EventSink for JsonlSink {
    fn record(&mut self, rec: EventRecord) {
        let line = serde_json::to_string(&rec)
            .unwrap_or_else(|_| "{\"event\":\"SERIALIZATION_ERROR\"}".to_string());
        let _ = writeln!(self.writer, "{line}");
    }

    fn finish(&mut self) -> Result<(), std::io::Error> {
        self.writer.flush()
    }
}

/// Sequencer shared by the event log and the ledger (single source of order).
#[derive(Debug, Clone)]
pub struct Sequencer {
    next: u64,
}

impl Sequencer {
    pub fn new() -> Self {
        Sequencer { next: 1 }
    }
    pub fn next_seq(&mut self) -> u64 {
        let s = self.next;
        self.next += 1;
        s
    }
    pub fn peek(&self) -> u64 {
        self.next
    }
    pub fn total(&self) -> u64 {
        self.next - 1
    }
}

impl Default for Sequencer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_interval;

    #[test]
    fn events_serialize_with_tag() {
        let utc: chrono_tz::Tz = "UTC".parse().unwrap();
        let ts = crate::time::parse_timestamp("2024-01-01T00:00:00Z", utc, "t").unwrap();
        let _ = parse_interval("1h").unwrap();
        let rec = EventRecord {
            seq: 7,
            ts,
            event: Event::OrderFilled {
                order_id: 3,
                fill_id: 1,
                price: rust_decimal_macros::dec!(1.1000),
                raw_price: rust_decimal_macros::dec!(1.1000),
                quantity: rust_decimal_macros::dec!(10000),
                commission: rust_decimal_macros::dec!(0),
                spread_cost: rust_decimal_macros::dec!(0),
                slippage_cost: rust_decimal_macros::dec!(0),
            },
        };
        let s = serde_json::to_string(&rec).unwrap();
        assert!(s.contains("\"event\":\"ORDER_FILLED\""));
        assert!(s.contains("\"seq\":7"));
        assert!(s.contains("\"price\":\"1.1000\""));
    }
}
