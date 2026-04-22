//! Operator-retry endpoint for webhook deliveries.
//!
//! Drives the full path: enqueue → fail repeatedly → dead-lettered →
//! `POST /icp/v1/webhook_deliveries/:id/retry` → pending → next worker
//! tick delivers (against a now-healthy receiver). Plus the four
//! refusal cases (404 on unknown id, 412 on wrong-state retries).

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use stateset_icp_handler::webhook::{DeliveryStatus, WebhookOutbox, WebhookWorker};
use stateset_icp_handler::{build_app_state, build_router, config::Config, AppState};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";
const DEMO_AGENT: &str = "did:stateset:agent:retry-test";
const SECRET: &str = "test-webhook-secret";

// --------------------------------------------------------------------------
// Mock receiver — same shape as tests/webhooks.rs but local to this file
// to keep the test crate-independent.
// --------------------------------------------------------------------------

#[derive(Clone)]
struct ReceiverState {
    status_sequence: Arc<Mutex<Vec<u16>>>,
    call_count: Arc<AtomicU32>,
}

async fn receive(
    State(state): State<ReceiverState>,
    _headers: HeaderMap,
    _body: axum::body::Bytes,
) -> impl IntoResponse {
    let n = state.call_count.fetch_add(1, Ordering::SeqCst) as usize;
    let code = state
        .status_sequence
        .lock()
        .unwrap()
        .get(n)
        .copied()
        .unwrap_or(200);
    let status = http::StatusCode::from_u16(code).unwrap_or(http::StatusCode::OK);
    (status, "")
}

struct Receiver {
    addr: SocketAddr,
    state: ReceiverState,
    handle: tokio::task::JoinHandle<()>,
}

