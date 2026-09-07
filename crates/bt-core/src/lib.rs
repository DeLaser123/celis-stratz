//! bt-core — shared domain model for the backtesting engine.
//!
//! Contains the primitive types every other crate builds on: exact decimal
//! money helpers, the time model, instruments, orders, events, the ledger,
//! deterministic hashing and the seeded PRNG used by Monte Carlo analysis.
//!
//! Nothing in this crate performs I/O or mutates global state.

pub mod error;
pub mod event;
pub mod hash;
pub mod instrument;
pub mod ledger;
pub mod money;
pub mod order;
pub mod prng;
pub mod time;

pub use error::{CoreError, CoreResult};
pub use money::D;
pub use order::Side;
pub use time::{format_ts, parse_interval, parse_timestamp, Ts};
