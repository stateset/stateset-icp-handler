//! Phase 0 persistence tests — proof that handler state survives restart.
//!
//! Each test writes to a file-backed SQLite state DB, drops the store
//! handles entirely (simulating a handler crash or pod bounce), then
//! reopens against the same DB path and verifies the state is intact.
//!
//! If any of these fail, a 24-hour budget mandate that was half-spent
//! before the restart would allow unbounded further spend in the
//! remaining window — the exact condition that disqualifies in-memory
//! state from v1.0.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use stateset_icp_handler::{
    mandate::MandateLedger,
    models::{Buyer, Totals, Transaction, TransactionState},
    receipts::{ReceiptStore, StoredReceipt},
    signing::{ReceiptClaims, ReceiptIcp},
    state_db,
    state_store::TransactionStore,
};

fn scratch_db_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "icp-state-test-{}.db",
        uuid::Uuid::new_v4().simple()
    ))
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

// --------------------------------------------------------------------------
// MandateLedger — the security-critical case
// --------------------------------------------------------------------------

#[test]
fn mandate_spend_survives_reopen() {
    let path = scratch_db_path();
    let path_str = path.to_string_lossy().to_string();
    let jti = "mandate-persist-1";
    let first_spend_at = Utc::now();

    {
        let pool = state_db::open(&path_str).expect("open pool 1");
        let ledger = MandateLedger::with_pool(pool);
        ledger.record_spend(jti, 2500, first_spend_at);
        ledger.record_spend(jti, 3000, first_spend_at + Duration::seconds(5));
        let usage = ledger.usage(jti);
        assert_eq!(usage.spent_minor, 5500);
        assert!(usage.window_start.is_some());
    }
    // Pool dropped — simulates handler shutdown. WAL checkpoints should
    // have flushed on connection close.

    {
        let pool = state_db::open(&path_str).expect("open pool 2");
        let ledger = MandateLedger::with_pool(pool);
        let usage = ledger.usage(jti);
        assert_eq!(
            usage.spent_minor, 5500,
            "mandate spend must survive restart — otherwise remaining budget is \
             unbounded on the second handler's first evaluate() call"
        );
        assert!(
            usage.window_start.is_some(),
            "window_start must survive restart — otherwise lazy-reset logic in \
             evaluate() reads zero and the mandate is effectively re-issued"
        );
    }

    cleanup(&path);
}

#[test]
fn mandate_spend_accumulates_across_reopen() {
    let path = scratch_db_path();
    let path_str = path.to_string_lossy().to_string();
    let jti = "mandate-persist-2";
    let now = Utc::now();

    {
        let pool = state_db::open(&path_str).expect("open pool 1");
        let ledger = MandateLedger::with_pool(pool);
        ledger.record_spend(jti, 1000, now);
    }
    {
        let pool = state_db::open(&path_str).expect("open pool 2");
        let ledger = MandateLedger::with_pool(pool);
        ledger.record_spend(jti, 2500, now + Duration::seconds(10));
        let usage = ledger.usage(jti);
        assert_eq!(usage.spent_minor, 3500);
    }

    cleanup(&path);
}

#[test]
fn mandate_usage_empty_for_unknown_jti() {
    let pool = state_db::open(":memory:").expect("open in-memory");
    let ledger = MandateLedger::with_pool(pool);
    let usage = ledger.usage("never-seen");
    assert_eq!(usage.spent_minor, 0);
    assert!(usage.window_start.is_none());
}

// --------------------------------------------------------------------------
// ReceiptStore — required for GET /icp/v1/receipts/:jti after restart
// --------------------------------------------------------------------------

#[test]
fn receipt_survives_reopen() {
    let path = scratch_db_path();
    let path_str = path.to_string_lossy().to_string();
    let jti = "receipt-persist-1";

    let claims = ReceiptClaims {
        iss: "icp://test-handler".into(),
        aud: "did:stateset:agent:a".into(),
        iat: Utc::now().timestamp(),
        jti: jti.into(),
        icp: ReceiptIcp {
            version: "2026-04-21".into(),
            intent: "intent.buy".into(),
            transaction_id: "txn-receipt-1".into(),
            order_id: Some("ORD-42".into()),
            mandate_jti: Some("m1".into()),
            body_digest: "sha-256=aaa".into(),
            body_canonicalization: "JCS".into(),
        },
    };

    {
        let pool = state_db::open(&path_str).expect("open pool 1");
        let store = ReceiptStore::with_pool(pool);
        store.insert(StoredReceipt {
            jti: jti.into(),
            kid: "test-kid".into(),
            jws: "header.payload.signature".into(),
            body_digest: "sha-256=aaa".into(),
            claims: claims.clone(),
        });
        assert_eq!(store.len(), 1);
    }

    {
        let pool = state_db::open(&path_str).expect("open pool 2");
        let store = ReceiptStore::with_pool(pool);
        let got = store
            .get(jti)
            .expect("receipt must be retrievable after handler restart");
        assert_eq!(got.jti, jti);
        assert_eq!(got.kid, "test-kid");
        assert_eq!(got.jws, "header.payload.signature");
        assert_eq!(got.body_digest, "sha-256=aaa");
        assert_eq!(got.claims.icp.transaction_id, "txn-receipt-1");
        assert_eq!(got.claims.icp.order_id.as_deref(), Some("ORD-42"));
    }

    cleanup(&path);
}

