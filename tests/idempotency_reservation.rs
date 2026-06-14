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

/// Assert a reserve won the claim and return its fencing token.
fn won(outcome: ReserveOutcome) -> String {
    match outcome {
        ReserveOutcome::Won(token) => token,
        other => panic!("expected Won, got {other:?}"),
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
    let token = won(a.reserve("t", "k", "dig", now, lease));

    // Process B, hitting the same shared DB, sees an in-progress claim — it
    // must NOT also get Won (that would be the double-charge).
    assert!(
        matches!(
            b.reserve("t", "k", "dig", now, lease),
            ReserveOutcome::InProgress
        ),
        "second claimant must observe the in-progress reservation"
    );

    // A finishes and records its response (using its token).
    a.store("t", "k", "dig", completed(br#"{"ok":1}"#), now, &token);

    // B now replays A's response instead of executing.
    match b.reserve("t", "k", "dig", now, lease) {
        ReserveOutcome::Replay(c) => assert_eq!(c.body_json, br#"{"ok":1}"#),
        other => panic!("want Replay after completion, got {other:?}"),
    }
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

    let token = won(a.reserve("t", "k", "digest-A", now, lease));
    a.store("t", "k", "digest-A", completed(br#"{"a":1}"#), now, &token);

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

    let token = won(store.reserve("t", "k", "dig", now, lease));
    // Execution failed — release so a retry isn't blocked for the lease.
    store.release("t", "k", &token);
    assert!(
        matches!(
            store.reserve("t", "k", "dig", now, lease),
            ReserveOutcome::Won(_)
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
    let _a_token = won(a.reserve("t", "k", "dig", now, lease));

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
            ReserveOutcome::Won(_)
        ),
        "an expired reservation must be reclaimable after the lease"
    );

    cleanup(&path);
}

#[test]
fn superseded_holder_cannot_overwrite_the_takers_response() {
    // Fencing: A claims, is slow; past the lease B takes over and completes.
    // A's later store (with A's stale token) must NOT clobber B's response.
    let path = scratch_db_path();
    let a = IdempotencyStore::with_pool(state_db::open(&path).expect("pool a"));
    let b = IdempotencyStore::with_pool(state_db::open(&path).expect("pool b"));
    let now = Utc::now();
    let lease = Duration::seconds(30);

    let a_token = won(a.reserve("t", "k", "dig", now, lease));
    // B takes over after the lease and completes with its own response.
    let later = now + Duration::seconds(31);
    let b_token = won(b.reserve("t", "k", "dig", later, lease));
    assert_ne!(a_token, b_token, "takeover must mint a fresh token");
    b.store(
        "t",
        "k",
        "dig",
        completed(br#"{"winner":"B"}"#),
        later,
        &b_token,
    );

    // A finally finishes and tries to store with its stale token — no-op,
    // and the takeover metric must record the rejected write.
    let takeovers_before = stateset_icp_handler::metrics::IDEMPOTENCY_TAKEOVERS.get();
    a.store(
        "t",
        "k",
        "dig",
        completed(br#"{"winner":"A"}"#),
        later,
        &a_token,
    );
    assert!(
        stateset_icp_handler::metrics::IDEMPOTENCY_TAKEOVERS.get() > takeovers_before,
        "a superseded write must bump the takeover metric"
    );

    // The cache holds B's response, not A's.
    match b.lookup("t", "k", "dig", later) {
        LookupOutcome::Replay(c) => assert_eq!(
            c.body_json, br#"{"winner":"B"}"#,
            "superseded holder must not overwrite the taker's cached response"
        ),
        other => panic!("want Replay of B's response, got {other:?}"),
    }

    cleanup(&path);
}

#[test]
fn in_memory_reservation_round_trips() {
    // Single-process (in-memory) parity: claim → complete → replay.
    let store = IdempotencyStore::in_memory();
    let now = Utc::now();
    let lease = Duration::seconds(60);

    let token = won(store.reserve("t", "k", "dig", now, lease));
    store.store("t", "k", "dig", completed(br#"{"ok":1}"#), now, &token);
    match store.reserve("t", "k", "dig", now, lease) {
        ReserveOutcome::Replay(c) => assert_eq!(c.body_json, br#"{"ok":1}"#),
        other => panic!("want Replay, got {other:?}"),
    }
}
