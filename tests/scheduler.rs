//! Subscription scheduler integration tests.
//!
//! Drives `IcpService::tick_subscriptions(now)` directly so behavior is
//! deterministic — no sleeping, no wall-clock dependency. The
//! `scheduler::run_loop` background task is exercised by exactly one
//! test (`background_loop_actually_renews`) that uses a millisecond-level
//! interval and a short wait.

use axum::body::{to_bytes, Body};
use axum::http::Request;
use axum::Router;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use stateset_icp_handler::{build_app_state, build_router, config::Config, scheduler, AppState};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";
const DEMO_AGENT: &str = "did:stateset:agent:scheduler-test";

async fn build() -> (AppState, Router) {
    build_with(|_| {}).await
}

async fn build_with(mut mutate: impl FnMut(&mut Config)) -> (AppState, Router) {
    let mut config = Config::for_test();
    mutate(&mut config);
    let state = build_app_state(&config).await.expect("state");
    let router = build_router(state.clone());
    (state, router)
}

async fn submit(app: &Router, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", DEMO_AGENT)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn subscribe_body(payment: Value) -> Value {
    json!({
        "intent": "intent.subscribe",
        "agent_id": DEMO_AGENT,
        "params": {
            "items": [{
                "sku": "PLAN-PRO",
                "quantity": 1,
                "unit_price_hint": { "amount_minor": 4999, "currency": "USD" }
            }],
            "buyer": { "first_name": "Alice", "email": "alice@example.com" },
            "cadence": "monthly",
            "payment": payment,
        },
        "context": { "currency": "USD" }
    })
}

fn card_payment() -> Value {
    json!({ "method": "card", "token": "tok_sub", "last_digits": "4242", "brand": "visa" })
}

/// Force a subscription's `next_charge_at` to the supplied instant —
/// the only way to make a fresh sub "due" without sleeping for a month.
fn force_due(state: &AppState, sub_id: &str, when: DateTime<Utc>) {
    let result = state.service.subscriptions.update(sub_id, |s| {
        s.next_charge_at = when;
        s.current_period_end = when;
    });
    assert!(result.is_some(), "subscription {sub_id} not found");
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn tick_with_no_due_subs_is_a_noop() {
    let (state, _) = build().await;
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.scanned, 0);
    assert_eq!(report.due, 0);
    assert_eq!(report.renewed, 0);
}

#[tokio::test]
async fn subscribe_then_tick_before_due_does_not_charge_again() {
    let (state, app) = build().await;
    let resp = submit(&app, subscribe_body(card_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    // next_charge_at is ~30 days out; ticking now is well before.
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.due, 0, "fresh sub is not yet due");
    assert_eq!(report.renewed, 0);

    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.charges_completed, 1, "no extra charge");
}

#[tokio::test]
async fn tick_after_next_charge_at_runs_auto_renewal() {
    let (state, app) = build().await;
    let resp = submit(&app, subscribe_body(card_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let initial_txn_id = resp["transaction"]["id"].as_str().unwrap().to_string();

    let due_at = Utc::now() - Duration::seconds(1);
    force_due(&state, &sub_id, due_at);

    let now = Utc::now();
    let report = state.service.tick_subscriptions(now).await;
    assert_eq!(report.due, 1);
    assert_eq!(report.renewed, 1);
    assert_eq!(report.failed, 0);

    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.charges_completed, 2, "scheduler ran a second charge");
    assert!(
        after.last_transaction_id.as_deref() != Some(initial_txn_id.as_str()),
        "last_transaction_id should advance to the new charge"
    );
    assert_eq!(after.failed_renewal_attempts, 0);
    assert_eq!(after.status.wire_name(), "active");
    // Period anchored on the previous current_period_end, not on `now`.
    assert_eq!(after.current_period_start, due_at);
    assert!(after.next_charge_at > due_at);
}

#[tokio::test]
async fn trial_subscription_defers_first_charge_then_converts_to_active() {
    let (state, app) = build().await;
    let mut body = subscribe_body(card_payment());
    body["params"]["trial_days"] = json!(14);

    let resp = submit(&app, body).await;
    let sub = &resp["subscription"];
    let sub_id = sub["id"].as_str().unwrap().to_string();

    // Enrolled in a trial — no money moved, status trialing, first charge
    // deferred. The response transaction is the priced preview, not a charge.
    assert_eq!(sub["status"], "trialing");
    assert_eq!(sub["charges_completed"], 0);
    assert!(sub["trial_end"].is_string(), "trial_end must be set");
    assert!(
        sub["last_transaction_id"].is_null(),
        "no charge transaction during trial"
    );
    assert_eq!(
        resp["transaction"]["state"], "authorized",
        "trial enrollment is a pseudo-transaction, not a completed charge"
    );

    // Before trial end, the scheduler must NOT charge it.
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(
        report.renewed, 0,
        "trial must not be charged before it ends"
    );
    assert_eq!(
        state.service.subscriptions.get(&sub_id).unwrap().status,
        stateset_icp_handler::models::SubscriptionStatus::Trialing
    );

    // At trial end the scheduler bills the first charge and activates it.
    let due_at = Utc::now() - Duration::seconds(1);
    force_due(&state, &sub_id, due_at);
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.due, 1);
    assert_eq!(report.renewed, 1, "trial's first charge fires at trial end");

    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(
        after.status.wire_name(),
        "active",
        "trial converts to active"
    );
    assert_eq!(after.charges_completed, 1, "first real charge recorded");
    assert!(after.trial_end.is_none(), "trial_end cleared on conversion");
    assert!(after.last_transaction_id.is_some());
}

