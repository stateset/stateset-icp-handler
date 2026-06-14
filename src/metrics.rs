//! Prometheus metrics.

use lazy_static::lazy_static;
use prometheus::{
    register_histogram_vec, register_int_counter, register_int_counter_vec, register_int_gauge_vec,
    HistogramVec, IntCounter, IntCounterVec, IntGaugeVec, TextEncoder,
};

lazy_static! {
    pub static ref HTTP_REQUESTS: IntCounterVec = register_int_counter_vec!(
        "icp_http_requests_total",
        "Total ICP HTTP requests by route and status.",
        &["route", "status"]
    )
    .expect("register icp_http_requests_total");
    pub static ref HTTP_LATENCY: HistogramVec = register_histogram_vec!(
        "icp_http_request_duration_seconds",
        "ICP HTTP request latency in seconds.",
        &["route"]
    )
    .expect("register icp_http_request_duration_seconds");
    pub static ref INTENTS_PROCESSED: IntCounterVec = register_int_counter_vec!(
        "icp_intents_total",
        "Intents processed, partitioned by intent and outcome.",
        &["intent", "outcome"]
    )
    .expect("register icp_intents_total");

    /// Total webhook delivery attempts that resulted in each terminal
    /// or transient outcome. `delivered` is success; `failed` is one
    /// retryable failure; `dead_lettered` is a delivery that exhausted
    /// `max_attempts` and stopped being retried automatically.
    /// Operators alert on a sustained `dead_lettered` rate (a
    /// destination is broken).
    pub static ref WEBHOOK_DELIVERIES: IntCounterVec = register_int_counter_vec!(
        "icp_webhook_deliveries_total",
        "Webhook delivery outcomes per worker tick (delivered/failed/dead_lettered).",
        &["outcome"]
    )
    .expect("register icp_webhook_deliveries_total");

    /// Current depth of the durable outbox by FSM status. Refreshed
    /// once per worker tick (so backlog spikes between ticks aren't
    /// instantly visible — pick a sub-tick scrape interval if you
    /// need finer resolution). `pending` rising = backlog; `failed`
    /// rising = subscriber is flapping; `dead_lettered > 0` = a
    /// destination has fully exhausted retries.
    pub static ref WEBHOOK_OUTBOX_DEPTH: IntGaugeVec = register_int_gauge_vec!(
        "icp_webhook_outbox_queue_depth",
        "Webhook outbox row count by status.",
        &["status"]
    )
    .expect("register icp_webhook_outbox_queue_depth");

    /// Worker liveness signal — bumped on every tick, even ticks
    /// that processed zero deliveries. Alert on the rate of change
    /// dropping (worker stuck) rather than the absolute value.
    pub static ref WEBHOOK_TICKS: IntCounter = register_int_counter!(
        "icp_webhook_worker_ticks_total",
        "Total webhook worker tick invocations (liveness signal)."
    )
    .expect("register icp_webhook_worker_ticks_total");

    /// Rows pruned by the retention sweep, partitioned by FSM
    /// status. `delivered` rows past the configured retention are
    /// historical noise; `dead_lettered` rows past their retention
    /// are operationally written off. A sustained nonzero rate is
    /// the system telling you that without the sweep, the outbox
    /// would grow unbounded — exactly what the retention is for.
    pub static ref WEBHOOK_OUTBOX_PRUNED: IntCounterVec = register_int_counter_vec!(
        "icp_webhook_outbox_pruned_total",
        "Webhook outbox rows pruned by the retention sweep, by status.",
        &["reason"]
    )
    .expect("register icp_webhook_outbox_pruned_total");

    /// Subscription renewal outcomes per scheduler tick. `outcome`
    /// is `renewed` (charge succeeded), `failed` (transient charge
    /// failure — backoff schedule consumed an attempt), or
    /// `past_due` (dunning schedule exhausted, sub transitioned).
    /// Alert on a rising `past_due` rate (payment infra is degraded
    /// or a customer cohort is failing at unusual rates).
    pub static ref SUBSCRIPTION_RENEWALS: IntCounterVec = register_int_counter_vec!(
        "icp_subscription_renewals_total",
        "Subscription renewal outcomes per scheduler tick (renewed/failed/past_due).",
        &["outcome"]
    )
    .expect("register icp_subscription_renewals_total");

    /// Current subscription headcount by status. Refreshed once per
    /// scheduler tick. `active` = healthy paying customer, `paused`
    /// = customer-initiated pause, `past_due` = dunning failed and
    /// auto-renewals halted (operator may need to intervene),
    /// `canceled` = terminal state (kept for audit).
    pub static ref SUBSCRIPTIONS_BY_STATUS: IntGaugeVec = register_int_gauge_vec!(
        "icp_subscriptions_by_status",
        "Subscription headcount by status.",
        &["status"]
    )
    .expect("register icp_subscriptions_by_status");

    /// Scheduler liveness signal — bumped on every scheduler tick,
    /// including no-op ticks. Alert on rate-of-change dropping
    /// (scheduler stuck or crashed).
    pub static ref SUBSCRIPTION_SCHEDULER_TICKS: IntCounter = register_int_counter!(
        "icp_subscription_scheduler_ticks_total",
        "Total subscription scheduler tick invocations (liveness signal)."
    )
    .expect("register icp_subscription_scheduler_ticks_total");

    /// Idempotency cache rows pruned by the TTL sweep. Without
    /// active eviction, the table grows unbounded — lazy TTL only
    /// keeps stale entries from being replayed, it never reclaims
    /// the row. A sustained nonzero rate is the system telling you
    /// retention is doing its job.
    pub static ref IDEMPOTENCY_PRUNED: IntCounter = register_int_counter!(
        "icp_idempotency_pruned_total",
        "Idempotency cache rows pruned by the TTL sweep."
    )
    .expect("register icp_idempotency_pruned_total");

    /// Idempotency sweeper liveness signal — bumped on every
    /// sweeper tick, including no-op ticks. Alert on
    /// rate-of-change dropping.
    pub static ref IDEMPOTENCY_SWEEPER_TICKS: IntCounter = register_int_counter!(
        "icp_idempotency_sweeper_ticks_total",
        "Total idempotency sweeper tick invocations (liveness signal)."
    )
    .expect("register icp_idempotency_sweeper_ticks_total");

    /// Quote / peer-quote expiries effected by the expiry sweeper.
    /// `kind` ∈ `{transaction, peer_quote}`. A sustained rate
    /// is normal traffic (agents request quotes they never
    /// authorize); a sudden spike can indicate authorization-path
    /// breakage keeping legitimate users from converting.
    pub static ref EXPIRIES: IntCounterVec = register_int_counter_vec!(
        "icp_expiries_total",
        "Quote/peer-quote expiries effected by the expiry sweeper.",
        &["kind"]
    )
    .expect("register icp_expiries_total");

    /// Expiry sweeper liveness signal — bumped on every tick.
    pub static ref EXPIRY_SWEEPER_TICKS: IntCounter = register_int_counter!(
        "icp_expiry_sweeper_ticks_total",
        "Total expiry sweeper tick invocations (liveness signal)."
    )
    .expect("register icp_expiry_sweeper_ticks_total");
}