impl Receiver {
    async fn start(initial: Vec<u16>) -> Self {
        let state = ReceiverState {
            status_sequence: Arc::new(Mutex::new(initial)),
            call_count: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/webhook", post(receive))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::task::yield_now().await;
        Self {
            addr,
            state,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/webhook", self.addr)
    }

    fn count(&self) -> u32 {
        self.state.call_count.load(Ordering::SeqCst)
    }

    /// Replace the future status sequence — lets a test simulate
    /// "operator fixed the bug, receiver is now healthy."
    fn set_status_sequence(&self, codes: Vec<u16>) {
        let mut guard = self.state.status_sequence.lock().unwrap();
        guard.clear();
        guard.extend(codes);
        // Reset the call counter so the new sequence is consumed from
        // index 0, otherwise the worker would jump past it.
        self.state.call_count.store(0, Ordering::SeqCst);
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn build_with(url: &str) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.webhook_url = Some(url.to_string());
    cfg.webhook_secret = Some(SECRET.to_string());
    let state = build_app_state(&cfg).await.unwrap();
    let router = build_router(state.clone());
    (state, router)
}

async fn quote(app: &Router) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", DEMO_AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.quote",
                "agent_id": DEMO_AGENT,
                "params": { "items": [{
                    "sku": "WIDGET-001", "quantity": 1,
                    "unit_price_hint": { "amount_minor": 1500, "currency": "USD" }
                }] }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn retry_call(app: &Router, delivery_id: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/icp/v1/webhook_deliveries/{delivery_id}/retry"))
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", DEMO_AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[tokio::test]
async fn dead_lettered_can_be_manually_retried_and_then_delivers() {
    // Receiver returns 500 forever for the first 5 attempts → row gets
    // dead-lettered. Then the operator "fixes" the receiver, calls
    // retry, and the next worker tick delivers.
    let receiver = Receiver::start(vec![500; 5]).await;
    let (state, app) = build_with(&receiver.url()).await;

    // Trigger the enqueue.
    let _ = quote(&app).await;
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 1);
    let id = recent[0].id.clone();

    // 5 forced ticks → dead_lettered.
    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    let far_future = Utc::now() + chrono::Duration::days(1);
    for _ in 0..5 {
        worker.tick(far_future).await;
    }
    let after_failures = state.service.webhook_outbox.get(&id).unwrap();
    assert_eq!(after_failures.status, DeliveryStatus::DeadLettered);
    assert_eq!(after_failures.attempts, 5);

    // Operator "fixes" the receiver.
    receiver.set_status_sequence(vec![200; 10]);

    // Retry endpoint flips the row back to Pending with attempts=0.
    let (status, body) = retry_call(&app, &id).await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["attempts"], 0);
    assert!(body["last_error"].is_null());
    assert!(body["last_status_code"].is_null());

    // Worker tick now delivers.
    let report = worker.tick(Utc::now()).await;
    assert_eq!(report.delivered, 1, "post-retry delivery should succeed");
    let final_state = state.service.webhook_outbox.get(&id).unwrap();
    assert_eq!(final_state.status, DeliveryStatus::Delivered);
    assert_eq!(final_state.last_status_code, Some(200));
    assert_eq!(
        receiver.count(),
        1,
        "exactly one POST hit the healthy receiver"
    );
}

#[tokio::test]
async fn failed_can_be_manually_retried_short_circuiting_backoff() {
    // After 1 failure the row is `Failed` with backoff. Operator
    // doesn't want to wait — calls retry, immediate re-enqueue.
    let receiver = Receiver::start(vec![503; 1]).await;
    let (state, app) = build_with(&receiver.url()).await;
    let _ = quote(&app).await;
    let id = state.service.webhook_outbox.list_recent(10)[0].id.clone();

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    worker.tick(Utc::now()).await;
    let after = state.service.webhook_outbox.get(&id).unwrap();
    assert_eq!(after.status, DeliveryStatus::Failed);
    assert_eq!(after.attempts, 1);
    assert!(after.next_attempt_at > Utc::now(), "backoff scheduled");

    receiver.set_status_sequence(vec![200]);
    let (status, body) = retry_call(&app, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "pending");
    let after_retry = state.service.webhook_outbox.get(&id).unwrap();
    assert!(
        after_retry.next_attempt_at <= Utc::now() + chrono::Duration::seconds(1),
        "retry must collapse the backoff to immediate"
    );

    let report = worker.tick(Utc::now()).await;
    assert_eq!(report.delivered, 1);
}

#[tokio::test]
async fn retry_unknown_id_returns_404() {
    let receiver = Receiver::start(vec![]).await;
    let (_state, app) = build_with(&receiver.url()).await;
    let (status, body) = retry_call(&app, "del_does_not_exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "resource_not_found");
}

#[tokio::test]
async fn retry_pending_delivery_returns_412() {
    let receiver = Receiver::start(vec![]).await;
    let (state, app) = build_with(&receiver.url()).await;
    let _ = quote(&app).await;
    let id = state.service.webhook_outbox.list_recent(10)[0].id.clone();
    // Without driving the worker, the row is still `pending`.
    let (status, body) = retry_call(&app, &id).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already pending"));
}

#[tokio::test]
async fn retry_delivered_returns_412() {
    let receiver = Receiver::start(vec![]).await; // 200 default
    let (state, app) = build_with(&receiver.url()).await;
    let _ = quote(&app).await;
    let id = state.service.webhook_outbox.list_recent(10)[0].id.clone();

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    worker.tick(Utc::now()).await;
    assert_eq!(
        state.service.webhook_outbox.get(&id).unwrap().status,
        DeliveryStatus::Delivered
    );

    let (status, body) = retry_call(&app, &id).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already succeeded"));
}

#[tokio::test]
async fn outbox_unit_refuses_in_flight_retries() {
    // Direct API test — race-with-worker semantics. We can't easily
    // catch the worker mid-flight from outside, so we test the unit
    // by mutating to InFlight directly.
    let outbox = WebhookOutbox::in_memory();
    let now = Utc::now();
    outbox.enqueue(stateset_icp_handler::webhook::WebhookDelivery {
        id: "del_x".into(),
        event_id: "e".into(),
        event_type: "transaction.completed".into(),
        url: "http://nowhere".into(),
        payload_json: "{}".into(),
        status: DeliveryStatus::Pending,
        attempts: 0,
        max_attempts: 3,
        next_attempt_at: now,
        last_status_code: None,
        last_error: None,
        created_at: now,
        updated_at: now,
        delivered_at: None,
    });
    outbox.mark_in_flight("del_x", now);
    let err = outbox.reset_for_retry("del_x", now).unwrap_err();
    assert_eq!(
        err,
        stateset_icp_handler::webhook::RetryError::InFlight,
        "in_flight retry must be refused"
    );
}

#[tokio::test]
async fn retry_resets_attempt_counter_so_next_failure_starts_fresh() {
    // After the operator retries, the row should behave as if it were
    // freshly enqueued — which means it gets the full max_attempts
    // budget on the next failure cycle.
    let receiver = Receiver::start(vec![500; 5]).await;
    let (state, app) = build_with(&receiver.url()).await;
    let _ = quote(&app).await;
    let id = state.service.webhook_outbox.list_recent(10)[0].id.clone();

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    let far_future = Utc::now() + chrono::Duration::days(1);
    for _ in 0..5 {
        worker.tick(far_future).await;
    }
    let dead = state.service.webhook_outbox.get(&id).unwrap();
    assert_eq!(dead.status, DeliveryStatus::DeadLettered);

    // Retry — receiver still broken so it'll re-fail.
    let (status, body) = retry_call(&app, &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["attempts"], 0, "attempts MUST reset on retry");

    // Drive 4 more failures — should NOT yet be dead-lettered (max=5).
    receiver.set_status_sequence(vec![500; 5]);
    for _ in 0..4 {
        worker.tick(far_future).await;
    }
    let after_4_more = state.service.webhook_outbox.get(&id).unwrap();
    assert_eq!(
        after_4_more.status,
        DeliveryStatus::Failed,
        "retry budget must restart from zero, so 4 fresh failures stay below max=5"
    );
    assert_eq!(after_4_more.attempts, 4);
}
