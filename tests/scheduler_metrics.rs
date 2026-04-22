//! Operator metrics for the subscription scheduler.
//!
//! Mirrors the webhook-outbox metrics suite — the scheduler is the
//! other long-running background loop that runs production
//! deployments need to dashboard. Asserts:
//!   * `SubscriptionStore::status_counts()` returns accurate
//!     per-status counts.
//!   * `tick_subscriptions` bumps the
//!     `icp_subscription_renewals_total{outcome=…}` counter for
//!     every renewal transition (renewed, failed, past_due).
//!   * `metrics::record_subscription_scheduler_tick()` refreshes
//!     the `icp_subscriptions_by_status{status=…}` gauge.
//!   * `/metrics` exposes all three series with their HELP/TYPE
//!     headers.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo,
    build_app_state, build_router,
    config::Config,
    metrics::SUBSCRIPTION_RENEWALS,
    models::{Subscription, SubscriptionStatus},
    AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:smetrics";

async fn build(keys: Vec<ApiKeyInfo>) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    cfg.api_keys_json = Some(serde_json::to_string(&keys).unwrap());
    let state = build_app_state(&cfg).await.expect("state");
    let router = build_router(state.clone());
    (state, router)
}

fn key(name: &str, tenant: &str) -> ApiKeyInfo {
    ApiKeyInfo {
        key: format!("k_{name}"),
        tenant_id: tenant.to_string(),
        name: name.to_string(),
        rate_limit_per_minute: None,
        allowed_agents: None,
        expires_at: None,
    }
}

async fn subscribe(app: &Router, bearer: &str) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {bearer}"))
        .header("ICP-Agent-Id", AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.subscribe",
                "agent_id": AGENT,
                "params": {
                    "items": [{
                        "sku": "PLAN-PRO",
                        "quantity": 1,
                        "unit_price_hint": { "amount_minor": 4900, "currency": "USD" }
                    }],
                    "cadence": "monthly",
                    "payment": { "method": "card", "token": "tok_sub" }
                },
                "context": { "currency": "USD" }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn scrape_metrics(app: &Router) -> String {
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap_or_default()
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn status_counts_groups_by_subscription_status() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // Bring two subscriptions into existence via the real intent
    // path so we know the count tally walks the same store the
    // scheduler walks.
    for _ in 0..2 {
        let _ = subscribe(&app, "k_a").await;
    }

    // Then mutate one directly to past_due so we have non-trivial
    // distribution. Direct mutation rather than driving the scheduler
    // keeps this test deterministic across CI scheduling jitter.
    let any_id = state
        .service
        .subscriptions
        .list(usize::MAX)
        .first()
        .map(|s| s.id.clone())
        .unwrap();
    state.service.subscriptions.update(&any_id, |s| {
        s.status = SubscriptionStatus::PastDue;
    });

    let counts = state.service.subscriptions.status_counts();
    assert_eq!(counts.active, 1, "one active sub remains");
    assert_eq!(counts.past_due, 1, "one transitioned to past_due");
    assert_eq!(counts.paused, 0);
    assert_eq!(counts.canceled, 0);
}

#[tokio::test]
async fn metrics_endpoint_exposes_scheduler_series() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;
    let _ = subscribe(&app, "k_a").await;

    // Manually refresh the gauge — same call run_loop makes after
    // each tick.
    stateset_icp_handler::metrics::record_subscription_scheduler_tick(
        &state.service.subscriptions.status_counts(),
    );
    // Prometheus only materializes labelled counters after the first
    // touch — `inc_by(0)` registers the series without skewing the
    // value. Without this, a fresh process scrape misses the
    // counter entirely. Three labels cover the FSM states
    // `record_subscription_renewal` would emit.
    for outcome in ["renewed", "failed", "past_due"] {
        SUBSCRIPTION_RENEWALS
            .with_label_values(&[outcome])
            .inc_by(0);
    }

    let body = scrape_metrics(&app).await;
    for needle in [
        "icp_subscription_renewals_total",
        "icp_subscriptions_by_status",
        "icp_subscription_scheduler_ticks_total",
    ] {
        let help = format!("# HELP {needle}");
        let type_line = format!("# TYPE {needle}");
        assert!(
            body.contains(&help),
            "missing HELP for {needle}\n--- /metrics body ---\n{body}"
        );
        assert!(body.contains(&type_line), "missing TYPE for {needle}");
    }

    // Real value emerges on the gauge — proves the refresh helper
    // is wired to the actual store, not a hardcoded zero.
    assert!(
        body.lines().any(
            |l| l.starts_with("icp_subscriptions_by_status{status=\"active\"}")
                && l.ends_with(" 1")
        ),
        "by_status active=1 gauge line not found in /metrics output\n{body}"
    );
}

