//! Subscription dunning — backoff schedule between failed renewals.
//!
//! The scheduler tests in `tests/scheduler.rs` lock in legacy
//! semantics (empty schedule → 3 immediate failures → past_due). This
//! suite proves the production-grade behavior:
//!
//! * Each failure pushes `next_charge_at` forward by the configured
//!   backoff, so the worker won't re-pick the sub until the wall
//!   clock has advanced past it. A transient card decline gets time
//!   to self-resolve instead of burning the retry budget in seconds.
//! * After the schedule is exhausted, the next failure transitions
//!   to `past_due`.
//!
//! Tests run via direct `tick_subscriptions(now)` calls so behavior
//! is deterministic with no `tokio::sleep`. The configurable env
//! parser is exercised separately.

use axum::body::{to_bytes, Body};
use axum::http::Request;
use axum::Router;
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use stateset_icp_handler::{build_app_state, build_router, config::Config, AppState};
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";
const AGENT: &str = "did:stateset:agent:dunning-test";

/// Build a handler whose default subscription dunning schedule is the
/// supplied list of hour-counts. Empty preserves legacy semantics.
async fn build(schedule_hours: Vec<u32>) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.subscription_dunning_schedule_hours = schedule_hours;
    let state = build_app_state(&cfg).await.expect("state");
    let router = build_router(state.clone());
    (state, router)
}

async fn submit(app: &Router, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", AGENT)
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
        "agent_id": AGENT,
        "params": {
            "items": [{
                "sku": "PLAN-PRO", "quantity": 1,
                "unit_price_hint": { "amount_minor": 1999, "currency": "USD" }
            }],
            "buyer": { "first_name": "Alice", "email": "alice@example.com" },
            "cadence": "monthly",
            "payment": payment
        },
        "context": { "currency": "USD" }
    })
}

/// A2A payment instruments are explicitly rejected by the scheduler —
/// gives us deterministic failure without mocking the engine.
fn always_failing_payment() -> Value {
    json!({ "method": "a2a", "peer_agent_id": "did:peer:other" })
}

/// Force a subscription's `next_charge_at` to a specific instant.
fn force_due(state: &AppState, sub_id: &str, when: chrono::DateTime<Utc>) {
    state
        .service
        .subscriptions
        .update(sub_id, |s| {
            s.next_charge_at = when;
            s.current_period_end = when;
        })
        .expect("subscription exists");
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn first_failure_pushes_next_charge_by_first_backoff() {
    let (state, app) = build(vec![1, 6, 24]).await; // 1h, 6h, 24h
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let now = Utc::now();
    force_due(&state, &sub_id, now - Duration::seconds(1));

    let report = state.service.tick_subscriptions(now).await;
    assert_eq!(report.due, 1);
    assert_eq!(report.failed, 1);
    assert_eq!(report.past_due, 0); // far from past_due

    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.failed_renewal_attempts, 1);
    assert_eq!(
        after.status.wire_name(),
        "active",
        "1 of 4 attempts is far from past_due"
    );

    // next_charge_at must have moved forward by exactly 1 hour.
    let expected = now + Duration::hours(1);
    let drift_ms = (after.next_charge_at - expected).num_milliseconds().abs();
    assert!(
        drift_ms < 100,
        "next_charge_at should be ~{expected:?}, got {:?} (drift {drift_ms}ms)",
        after.next_charge_at,
    );
}

#[tokio::test]
async fn second_tick_before_backoff_elapses_does_not_re_attempt() {
    let (state, app) = build(vec![1, 6, 24]).await;
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let now = Utc::now();
    force_due(&state, &sub_id, now - Duration::seconds(1));

    state.service.tick_subscriptions(now).await; // 1 failure scheduled
    let after_first = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after_first.failed_renewal_attempts, 1);

    // 30 minutes later — still inside the 1h backoff window.
    let report = state
        .service
        .tick_subscriptions(now + Duration::minutes(30))
        .await;
    assert_eq!(
        report.due, 0,
        "scheduler must NOT re-pick the sub mid-backoff"
    );
    let unchanged = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(
        unchanged.failed_renewal_attempts, 1,
        "no second failure should have been recorded"
    );
}

#[tokio::test]
async fn backoff_grows_with_each_failure_per_schedule() {
    let (state, app) = build(vec![1, 6, 24]).await;
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let mut now = Utc::now();

    let expected_backoffs = [1, 6, 24];
    for (idx, &hours) in expected_backoffs.iter().enumerate() {
        force_due(&state, &sub_id, now);
        let report = state.service.tick_subscriptions(now).await;
        assert_eq!(report.failed, 1, "failure #{}", idx + 1);

        let after = state.service.subscriptions.get(&sub_id).unwrap();
        assert_eq!(after.failed_renewal_attempts, (idx as u32) + 1);
        let expected = now + Duration::hours(hours);
        let drift_ms = (after.next_charge_at - expected).num_milliseconds().abs();
        assert!(
            drift_ms < 100,
            "after failure #{}: expected next_charge_at ≈ +{}h, got {} ({}ms drift)",
            idx + 1,
            hours,
            after.next_charge_at,
            drift_ms,
        );
        // Advance the clock past the scheduled retry for the next iteration.
        now = expected + Duration::seconds(1);
    }
}