#[tokio::test]
async fn trial_days_overflow_is_rejected_not_panic() {
    // An unbounded trial_days would overflow `now + Duration` and (with
    // panic=abort) kill the process. It must be rejected as invalid input.
    let (_state, app) = build().await;
    let mut body = subscribe_body(card_payment());
    body["params"]["trial_days"] = json!(4_000_000_000u64); // > u32 cap anyway
    let resp = submit(&app, body).await;
    assert_eq!(resp["error"]["type"], "invalid_request");
}

#[tokio::test]
async fn trial_transaction_cannot_be_bought_directly() {
    // The trial enrollment returns a pseudo-transaction; an agent must NOT
    // be able to `intent.buy` it to charge the customer mid-trial.
    let (_state, app) = build().await;
    let mut body = subscribe_body(card_payment());
    body["params"]["trial_days"] = json!(14);
    let resp = submit(&app, body).await;
    let txn_id = resp["transaction"]["id"].as_str().unwrap().to_string();

    let buy = submit(
        &app,
        json!({
            "intent": "intent.buy",
            "agent_id": DEMO_AGENT,
            "params": {
                "transaction_id": txn_id,
                "payment": { "method": "card", "token": "tok_x" }
            }
        }),
    )
    .await;
    assert_eq!(buy["error"]["type"], "precondition_failed", "{buy}");
    assert!(buy["error"]["message"]
        .as_str()
        .unwrap()
        .contains("subscription"));
}

#[tokio::test]
async fn paused_subscription_is_skipped_by_scheduler() {
    let (state, app) = build().await;
    let resp = submit(&app, subscribe_body(card_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    // Pause via the intent path.
    submit(
        &app,
        json!({
            "intent": "intent.pause",
            "agent_id": DEMO_AGENT,
            "params": { "subscription_id": sub_id }
        }),
    )
    .await;

    force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.due, 0, "paused subs must not be picked up");

    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.charges_completed, 1, "no auto-renew while paused");
    assert_eq!(after.status.wire_name(), "paused");
}

#[tokio::test]
async fn canceled_subscription_is_skipped_by_scheduler() {
    let (state, app) = build().await;
    let resp = submit(&app, subscribe_body(card_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    submit(
        &app,
        json!({
            "intent": "intent.cancel_subscription",
            "agent_id": DEMO_AGENT,
            "params": { "subscription_id": sub_id }
        }),
    )
    .await;

    force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.due, 0);
    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.status.wire_name(), "canceled");
}

#[tokio::test]
async fn sub_without_payment_is_skipped() {
    let (state, app) = build().await;
    let resp = submit(&app, subscribe_body(card_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    // Strip the payment instrument and force the sub due.
    state
        .service
        .subscriptions
        .update(&sub_id, |s| {
            s.payment_instrument = None;
        })
        .unwrap();
    force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));

    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.due, 0, "sub without payment must be skipped");
}

