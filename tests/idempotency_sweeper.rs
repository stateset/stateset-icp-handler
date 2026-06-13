//! Idempotency cache TTL sweeper.
//!
//! Without active eviction, the `idempotency` table grows unbounded:
//! lazy TTL at lookup time keeps stale entries from being replayed
//! but never reclaims the row. The sweeper closes that gap.
//!
//! Asserts:
//!   * `IdempotencyStore::prune` deletes only entries older than
//!     `now - ttl`; entries within TTL stay.
//!   * Both backends (in-memory + SQLite) behave identically.
//!   * After a sweep removes a stale entry, a follow-up
//!     `lookup` for the same key returns a fresh `Miss` (not
//!     `Conflict`) so the caller can re-store with new content.
//!   * `record_idempotency_sweep` bumps both the prune counter and
//!     the sweeper liveness counter.

use chrono::{Duration, Utc};
use stateset_icp_handler::{
    idempotency::{CachedResponse, IdempotencyStore, LookupOutcome},
    metrics::{IDEMPOTENCY_PRUNED, IDEMPOTENCY_SWEEPER_TICKS},
    state_db,
};

fn store_entry(store: &IdempotencyStore, key: &str, body: &[u8], at: chrono::DateTime<Utc>) {
    let digest = IdempotencyStore::digest_request(body);
    store.store(
        "tenant_a",
        key,
        &digest,
        CachedResponse {
            status: 200,
            body_json: br#"{"ok":true}"#.to_vec(),
        },
        at,
    );
}

// --------------------------------------------------------------------------

#[test]
fn prune_in_memory_deletes_only_expired_entries() {
    let store = IdempotencyStore::in_memory(); // default 24h TTL
    let now = Utc::now();
    // Stale: 25h old → past TTL.
    store_entry(&store, "stale", b"hello", now - Duration::hours(25));
    // Fresh: 1h old → well within TTL.
    store_entry(&store, "fresh", b"world", now - Duration::hours(1));

    assert_eq!(store.len(), 2, "both entries written");
    let pruned = store.prune(now);
    assert_eq!(pruned, 1, "exactly the stale entry should drop");
    assert_eq!(store.len(), 1);

    // Fresh still replays; stale is now a miss.
    let fresh_digest = IdempotencyStore::digest_request(b"world");
    let out = store.lookup("tenant_a", "fresh", &fresh_digest, now);
    assert!(matches!(out, LookupOutcome::Replay(_)));

    let stale_digest = IdempotencyStore::digest_request(b"hello");
    let out = store.lookup("tenant_a", "stale", &stale_digest, now);
    assert!(
        matches!(out, LookupOutcome::Miss),
        "pruned stale key behaves as Miss on lookup"
    );
}

#[test]
fn prune_sqlite_deletes_only_expired_entries() {
    let pool = state_db::open(":memory:").expect("open pool");
    let store = IdempotencyStore::with_pool(pool);
    let now = Utc::now();
    store_entry(&store, "stale", b"hello", now - Duration::hours(25));
    store_entry(&store, "fresh", b"world", now - Duration::hours(1));

    assert_eq!(store.len(), 2);
    let pruned = store.prune(now);
    assert_eq!(
        pruned, 1,
        "SQLite prune must match in-memory semantics — exactly one row deleted"
    );
    assert_eq!(store.len(), 1);
}

#[test]
fn prune_with_no_expired_entries_is_a_noop() {
    let store = IdempotencyStore::in_memory();
    let now = Utc::now();
    store_entry(&store, "k1", b"a", now - Duration::minutes(30));
    store_entry(&store, "k2", b"b", now - Duration::hours(2));

    assert_eq!(store.prune(now), 0, "nothing past 24h TTL");
    assert_eq!(store.len(), 2);
}

#[test]
fn pruned_then_relookup_returns_miss_not_conflict() {
    // Property the sweeper buys you: after eviction, re-using the
    // same idempotency key with a *different* body succeeds (Miss
    // → store), instead of returning Conflict against the cached
    // body that lazy TTL already considered "stale" but hadn't
    // reclaimed.
    let store = IdempotencyStore::in_memory();
    let now = Utc::now();
    store_entry(&store, "k", b"original", now - Duration::hours(25));

    // Without prune: lookup with a different body would still surface
    // as Miss (lazy TTL), but the row remains in the table — so the
    // count stays at 1 forever as the tenant's traffic flows.
    assert_eq!(store.len(), 1);
    store.prune(now);
    assert_eq!(store.len(), 0, "row reclaimed");

    // Now a fresh request with a totally different body succeeds.
    let new_digest = IdempotencyStore::digest_request(b"changed");
    let out = store.lookup("tenant_a", "k", &new_digest, now);
    assert!(matches!(out, LookupOutcome::Miss));
}

#[test]
fn record_idempotency_sweep_advances_both_counters() {
    let before_pruned = IDEMPOTENCY_PRUNED.get();
    let before_ticks = IDEMPOTENCY_SWEEPER_TICKS.get();

    stateset_icp_handler::metrics::record_idempotency_sweep(7);

    assert!(
        IDEMPOTENCY_PRUNED.get() >= before_pruned + 7,
        "pruned counter must advance by the count reported"
    );
    assert!(
        IDEMPOTENCY_SWEEPER_TICKS.get() > before_ticks,
        "sweeper liveness counter must advance regardless of count"
    );

    // Zero-prune ticks still bump the liveness counter — that's the
    // monitoring property.
    let liveness_before = IDEMPOTENCY_SWEEPER_TICKS.get();
    stateset_icp_handler::metrics::record_idempotency_sweep(0);
    assert!(
        IDEMPOTENCY_SWEEPER_TICKS.get() > liveness_before,
        "liveness counter must advance on zero-prune ticks too"
    );
}