#[tokio::test]
async fn schedule_exhaustion_transitions_to_past_due() {
    // Schedule of 3 entries → 4 attempts allowed. The 4th failure
    // exhausts the schedule and triggers past_due.
    let (state, app) = build(vec![1, 6, 24]).await;
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let mut now = Utc::now();

    for attempt in 1..=4u32 {
        force_due(&state, &sub_id, now);
        let report = state.service.tick_subscriptions(now).await;
        let after = state.service.subscriptions.get(&sub_id).unwrap();
        assert_eq!(
            after.failed_renewal_attempts, attempt,
            "attempt {attempt}: counter"
        );
        if attempt < 4 {
            assert_eq!(report.past_due, 0, "attempt {attempt}: not yet past_due");
            assert_eq!(after.status.wire_name(), "active");
            // Advance past whatever backoff was scheduled.
            now = after.next_charge_at + Duration::seconds(1);
        } else {
            assert_eq!(
                report.past_due, 1,
                "the 4th failure exhausts schedule [1,6,24] and pasts_due"
            );
            assert_eq!(after.status.wire_name(), "past_due");
        }
    }
}

#[tokio::test]
async fn successful_renewal_clears_dunning_state() {
    // Standard property: a working renewal between failures resets
    // both the failure counter AND the dunning trajectory.
    let (state, app) = build(vec![1, 6, 24]).await;
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let now = Utc::now();
    force_due(&state, &sub_id, now);
    state.service.tick_subscriptions(now).await;

    let after_failure = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after_failure.failed_renewal_attempts, 1);
    let backoff_target = after_failure.next_charge_at;
    assert!(backoff_target > now, "backoff scheduled");

    // Manual renew with a working card.
    submit(
        &app,
        json!({
            "intent": "intent.renew",
            "agent_id": AGENT,
            "params": {
                "subscription_id": sub_id,
                "payment": { "method": "card", "token": "tok_works" }
            }
        }),
    )
    .await;
    let after_renew = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after_renew.failed_renewal_attempts, 0, "counter cleared");
    assert_eq!(after_renew.status.wire_name(), "active");
    // next_charge_at moved to the new period_end, which is FAR past
    // the dunning backoff target.
    assert!(
        after_renew.next_charge_at > backoff_target,
        "successful renewal should advance period past the backoff target"
    );
}

#[tokio::test]
async fn renewed_event_payload_carries_cycle_context() {
    // Operator automation around `subscription.renewed` needs
    // cycle context: receipt mailers need `transaction_id` to link
    // to the signed receipt; accounting needs `cycle_number` +
    // period bounds; customer-facing notifications need
    // `next_charge_at` to set expectations. The original payload
    // (`subscription_id` + `automatic`) was the bare-minimum signal.
    let (state, app) = build(vec![]).await;
    let mut events = state.service.events.subscribe();

    // Subscribe with a working card (default test engine accepts
    // `tok_works`-style tokens).
    let resp = submit(
        &app,
        subscribe_body(json!({ "method": "card", "token": "tok_works" })),
    )
    .await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let pre = state.service.subscriptions.get(&sub_id).unwrap();
    let charges_pre = pre.charges_completed;

    // Drain setup events so recv() lands on the renewed one.
    while events.try_recv().is_ok() {}

    let now = Utc::now();
    force_due(&state, &sub_id, now);
    let report = state.service.tick_subscriptions(now).await;
    assert_eq!(report.renewed, 1, "scheduler should renew");

    let mut payload: Option<Value> = None;
    while let Ok(ev) = events.try_recv() {
        if ev.r#type == "subscription.renewed" {
            payload = Some(ev.payload);
            break;
        }
    }
    let payload = payload.expect("subscription.renewed event must be emitted");

    // Pre-existing fields — kept for back-compat.
    assert_eq!(payload["subscription_id"], sub_id);
    assert_eq!(payload["automatic"], true);

    // New cycle-context fields.
    let txn_id = payload["transaction_id"]
        .as_str()
        .expect("transaction_id must be set so receipt mailers can link to the receipt");
    assert!(
        txn_id.starts_with("txn_"),
        "transaction id format: {txn_id}"
    );
    assert_eq!(
        payload["cycle_number"].as_u64().unwrap(),
        (charges_pre + 1) as u64,
        "cycle_number is the new charges_completed value (post-increment)"
    );
    assert!(
        payload["current_period_start"].is_string(),
        "period bounds drive cycle accounting"
    );
    assert!(payload["current_period_end"].is_string());
    assert!(
        payload["next_charge_at"].is_string(),
        "next_charge_at sets customer expectations in renewal emails"
    );
    assert_eq!(
        payload["next_charge_at"], payload["current_period_end"],
        "next_charge_at == period_end on a successful renewal — both stamps"
    );
    // amount_minor / currency are best-effort (engine may not always
    // populate totals.total). Just assert they exist as fields,
    // even if amount_minor is null.
    assert!(payload.get("amount_minor").is_some());
    assert!(payload.get("currency").is_some());
}

