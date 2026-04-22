//! Webhook outbox retention sweep.
//!
//! Without TTL-based pruning of `delivered` + `dead_lettered` rows,
//! the outbox grows unbounded as production traffic flows through —
//! every successful delivery adds a permanent row. The retention
//! sweep deletes rows older than the configured windows on each
//! worker tick.
//!
//! Asserts:
//!   * `WebhookOutbox::prune` deletes ONLY `delivered` rows older
//!     than `delivered_cutoff` and `dead_lettered` rows older than
//!     `dead_lettered_cutoff` — pending/in_flight/failed rows are
//!     never touched (the worker still owns them).
//!   * `None` for either cutoff disables that side of the sweep.
//!   * `WebhookWorker::with_retention(0, 0)` is a no-op (matches
//!     `Config::for_test()` default — existing tests don't churn).
//!   * `with_retention(>0, >0)` computes cutoffs from `now` and
//!     prunes accordingly.
//!   * `record_webhook_prune` bumps the
//!     `icp_webhook_outbox_pruned_total{reason}` counter.
//!   * The persistent (SQLite) backend behaves the same as the
//!     in-memory one so the property holds in production.

use chrono::{Duration, Utc};
use stateset_icp_handler::{
    metrics::WEBHOOK_OUTBOX_PRUNED,
    state_db,
    webhook::{
        DeliveryStatus, WebhookDelivery, WebhookOutbox, WebhookWorker, DEFAULT_MAX_ATTEMPTS,
    },
};

fn delivery(
    id: &str,
    status: DeliveryStatus,
    created_at: chrono::DateTime<Utc>,
) -> WebhookDelivery {
    WebhookDelivery {
        id: id.to_string(),
        event_id: format!("evt_{id}"),
        event_type: "transaction.created".into(),
        url: "http://example.invalid/hook".into(),
        payload_json: "{}".into(),
        status,
        attempts: 0,
        max_attempts: DEFAULT_MAX_ATTEMPTS,
        next_attempt_at: created_at,
        last_status_code: None,
        last_error: None,
        created_at,
        updated_at: created_at,
        delivered_at: None,
        tenant_id: String::new(),
    }
}

// --------------------------------------------------------------------------

#[test]
fn prune_deletes_delivered_older_than_cutoff_only() {
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    outbox.enqueue(delivery(
        "old_delivered",
        DeliveryStatus::Delivered,
        now - Duration::days(10),
    ));
    outbox.enqueue(delivery(
        "new_delivered",
        DeliveryStatus::Delivered,
        now - Duration::hours(1),
    ));
    outbox.enqueue(delivery(
        "pending",
        DeliveryStatus::Pending,
        now - Duration::days(30),
    ));
    outbox.enqueue(delivery(
        "in_flight",
        DeliveryStatus::InFlight,
        now - Duration::days(30),
    ));
    outbox.enqueue(delivery(
        "failed",
        DeliveryStatus::Failed,
        now - Duration::days(30),
    ));

    // Cutoff: anything created before (now - 7 days). Only the
    // 10-day-old delivered row should drop.
    let report = outbox.prune(Some(now - Duration::days(7)), None);
    assert_eq!(report.delivered_pruned, 1);
    assert_eq!(report.dead_lettered_pruned, 0);
    assert!(outbox.get("old_delivered").is_none());
    assert!(
        outbox.get("new_delivered").is_some(),
        "1h-old delivered row stays"
    );
    assert!(outbox.get("pending").is_some(), "pending row never pruned");
    assert!(
        outbox.get("in_flight").is_some(),
        "in_flight row never pruned"
    );
    assert!(
        outbox.get("failed").is_some(),
        "failed row never pruned (worker still owns it)"
    );
}

#[test]
fn prune_deletes_dead_lettered_older_than_cutoff_only() {
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    outbox.enqueue(delivery(
        "old_dead",
        DeliveryStatus::DeadLettered,
        now - Duration::days(60),
    ));
    outbox.enqueue(delivery(
        "new_dead",
        DeliveryStatus::DeadLettered,
        now - Duration::days(5),
    ));

    // Cutoff: anything created before (now - 30 days). Only the
    // 60-day-old dead_lettered row drops.
    let report = outbox.prune(None, Some(now - Duration::days(30)));
    assert_eq!(report.delivered_pruned, 0);
    assert_eq!(report.dead_lettered_pruned, 1);
    assert!(outbox.get("old_dead").is_none());
    assert!(
        outbox.get("new_dead").is_some(),
        "5d-old dead_lettered row stays"
    );
}

