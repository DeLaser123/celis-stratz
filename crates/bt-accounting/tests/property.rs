//! Property-based accounting invariants (spec §33). Randomized fill
//! sequences must never create money, never leave zero-quantity positions,
//! and must keep the ledger conserved.

use bt_accounting::account::Account;
use bt_core::ledger::LedgerEntryType;
use bt_core::{Side, D};
use proptest::prelude::*;
use rust_decimal_macros::dec;

fn ts() -> bt_core::time::Ts {
    let utc: chrono_tz::Tz = "UTC".parse().unwrap();
    bt_core::time::parse_timestamp("2024-01-01T00:00:00Z", utc, "t").unwrap()
}

const INITIAL: i64 = 100_000;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A zero-cost round trip at an identical price must produce exactly zero
    /// P&L (spec §33: no event creates money from nothing).
    #[test]
    fn round_trip_at_same_price_is_zero(qty in 1u32..1000u32, price in 1i64..100000i64) {
        let mut a = Account::new(D::from(INITIAL), dec!(1));
        let qty = D::from(qty);
        let price = D::from(price);
        a.apply_fill(1, "X", Side::Buy, qty, price, price, dec!(0), dec!(0), dec!(0), dec!(1), ts(), "long", None, None, None, "e").unwrap();
        a.apply_fill(2, "X", Side::Sell, qty, price, price, dec!(0), dec!(0), dec!(0), dec!(1), ts(), "long", None, None, None, "x").unwrap();
        prop_assert_eq!(a.balance, D::from(INITIAL));
        prop_assert!(a.positions.is_empty());
    }

    /// Random fill sequences: accounting never errors, no zero-quantity
    /// positions linger, ledger conservation holds, and realized P&L equals
    /// the sum of Trade ledger entries.
    #[test]
    fn random_fills_preserve_invariants(
        ops in proptest::collection::vec((proptest::bool::ANY, 1u32..100u32, -50i64..50i64), 1..40)
    ) {
        let mut a = Account::new(D::from(INITIAL), dec!(1));
        let mut price = D::from(10000);
        let mut oid = 0u64;
        for (sell, qty, dprice) in ops {
            oid += 1;
            price = (price + D::from(dprice)).max(dec!(1));
            let side = if sell { Side::Sell } else { Side::Buy };
            a.apply_fill(
                oid, "X", side, D::from(qty), price, price,
                dec!(0), dec!(0), dec!(0), dec!(1), ts(),
                if sell { "short" } else { "long" },
                None, None, None, "op",
            )
            .unwrap();
            // no zero-quantity position may remain
            for p in a.positions.values() {
                prop_assert!(p.qty != dec!(0), "zero-quantity position must be removed");
            }
        }
        // ledger conservation (tolerance documented in ACCOUNTING.md)
        let ledger_sum: D = a.ledger.iter().map(|e| e.amount).sum();
        let diff = (a.balance - D::from(INITIAL) - ledger_sum).abs();
        prop_assert!(diff <= D::new(1, 12), "conservation diff {diff}");
        // realized total equals sum of Trade entries
        let trade_sum: D = a.ledger.iter().filter(|e| e.entry_type == LedgerEntryType::Trade).map(|e| e.amount).sum();
        prop_assert_eq!(a.total_realized, trade_sum);
    }
}
