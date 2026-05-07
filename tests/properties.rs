//! Property-based tests for security-critical primitives.
//!
//! Covers:
//!
//! 1. **JCS canonicalization** (`serde_jcs`, RFC 8785) — mandate JWS
//!    validation requires the exact same canonical bytes on signer and
//!    verifier. If JCS were ever non-deterministic or sensitive to input
//!    key ordering, signatures would silently fail to validate. These
//!    properties protect against that.
//!
//! 2. **Mandate budget arithmetic** (`MandateLedger`) — the budget gate
//!    is the user-visible spending control surface. Conservation
//!    (no money disappears, no money is double-spent) and no-overspend
//!    are invariants we cannot get wrong.

use std::collections::BTreeMap;

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use proptest::prelude::*;
use serde_json::{json, Value};
use stateset_icp_handler::mandate::{MandateLedger, MandateSpendLimits};

// ---------------------------------------------------------------------------
// JCS canonicalization properties
// ---------------------------------------------------------------------------

/// Generates a small, finite, JSON value tree. Bounded depth keeps the
/// strategy fast enough to run in the default 256-case proptest budget;
/// the canonicalizer's behavior is structural so deep trees would not add
/// coverage.
fn arb_json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        // Strings: ASCII-only to avoid surrogate pair edge cases that the
        // serde_json parser handles (we are not testing parser unicode).
        "[ -~]{0,16}".prop_map(Value::String),
    ];
    leaf.prop_recursive(
        4,  // up to 4 levels deep
        16, // up to 16 total nodes
        4,  // each collection has up to 4 children
        |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                // Object keys are short ASCII; serde_jcs sorts them
                // lexicographically, so duplicate keys would already have
                // been collapsed by serde_json on parse.
                prop::collection::btree_map("[a-zA-Z0-9_]{1,8}", inner, 0..4)
                    .prop_map(|map| { Value::Object(serde_json::Map::from_iter(map)) }),
            ]
        },
    )
}

proptest! {
    /// JCS is deterministic: the same value always encodes to the same bytes.
    #[test]
    fn jcs_is_deterministic(value in arb_json_value()) {
        let a = serde_jcs::to_vec(&value).expect("jcs a");
        let b = serde_jcs::to_vec(&value).expect("jcs b");
        prop_assert_eq!(a, b);
    }

    /// JCS output is independent of source key order: an object built
    /// from a `BTreeMap` (sorted) and the same logical object built from
    /// the reverse-sorted entries must canonicalize to the same bytes.
    /// This is the property that makes detached-signature verification
    /// possible across implementations that emit keys in different orders.
    #[test]
    fn jcs_is_key_order_independent(
        keys in prop::collection::btree_set("[a-z]{1,6}", 1..8),
        values in prop::collection::vec(any::<i64>(), 1..8),
    ) {
        // Build two Value::Object instances with identical content but
        // populated in opposite orders. serde_json::Map preserves insertion
        // order, so this is a meaningful test of JCS sorting (not just of
        // serde_json's internal ordering).
        let pairs: Vec<(String, i64)> = keys
            .iter()
            .cloned()
            .zip(values.iter().cycle().copied())
            .collect();

        let mut forward = serde_json::Map::new();
        for (k, v) in pairs.iter() {
            forward.insert(k.clone(), json!(v));
        }
        let mut reverse = serde_json::Map::new();
        for (k, v) in pairs.iter().rev() {
            reverse.insert(k.clone(), json!(v));
        }

        let a = serde_jcs::to_vec(&Value::Object(forward)).expect("jcs forward");
        let b = serde_jcs::to_vec(&Value::Object(reverse)).expect("jcs reverse");
        prop_assert_eq!(a, b);
    }

    /// Round-trip through JSON parsing preserves canonical bytes:
    /// `jcs(parse(jcs(v))) == jcs(v)`. If this fails it means the
    /// canonical encoding produces JSON that the parser then re-shapes,
    /// which would break detached-signature verification.
    #[test]
    fn jcs_roundtrip_through_serde_json_is_stable(value in arb_json_value()) {
        let canon = serde_jcs::to_vec(&value).expect("jcs initial");
        let reparsed: Value = serde_json::from_slice(&canon).expect("parse canonical bytes");
        let recanon = serde_jcs::to_vec(&reparsed).expect("jcs reparsed");
        prop_assert_eq!(canon, recanon);
    }

    /// A JCS-canonicalized object's byte representation is invariant
    /// under rebuilding the object from a sorted BTreeMap of its entries.
    /// This is a stronger version of key-order independence: it asserts
    /// that the canonical form genuinely depends only on the (key, value)
    /// set, not on any container choices.
    #[test]
    fn jcs_object_depends_only_on_entries(
        entries in prop::collection::btree_map("[a-z]{1,6}", any::<i64>(), 1..6),
    ) {
        let map = serde_json::Map::from_iter(entries.iter().map(|(k, v)| (k.clone(), json!(v))));
        let direct = serde_jcs::to_vec(&Value::Object(map)).expect("jcs direct");

        // Reconstruct via BTreeMap to ensure sorted-order traversal then
        // re-emit. Should match.
        let btree: BTreeMap<&String, &i64> = entries.iter().collect();
        let mut rebuilt = serde_json::Map::new();
        for (k, v) in btree {
            rebuilt.insert(k.clone(), json!(v));
        }
        let via_btree = serde_jcs::to_vec(&Value::Object(rebuilt)).expect("jcs via btree");
        prop_assert_eq!(direct, via_btree);
    }
}