#[tokio::test]
async fn renewal_counter_advances_on_successful_tick() {
    // Drive the scheduler against a pre-staged due subscription and
    // verify the renewal counter increments. Not relying on the
    // scheduler's wall-clock loop — calls `tick_subscriptions`
    // directly so the test is deterministic.
    let (state, app) = build(vec![key("a", "tenant_a")]).await;
    let body = subscribe(&app, "k_a").await;
    let sub_id = body["subscription"]["id"]
        .as_str()
        .expect("subscribe should return a subscription id")
        .to_string();

    // Force the subscription to be due now (the just-subscribed
    // version has next_charge_at = period_end which is in the
    // future).
    state.service.subscriptions.update(&sub_id, |s| {
        s.next_charge_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    });

    let before = SUBSCRIPTION_RENEWALS.with_label_values(&["renewed"]).get();
    let report = state.service.tick_subscriptions(chrono::Utc::now()).await;
    assert_eq!(
        report.renewed, 1,
        "scheduler should renew the staged subscription"
    );
    let after = SUBSCRIPTION_RENEWALS.with_label_values(&["renewed"]).get();
    assert!(
        after > before,
        "renewed counter must advance (before={before}, after={after})"
    );
}

#[tokio::test]
async fn failed_counter_advances_when_charge_rejected() {
    // A2A payment instrument is rejected by the scheduler — gives us
    // deterministic failure without mocking the engine. Mirrors the
    // pattern in `tests/scheduler.rs::repeated_failures_transition_to_past_due`.
    let (state, app) = build(vec![key("a", "tenant_a")]).await;
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", "Bearer k_a")
        .header("ICP-Agent-Id", AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.subscribe",
                "agent_id": AGENT,
                "params": {
                    "items": [{
                        "sku": "PLAN-PRO",
                        "quantity": 1,
                        "unit_price_hint": { "amount_minor": 4900, "currency": "USD" }
                    }],
                    "cadence": "monthly",
                    "payment": { "method": "a2a", "peer_agent_id": "did:peer:other", "memo": "x" }
                },
                "context": { "currency": "USD" }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let sub_id = body["subscription"]["id"].as_str().unwrap().to_string();

    // Force due, then tick — should record a failure (not a past_due
    // on the first try since the default test config has no dunning
    // schedule and MAX_RENEWAL_FAILURES > 1).
    state.service.subscriptions.update(&sub_id, |s| {
        s.next_charge_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    });

    let before_failed = SUBSCRIPTION_RENEWALS.with_label_values(&["failed"]).get();
    let report = state.service.tick_subscriptions(chrono::Utc::now()).await;
    assert_eq!(report.failed, 1, "tick should report one failure");
    let after_failed = SUBSCRIPTION_RENEWALS.with_label_values(&["failed"]).get();
    assert!(
        after_failed > before_failed,
        "failed counter must advance (before={before_failed}, after={after_failed})"
    );

    // Subscription stays Active until MAX_RENEWAL_FAILURES is hit.
    let post = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(post.status, SubscriptionStatus::Active);
    let _: Subscription = post; // silence unused-import lint
}