#[test]
fn prune_with_no_cutoffs_is_noop() {
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    outbox.enqueue(delivery(
        "d1",
        DeliveryStatus::Delivered,
        now - Duration::days(100),
    ));
    outbox.enqueue(delivery(
        "d2",
        DeliveryStatus::DeadLettered,
        now - Duration::days(100),
    ));

    let report = outbox.prune(None, None);
    assert_eq!(report.delivered_pruned, 0);
    assert_eq!(report.dead_lettered_pruned, 0);
    assert_eq!(outbox.len(), 2, "no cutoff = no deletion");
}

#[test]
fn worker_with_retention_zero_is_a_noop_sweep() {
    // Mirrors Config::for_test() defaults: retain_*_days = 0 disables
    // pruning entirely. This is the property that keeps every
    // pre-existing webhook test passing untouched.
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    outbox.enqueue(delivery(
        "d",
        DeliveryStatus::Delivered,
        now - Duration::days(100),
    ));
    outbox.enqueue(delivery(
        "dl",
        DeliveryStatus::DeadLettered,
        now - Duration::days(100),
    ));

    let worker = WebhookWorker::new(outbox.clone(), "secret".into()).with_retention(0, 0);
    let report = worker.prune_now(now);
    assert_eq!(report.delivered_pruned, 0);
    assert_eq!(report.dead_lettered_pruned, 0);
    assert_eq!(outbox.len(), 2);
}

#[test]
fn worker_with_retention_positive_prunes_at_cutoffs() {
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    // Two delivered rows: one fresh (1d), one stale (10d). Retention
    // = 7d → only the stale one drops.
    outbox.enqueue(delivery(
        "fresh_d",
        DeliveryStatus::Delivered,
        now - Duration::days(1),
    ));
    outbox.enqueue(delivery(
        "stale_d",
        DeliveryStatus::Delivered,
        now - Duration::days(10),
    ));
    // Two dead_lettered rows: one inside 30d, one outside.
    outbox.enqueue(delivery(
        "fresh_dl",
        DeliveryStatus::DeadLettered,
        now - Duration::days(20),
    ));
    outbox.enqueue(delivery(
        "stale_dl",
        DeliveryStatus::DeadLettered,
        now - Duration::days(40),
    ));

    let worker = WebhookWorker::new(outbox.clone(), "secret".into()).with_retention(7, 30);
    let report = worker.prune_now(now);
    assert_eq!(report.delivered_pruned, 1);
    assert_eq!(report.dead_lettered_pruned, 1);
    assert!(outbox.get("fresh_d").is_some());
    assert!(outbox.get("stale_d").is_none());
    assert!(outbox.get("fresh_dl").is_some());
    assert!(outbox.get("stale_dl").is_none());
}

#[test]
fn pruned_counter_advances_when_rows_drop() {
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    outbox.enqueue(delivery(
        "doomed",
        DeliveryStatus::Delivered,
        now - Duration::days(10),
    ));

    let before = WEBHOOK_OUTBOX_PRUNED
        .with_label_values(&["delivered"])
        .get();
    let worker = WebhookWorker::new(outbox.clone(), "secret".into()).with_retention(7, 0);
    let report = worker.prune_now(now);
    stateset_icp_handler::metrics::record_webhook_prune(&report);
    let after = WEBHOOK_OUTBOX_PRUNED
        .with_label_values(&["delivered"])
        .get();
    assert!(
        after > before,
        "delivered pruned counter must advance (before={before}, after={after})"
    );
}

#[test]
fn persistent_backend_prune_matches_in_memory() {
    // Same scenario as `prune_deletes_delivered_older_than_cutoff_only`
    // but against the SQLite backend — guards against a divergence
    // between the two execution paths (the SQL DELETE vs the in-memory
    // `retain` filter).
    let pool = state_db::open(":memory:").expect("open pool");
    let outbox = WebhookOutbox::with_pool(pool);
    let now = Utc::now();
    outbox.enqueue(delivery(
        "old_d",
        DeliveryStatus::Delivered,
        now - Duration::days(10),
    ));
    outbox.enqueue(delivery(
        "new_d",
        DeliveryStatus::Delivered,
        now - Duration::hours(1),
    ));
    outbox.enqueue(delivery(
        "p",
        DeliveryStatus::Pending,
        now - Duration::days(30),
    ));
    outbox.enqueue(delivery(
        "f",
        DeliveryStatus::Failed,
        now - Duration::days(30),
    ));

    let report = outbox.prune(Some(now - Duration::days(7)), None);
    assert_eq!(
        report.delivered_pruned, 1,
        "SQLite DELETE must match in-memory retain semantics"
    );
    assert!(outbox.get("old_d").is_none());
    assert!(outbox.get("new_d").is_some());
    assert!(
        outbox.get("p").is_some(),
        "pending never pruned by SQLite path either"
    );
    assert!(
        outbox.get("f").is_some(),
        "failed never pruned by SQLite path either"
    );
}