pub fn encode() -> String {
    let encoder = TextEncoder::new();
    let metrics = prometheus::gather();
    encoder.encode_to_string(&metrics).unwrap_or_default()
}

pub fn record_http(route: &str, status: u16, elapsed_secs: f64) {
    HTTP_REQUESTS
        .with_label_values(&[route, &status.to_string()])
        .inc();
    HTTP_LATENCY
        .with_label_values(&[route])
        .observe(elapsed_secs);
}

pub fn record_intent(intent: &str, outcome: &str) {
    INTENTS_PROCESSED
        .with_label_values(&[intent, outcome])
        .inc();
}

/// Bump the per-outcome webhook delivery counter.
pub fn record_webhook_delivery(outcome: &str) {
    WEBHOOK_DELIVERIES.with_label_values(&[outcome]).inc();
}

/// Bump the worker tick counter and refresh the queue-depth gauge
/// from a fresh outbox snapshot. Called at the end of each worker
/// tick so scrapes reflect post-tick state.
pub fn record_webhook_tick(counts: &crate::webhook::StatusCounts) {
    WEBHOOK_TICKS.inc();
    WEBHOOK_OUTBOX_DEPTH
        .with_label_values(&["pending"])
        .set(counts.pending as i64);
    WEBHOOK_OUTBOX_DEPTH
        .with_label_values(&["in_flight"])
        .set(counts.in_flight as i64);
    WEBHOOK_OUTBOX_DEPTH
        .with_label_values(&["delivered"])
        .set(counts.delivered as i64);
    WEBHOOK_OUTBOX_DEPTH
        .with_label_values(&["failed"])
        .set(counts.failed as i64);
    WEBHOOK_OUTBOX_DEPTH
        .with_label_values(&["dead_lettered"])
        .set(counts.dead_lettered as i64);
}