// ---------------------------------------------------------------------------
// Mandate budget arithmetic properties
// ---------------------------------------------------------------------------

fn limits(budget_minor: i64, per_txn: Option<i64>) -> MandateSpendLimits {
    MandateSpendLimits {
        budget_minor,
        per_transaction: per_txn,
        window: ChronoDuration::days(1),
    }
}

proptest! {
    /// Sum-of-spends conservation: any sequence of accepted spends sums
    /// exactly to `usage().spent_minor`. No money created, none destroyed.
    #[test]
    fn budget_conserved_across_accepted_spends(
        budget_minor in 0i64..1_000_000_000,
        spends in prop::collection::vec(0i64..1_000_000, 0..32),
    ) {
        let ledger = MandateLedger::in_memory();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let jti = "prop-conservation";
        let lim = limits(budget_minor, None);

        let mut accepted_total: i64 = 0;
        for amount in spends {
            match ledger.try_record_spend_checked(jti, "tenant-a", amount, now, lim) {
                Ok(()) if amount > 0 => accepted_total = accepted_total.saturating_add(amount),
                Ok(()) => {} // amount <= 0 is a no-op by contract
                Err(_) => {} // rejected — conservation still holds
            }
        }

        let observed = ledger.usage(jti).spent_minor;
        prop_assert_eq!(observed, accepted_total);
    }

    /// No-overspend: under any sequence of attempted spends, the
    /// accumulated ledger total never exceeds the budget. This is the
    /// principal-protection invariant the budget gate exists to enforce.
    #[test]
    fn budget_never_exceeded(
        budget_minor in 0i64..1_000_000_000,
        spends in prop::collection::vec(0i64..1_000_000, 0..64),
    ) {
        let ledger = MandateLedger::in_memory();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let jti = "prop-no-overspend";
        let lim = limits(budget_minor, None);

        for amount in spends {
            // Ignore Result — we are asserting the invariant holds
            // regardless of accept/reject mix.
            let _ = ledger.try_record_spend_checked(jti, "tenant-a", amount, now, lim);
            prop_assert!(ledger.usage(jti).spent_minor <= budget_minor);
        }
    }

    /// Per-transaction cap is hard: a single spend strictly greater than
    /// the cap must be rejected, regardless of remaining budget.
    #[test]
    fn per_transaction_cap_is_strict(
        budget_minor in 1i64..1_000_000_000,
        per_txn in 1i64..1_000_000,
        amount in 1i64..1_000_000_000,
    ) {
        let ledger = MandateLedger::in_memory();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let jti = "prop-per-txn";
        let lim = limits(budget_minor, Some(per_txn));

        let result = ledger.try_record_spend_checked(jti, "tenant-a", amount, now, lim);
        if amount > per_txn {
            prop_assert!(result.is_err(), "spend {amount} > per_txn {per_txn} should reject");
        }
    }

    /// Non-positive spends are no-ops: they never advance the ledger.
    #[test]
    fn non_positive_spends_are_noops(
        budget_minor in 0i64..1_000_000,
        amount in i64::MIN..=0i64,
    ) {
        let ledger = MandateLedger::in_memory();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let jti = "prop-noop";
        let lim = limits(budget_minor, None);

        let before = ledger.usage(jti).spent_minor;
        let result = ledger.try_record_spend_checked(jti, "tenant-a", amount, now, lim);
        let after = ledger.usage(jti).spent_minor;

        prop_assert!(result.is_ok());
        prop_assert_eq!(before, after);
    }

    /// Window reset: once the budget window elapses, the ledger lets a
    /// fresh full-budget spend through even if the prior window was
    /// fully consumed. (This is what makes a daily mandate actually
    /// daily, rather than lifetime.)
    #[test]
    fn window_reset_clears_prior_spend(
        budget_minor in 100i64..1_000_000,
    ) {
        let ledger = MandateLedger::in_memory();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let jti = "prop-window";
        let lim = limits(budget_minor, None);

        // Exhaust the budget at t0.
        let r = ledger.try_record_spend_checked(jti, "tenant-a", budget_minor, t0, lim);
        prop_assert!(r.is_ok());
        prop_assert_eq!(ledger.usage(jti).spent_minor, budget_minor);

        // Within the window, a further spend must be rejected.
        let r = ledger.try_record_spend_checked(jti, "tenant-a", 1, t0, lim);
        prop_assert!(r.is_err());

        // Past the window (24h + 1s), the same spend must succeed.
        let t1 = t0 + ChronoDuration::days(1) + ChronoDuration::seconds(1);
        let r = ledger.try_record_spend_checked(jti, "tenant-a", budget_minor, t1, lim);
        prop_assert!(r.is_ok());
        prop_assert_eq!(ledger.usage(jti).spent_minor, budget_minor);
    }
}