#[test]
fn receipt_missing_returns_none() {
    let pool = state_db::open(":memory:").expect("open in-memory");
    let store = ReceiptStore::with_pool(pool);
    assert!(store.get("never-signed").is_none());
    assert!(store.is_empty());
}

// --------------------------------------------------------------------------
// TransactionStore — exercises the generic JsonStore update() path
// --------------------------------------------------------------------------

fn minimal_txn(id: &str) -> Transaction {
    let now = Utc::now();
    Transaction {
        id: id.into(),
        state: TransactionState::Draft,
        agent_id: "did:stateset:agent:a".into(),
        mandate_jti: None,
        currency: "USD".into(),
        jurisdiction: None,
        buyer: Buyer {
            first_name: None,
            last_name: None,
            email: None,
            phone_number: None,
            principal_did: None,
        },
        ship_to: None,
        bill_to: None,
        line_items: vec![],
        totals: Totals {
            subtotal: None,
            discount: None,
            shipping: None,
            tax: None,
            total: None,
        },
        order_id: None,
        quote_expires_at: None,
        created_at: now,
        updated_at: now,
        external_refs: Default::default(),
    }
}

#[test]
fn transaction_survives_reopen() {
    let path = scratch_db_path();
    let path_str = path.to_string_lossy().to_string();
    let id = "txn-persist-1";

    {
        let pool = state_db::open(&path_str).expect("open pool 1");
        let store = TransactionStore::with_pool(pool);
        store.insert(minimal_txn(id));
    }

    {
        let pool = state_db::open(&path_str).expect("open pool 2");
        let store = TransactionStore::with_pool(pool);
        let got = store.get(id).expect("transaction must survive restart");
        assert_eq!(got.id, id);
        assert!(matches!(got.state, TransactionState::Draft));
    }

    cleanup(&path);
}

#[test]
fn transaction_update_closure_persists() {
    let path = scratch_db_path();
    let path_str = path.to_string_lossy().to_string();
    let id = "txn-update-1";

    {
        let pool = state_db::open(&path_str).expect("open pool 1");
        let store = TransactionStore::with_pool(pool);
        store.insert(minimal_txn(id));
        let updated = store
            .update(id, |t| {
                t.state = TransactionState::Quoted;
                t.order_id = Some("ORD-7".into());
            })
            .expect("update should find inserted txn");
        assert!(matches!(updated.state, TransactionState::Quoted));
        assert_eq!(updated.order_id.as_deref(), Some("ORD-7"));
    }

    {
        let pool = state_db::open(&path_str).expect("open pool 2");
        let store = TransactionStore::with_pool(pool);
        let got = store.get(id).expect("updated txn must survive");
        assert!(matches!(got.state, TransactionState::Quoted));
        assert_eq!(got.order_id.as_deref(), Some("ORD-7"));
    }

    cleanup(&path);
}

#[test]
fn transaction_update_missing_returns_none() {
    let pool = state_db::open(":memory:").expect("open in-memory");
    let store = TransactionStore::with_pool(pool);
    let result = store.update("not-there", |_t| {});
    assert!(result.is_none());
}

// --------------------------------------------------------------------------
// In-memory isolation — separate open() calls must not share state
// --------------------------------------------------------------------------

#[test]
fn in_memory_pools_are_isolated() {
    let jti = "m-iso";
    let now = Utc::now();

    let pool_a = state_db::open(":memory:").expect("open a");
    let ledger_a = MandateLedger::with_pool(pool_a);
    ledger_a.record_spend(jti, 1000, now);
    assert_eq!(ledger_a.usage(jti).spent_minor, 1000);

    let pool_b = state_db::open(":memory:").expect("open b");
    let ledger_b = MandateLedger::with_pool(pool_b);
    assert_eq!(
        ledger_b.usage(jti).spent_minor,
        0,
        "two :memory: pools must not share state — each test needs isolation"
    );
}