#[tokio::test]
async fn past_due_event_payload_carries_triage_metadata() {
    // Operators paged by `subscription.past_due` need triage info to
    // act: WHY did it fail (last_error), WHEN was the last attempt
    // (last_attempt_at), and WHAT should they do (next_action).
    // Without these the alert is a paging-only signal and the
    // operator has to dig through DB or logs to do anything useful.
    let (state, app) = build(vec![1]).await; // 1-entry schedule → 2nd failure pasts_due
    let mut events = state.service.events.subscribe();
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let mut now = Utc::now();

    // Drain any setup events (subscription.created etc) so the
    // recv() below lands on the past_due event we care about.
    while events.try_recv().is_ok() {}

    // Two failures: first stays active (1-entry schedule has 1 retry
    // budget left), second exhausts → past_due.
    for _ in 0..2 {
        force_due(&state, &sub_id, now);
        let _ = state.service.tick_subscriptions(now).await;
        // Advance past the dunning backoff so the next tick re-picks.
        let s = state.service.subscriptions.get(&sub_id).unwrap();
        now = s.next_charge_at + Duration::seconds(1);
    }

    let post = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(post.status.wire_name(), "past_due");

    // Drain events looking for the past_due one.
    let mut past_due_payload: Option<Value> = None;
    while let Ok(ev) = events.try_recv() {
        if ev.r#type == "subscription.past_due" {
            past_due_payload = Some(ev.payload);
            break;
        }
    }
    let payload = past_due_payload.expect("subscription.past_due event must be emitted");

    // Pre-existing fields — kept for back-compat.
    assert_eq!(payload["subscription_id"], sub_id);
    assert!(payload["consecutive_failures"].as_u64().unwrap() >= 2);

    // New triage fields.
    assert_eq!(
        payload["attempts_made"], payload["consecutive_failures"],
        "attempts_made is the back-compat-friendly alias"
    );
    let last_err = payload["last_error"].as_str().unwrap_or("");
    assert!(
        !last_err.is_empty(),
        "last_error must surface the underlying charge failure (operators page on past_due and need to know WHY): got {last_err:?}"
    );
    assert!(
        payload["last_attempt_at"].is_string(),
        "last_attempt_at must be set so operators can scope log searches: got {:?}",
        payload["last_attempt_at"]
    );
    assert_eq!(
        payload["next_action"], "manual_renewal_required",
        "operator-actionable next_action lets handlers switch on it"
    );
}

#[tokio::test]
async fn empty_schedule_preserves_legacy_immediate_retry() {
    // Empty schedule = legacy behavior. The existing
    // tests/scheduler.rs::repeated_failures_transition_to_past_due
    // covers the integration; this one pins the local property:
    // the next_charge_at field is NOT touched on failure when no
    // schedule is configured.
    let (state, app) = build(vec![]).await;
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();
    let now = Utc::now();
    let pinned = now - Duration::seconds(1);
    force_due(&state, &sub_id, pinned);

    state.service.tick_subscriptions(now).await;
    let after = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(after.failed_renewal_attempts, 1);
    assert_eq!(
        after.next_charge_at, pinned,
        "with empty schedule, next_charge_at must remain at the value force_due set — \
         the legacy test relies on this so consecutive force_due calls work"
    );
}

#[tokio::test]
async fn long_dunning_window_works_correctly() {
    // Realistic Stripe-style schedule: 1 day, 3 days, 7 days. After
    // the third failure (the 4th would hit past_due), next_charge_at
    // should be 7 days out.
    let (state, app) = build(vec![24, 72, 168]).await;
    let resp = submit(&app, subscribe_body(always_failing_payment())).await;
    let sub_id = resp["subscription"]["id"].as_str().unwrap().to_string();

    let mut now = Utc::now();
    let expected = [
        Duration::hours(24),
        Duration::hours(72),
        Duration::hours(168),
    ];
    for (i, &delta) in expected.iter().enumerate() {
        force_due(&state, &sub_id, now);
        state.service.tick_subscriptions(now).await;
        let after = state.service.subscriptions.get(&sub_id).unwrap();
        assert_eq!(after.failed_renewal_attempts, (i as u32) + 1);
        assert_eq!(after.status.wire_name(), "active", "still recoverable");
        let target = now + delta;
        let drift_ms = (after.next_charge_at - target).num_milliseconds().abs();
        assert!(
            drift_ms < 100,
            "after failure {}: backoff should be {}h, drift {drift_ms}ms",
            i + 1,
            delta.num_hours()
        );
        now = target + Duration::seconds(1);
    }

    // Fourth failure → past_due.
    force_due(&state, &sub_id, now);
    state.service.tick_subscriptions(now).await;
    let final_state = state.service.subscriptions.get(&sub_id).unwrap();
    assert_eq!(final_state.status.wire_name(), "past_due");
    assert_eq!(final_state.failed_renewal_attempts, 4);
}