/// Bump the idempotency-sweeper liveness counter and add `pruned`
/// to the prune total. Called from `idempotency::sweeper::run_loop`
/// after each sweep.
pub fn record_idempotency_sweep(pruned: usize) {
    IDEMPOTENCY_SWEEPER_TICKS.inc();
    if pruned > 0 {
        IDEMPOTENCY_PRUNED.inc_by(pruned as u64);
    }
}

/// Bump the expiry-sweeper liveness counter and add the per-kind
/// counts to the `icp_expiries_total` series.
pub fn record_expiry_tick(report: &crate::service::ExpiryTickReport) {
    EXPIRY_SWEEPER_TICKS.inc();
    if report.transactions_expired > 0 {
        EXPIRIES
            .with_label_values(&["transaction"])
            .inc_by(report.transactions_expired as u64);
    }
    if report.peer_quotes_expired > 0 {
        EXPIRIES
            .with_label_values(&["peer_quote"])
            .inc_by(report.peer_quotes_expired as u64);
    }
}

/// Bump the prune counter by the row counts reported by a sweep.
/// Called from `webhook::run_loop` once per tick after the sweep
/// runs.
pub fn record_webhook_prune(report: &crate::webhook::PruneReport) {
    if report.delivered_pruned > 0 {
        WEBHOOK_OUTBOX_PRUNED
            .with_label_values(&["delivered"])
            .inc_by(report.delivered_pruned as u64);
    }
    if report.dead_lettered_pruned > 0 {
        WEBHOOK_OUTBOX_PRUNED
            .with_label_values(&["dead_lettered"])
            .inc_by(report.dead_lettered_pruned as u64);
    }
}

/// Bump the per-outcome subscription renewal counter.
pub fn record_subscription_renewal(outcome: &str) {
    SUBSCRIPTION_RENEWALS.with_label_values(&[outcome]).inc();
}

/// Bump the scheduler tick counter and refresh the
/// `icp_subscriptions_by_status` gauge from a fresh subscription
/// snapshot. Called from `scheduler::run_loop` after each tick.
pub fn record_subscription_scheduler_tick(counts: &crate::state_store::SubscriptionStatusCounts) {
    SUBSCRIPTION_SCHEDULER_TICKS.inc();
    SUBSCRIPTIONS_BY_STATUS
        .with_label_values(&["trialing"])
        .set(counts.trialing as i64);
    SUBSCRIPTIONS_BY_STATUS
        .with_label_values(&["active"])
        .set(counts.active as i64);
    SUBSCRIPTIONS_BY_STATUS
        .with_label_values(&["paused"])
        .set(counts.paused as i64);
    SUBSCRIPTIONS_BY_STATUS
        .with_label_values(&["canceled"])
        .set(counts.canceled as i64);
    SUBSCRIPTIONS_BY_STATUS
        .with_label_values(&["past_due"])
        .set(counts.past_due as i64);
}