#[tokio::test]
async fn repeated_failures_transition_to_past_due() {
    let (state, app) = build().await;
    // A2A payment instrument is rejected by the scheduler — gives us
    // deterministic failure without mocking the engine.
    let resp = submit(
        &app,
        subscribe_body(json!({
            "method": "a2a",
            "peer_agent_id": "did:peer:other",
            "memo": "test"
        })),
    )
    .await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    for attempt in 1..=3u32 {
        force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));
        let report = state.service.tick_subscriptions(Utc::now()).await;
        assert_eq!(report.due, 1, "attempt {attempt}: due count");
        assert_eq!(report.renewed, 0, "attempt {attempt}: nothing renewed");
        assert_eq!(report.failed, 1, "attempt {attempt}: one failure");
        let s = state.service.subscriptions.get(&sub_id).unwrap();
        assert_eq!(s.failed_renewal_attempts, attempt);
    }

    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(
        after.status.wire_name(),
        "past_due",
        "after MAX_RENEWAL_FAILURES the sub must be past_due"
    );

    // Past-due subs are skipped by the scheduler from then on.
    force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));
    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.due, 0, "past_due subs must not be retried");
}

#[tokio::test]
async fn manual_renew_resets_failure_counter() {
    let (state, app) = build().await;
    let resp = submit(
        &app,
        subscribe_body(json!({
            "method": "a2a", "peer_agent_id": "did:peer:other"
        })),
    )
    .await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    // Trigger one failure.
    force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));
    state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(
        state
            .service
            .subscriptions
            .get(&sub_id)
            .unwrap()
            .failed_renewal_attempts,
        1
    );

    // Manual renew with a fresh card payment must clear the counter.
    submit(
        &app,
        json!({
            "intent": "intent.renew",
            "agent_id": DEMO_AGENT,
            "params": {
                "subscription_id": sub_id,
                "payment": card_payment(),
            }
        }),
    )
    .await;
    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.failed_renewal_attempts, 0);
    assert_eq!(after.status.wire_name(), "active");
    // Charge counter went 1 → +1 (subscribe) → renew bumps to 2.
    // Note the failed scheduler attempt does NOT increment charges_completed.
    assert_eq!(after.charges_completed, 2);
}

#[tokio::test]
async fn tick_report_counts_mixed_workload() {
    let (state, app) = build().await;
    // Three subs: one due, one not, one paused.
    let due_id = submit(&app, subscribe_body(card_payment())).await["subscription"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let _untouched = submit(&app, subscribe_body(card_payment())).await["subscription"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let paused_id = submit(&app, subscribe_body(card_payment())).await["subscription"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    submit(
        &app,
        json!({
            "intent": "intent.pause",
            "agent_id": DEMO_AGENT,
            "params": { "subscription_id": paused_id }
        }),
    )
    .await;

    force_due(&state, &due_id, Utc::now() - Duration::seconds(1));
    force_due(&state, &paused_id, Utc::now() - Duration::seconds(1));

    let report = state.service.tick_subscriptions(Utc::now()).await;
    assert_eq!(report.scanned, 3, "all three subs scanned");
    assert_eq!(report.due, 1, "only the active+due one charged");
    assert_eq!(report.renewed, 1);
    assert_eq!(report.failed, 0);
}

#[tokio::test]
async fn background_loop_actually_renews() {
    // The deterministic happy path is covered above. This test proves
    // the wiring: `scheduler::run_loop` actually wakes up on its
    // interval and calls tick_subscriptions.
    let (state, app) = build().await;
    let resp = submit(&app, subscribe_body(card_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    force_due(&state, &sub_id, Utc::now() - Duration::seconds(1));

    let svc = Arc::new((*state.service).clone());
    let task = tokio::spawn(scheduler::run_loop(
        svc.clone(),
        StdDuration::from_millis(50),
    ));

    // Poll for up to 2s for the auto-renewal to happen.
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > StdDuration::from_secs(2) {
            task.abort();
            panic!("background scheduler did not renew within 2s");
        }
        let s = state.service.subscriptions.get(&sub_id).unwrap();
        if s.charges_completed >= 2 {
            assert_eq!(s.status.wire_name(), "active");
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    task.abort();
}
