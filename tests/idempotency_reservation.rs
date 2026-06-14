//! Cross-instance idempotency reservation tests.
//!
//! Two `IdempotencyStore`s backed by the *same* SQLite file model two
//! handler processes sharing one database. The reservation
//! (`reserve`/`release`) is what stops both from executing the same keyed
//! write — the in-process lock alone can't, since each process has its own.

use chrono::{Duration, Utc};
use stateset_icp_handler::idempotency::{
    CachedResponse, IdempotencyStore, LookupOutcome, ReserveOutcome,
};
use stateset_icp_handler::state_db;

fn scratch_db_path() -> String {
    format!(
        "/tmp/icp-idem-reservation-{}.db",
        uuid::Uuid::new_v4().simple()
    )
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

fn completed(body: &[u8]) -> CachedResponse {
    CachedResponse {
        status: 200,
        body_json: body.to_vec(),
    }
}

#[test]
fn cross_instance_reservation_blocks_second_executor_then_replays() {
    let path = scratch_db_path();
    let a = IdempotencyStore::with_pool(state_db::open(&path).expect("pool a"));
    let b = IdempotencyStore::with_pool(state_db::open(&path).expect("pool b"));
    let now = Utc::now();
    let lease = Duration::seconds(60);

    // Process A wins the claim.
    assert!(
        matches!(a.reserve("t", "k", "dig", now, lease), ReserveOutcome::Won),
        "first claimant must win"
    );

    // Process B, hitting the same shared DB, sees an in-progress claim — it
    // must NOT also get Won (that would be the double-charge).
    assert!(
        matches!(
            b.reserve("t", "k", "dig", now, lease),
            ReserveOutcome::InProgress
        ),
        "second claimant must observe the in-progress reservation"
    );

    // A finishes and records its response.
    a.store("t", "k", "dig", completed(br#"{"ok":1}"#), now);

    // B now replays A's response instead of executing.
    match b.reserve("t", "k", "dig", now, lease) {
        ReserveOutcome::Replay(c) => assert_eq!(c.body_json, br#"{"ok":1}"#),
        other => panic!("want Replay after completion, got {other:?}"),
    }
    // And a plain lookup from B replays too.
    assert!(matches!(
        b.lookup("t", "k", "dig", now),
        LookupOutcome::Replay(_)
    ));

    cleanup(&path);
}

#[test]
fn cross_instance_conflict_on_different_body() {
    let path = scratch_db_path();
    let a = IdempotencyStore::with_pool(state_db::open(&path).expect("pool a"));
    let b = IdempotencyStore::with_pool(state_db::open(&path).expect("pool b"));
    let now = Utc::now();
    let lease = Duration::seconds(60);

    assert!(matches!(
        a.reserve("t", "k", "digest-A", now, lease),
        ReserveOutcome::Won
    ));
    a.store("t", "k", "digest-A", completed(br#"{"a":1}"#), now);

    // Same key, different request body → conflict across instances.
    assert!(
        matches!(
            b.reserve("t", "k", "digest-B", now, lease),
            ReserveOutcome::Conflict
        ),
        "reused key with a different body must conflict"
    );

    cleanup(&path);
}

#[test]
fn released_reservation_can_be_reclaimed_immediately() {
    let path = scratch_db_path();
    let store = IdempotencyStore::with_pool(state_db::open(&path).expect("pool"));
    let now = Utc::now();
    let lease = Duration::seconds(60);

    assert!(matches!(
        store.reserve("t", "k", "dig", now, lease),
        ReserveOutcome::Won
    ));
    // Execution failed — release so a retry isn't blocked for the lease.
    store.release("t", "k");
    assert!(
        matches!(
            store.reserve("t", "k", "dig", now, lease),
            ReserveOutcome::Won
        ),
        "released key must be immediately reclaimable"
    );

    cleanup(&path);
}

#[test]
fn stale_reservation_is_reclaimed_after_lease() {
    let path = scratch_db_path();
    let a = IdempotencyStore::with_pool(state_db::open(&path).expect("pool a"));
    let b = IdempotencyStore::with_pool(state_db::open(&path).expect("pool b"));
    let now = Utc::now();
    let lease = Duration::seconds(30);

    // A claims, then "crashes" (never stores, never releases).
    assert!(matches!(
        a.reserve("t", "k", "dig", now, lease),
        ReserveOutcome::Won
    ));

    // Within the lease, B must back off.
    assert!(matches!(
        b.reserve("t", "k", "dig", now + Duration::seconds(5), lease),
        ReserveOutcome::InProgress
    ));

    // Past the lease, the dead holder's claim is reclaimable so the system
    // makes progress (at-least-once recovery).
    assert!(
        matches!(
            b.reserve("t", "k", "dig", now + Duration::seconds(31), lease),
            ReserveOutcome::Won
        ),
        "an expired reservation must be reclaimable after the lease"
    );

    cleanup(&path);
}

#[test]
fn in_memory_reservation_round_trips() {
    // Single-process (in-memory) parity: claim → complete → replay.
    let store = IdempotencyStore::in_memory();
    let now = Utc::now();
    let lease = Duration::seconds(60);

    assert!(matches!(
        store.reserve("t", "k", "dig", now, lease),
        ReserveOutcome::Won
    ));
    store.store("t", "k", "dig", completed(br#"{"ok":1}"#), now);
    match store.reserve("t", "k", "dig", now, lease) {
        ReserveOutcome::Replay(c) => assert_eq!(c.body_json, br#"{"ok":1}"#),
        other => panic!("want Replay, got {other:?}"),
    }
}
