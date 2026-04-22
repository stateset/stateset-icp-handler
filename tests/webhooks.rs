//! Webhook delivery integration tests.
//!
//! Spins up an in-process axum receiver that captures incoming
//! deliveries and verifies the signature. Drives the full path:
//! intent submission → outbox enqueue → worker tick → HTTP POST →
//! receiver. Covers:
//!   * Successful delivery, signature verification, headers
//!   * Retry with backoff after 5xx
//!   * Dead-letter after exceeding max_attempts
//!   * Persistence: deliveries survive restart and resume from outbox
//!   * Read endpoints: GET list + GET by id
//!   * Worker honors no-secret config (refuses to spawn) — tested via
//!     manual `WebhookWorker` call
//!   * Disabled when no `webhook_url` configured

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use stateset_icp_handler::webhook::{verify, WebhookOutbox, WebhookSubscriber, WebhookWorker};
use stateset_icp_handler::{build_app_state, build_router, config::Config, AppState};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tokio::net::TcpListener;
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";
const DEMO_AGENT: &str = "did:stateset:agent:webhooks-test";
const SECRET: &str = "test-webhook-secret";

// --------------------------------------------------------------------------
// Mock receiver
// --------------------------------------------------------------------------

#[derive(Clone, Default)]
struct ReceivedDelivery {
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct ReceiverState {
    received: Arc<Mutex<Vec<ReceivedDelivery>>>,
    /// 0-indexed: response[i] is the status to return on the i'th call.
    /// Falls back to 200 once exhausted.
    status_sequence: Arc<Mutex<Vec<u16>>>,
    call_count: Arc<AtomicU32>,
}

async fn receive(
    State(state): State<ReceiverState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let n = state.call_count.fetch_add(1, Ordering::SeqCst) as usize;
    state.received.lock().unwrap().push(ReceivedDelivery {
        headers: headers.clone(),
        body: body.to_vec(),
    });
    let status_code = state
        .status_sequence
        .lock()
        .unwrap()
        .get(n)
        .copied()
        .unwrap_or(200);
    let status = http::StatusCode::from_u16(status_code).unwrap_or(http::StatusCode::OK);
    (status, "")
}

struct Receiver {
    addr: SocketAddr,
    state: ReceiverState,
    handle: tokio::task::JoinHandle<()>,
}

impl Receiver {
    async fn start(initial_status_sequence: Vec<u16>) -> Self {
        let state = ReceiverState {
            received: Arc::new(Mutex::new(Vec::new())),
            status_sequence: Arc::new(Mutex::new(initial_status_sequence)),
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

    fn last(&self) -> Option<ReceivedDelivery> {
        self.state.received.lock().unwrap().last().cloned()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// --------------------------------------------------------------------------
// Handler harness
// --------------------------------------------------------------------------

async fn build_with_url(url: &str) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.webhook_url = Some(url.to_string());
    cfg.webhook_secret = Some(SECRET.to_string());
    let state = build_app_state(&cfg).await.expect("state");
    let router = build_router(state.clone());
    (state, router)
}

async fn submit_quote(app: &Router) -> Value {
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
                "params": {
                    "items": [{
                        "sku": "WIDGET-001", "quantity": 1,
                        "unit_price_hint": { "amount_minor": 1500, "currency": "USD" }
                    }]
                }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[tokio::test]
async fn quote_enqueues_a_pending_delivery() {
    let receiver = Receiver::start(vec![]).await;
    let (state, app) = build_with_url(&receiver.url()).await;

    let _ = submit_quote(&app).await;

    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].status.wire_name(), "pending");
    assert_eq!(recent[0].url, receiver.url());
    assert_eq!(recent[0].event_type, "transaction.quoted");
    assert_eq!(recent[0].max_attempts, 5);
    assert_eq!(recent[0].attempts, 0);
}

#[tokio::test]
async fn worker_tick_delivers_and_signature_verifies() {
    let receiver = Receiver::start(vec![]).await; // 200 default
    let (state, app) = build_with_url(&receiver.url()).await;
    let _ = submit_quote(&app).await;

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    let report = worker.tick(Utc::now()).await;
    assert_eq!(report.due, 1);
    assert_eq!(report.delivered, 1);
    assert_eq!(report.failed, 0);

    assert_eq!(receiver.count(), 1);
    let last = receiver.last().unwrap();
    let sig = last
        .headers
        .get("icp-signature")
        .expect("signature header present")
        .to_str()
        .unwrap();
    assert!(verify(SECRET, sig, &last.body), "signature must verify");
    assert_eq!(
        last.headers
            .get("icp-event-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "transaction.quoted"
    );
    assert!(last.headers.get("icp-event-id").is_some());
    assert!(last.headers.get("icp-delivery-id").is_some());
    assert_eq!(
        last.headers
            .get("icp-delivery-attempt")
            .unwrap()
            .to_str()
            .unwrap(),
        "1"
    );

    // Outbox row is now Delivered.
    let stored = state.service.webhook_outbox.list_recent(10);
    assert_eq!(stored[0].status.wire_name(), "delivered");
    assert_eq!(stored[0].last_status_code, Some(200));
    assert!(stored[0].delivered_at.is_some());
}

#[tokio::test]
async fn worker_delivers_per_tenant_subscriber_without_global_secret() {
    let receiver = Receiver::start(vec![]).await;
    let mut cfg = Config::for_test();
    cfg.webhook_url = None;
    cfg.webhook_secret = None;
    let state = build_app_state(&cfg).await.expect("state");
    state.service.webhook_subscribers.insert(WebhookSubscriber {
        id: "whsub_test".into(),
        tenant_id: "merchant_demo".into(),
        url: receiver.url(),
        secret: Some("tenant-secret".into()),
        active: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    });
    let app = build_router(state.clone());
    let _ = submit_quote(&app).await;

    let worker =
        WebhookWorker::new_with_optional_secret(state.service.webhook_outbox.clone(), None)
            .with_subscribers(state.service.webhook_subscribers.clone());
    let report = worker.tick(Utc::now()).await;
    assert_eq!(report.due, 1);
    assert_eq!(report.delivered, 1);
    assert_eq!(receiver.count(), 1);

    let last = receiver.last().unwrap();
    let sig = last
        .headers
        .get("icp-signature")
        .expect("signature header present")
        .to_str()
        .unwrap();
    assert!(
        verify("tenant-secret", sig, &last.body),
        "subscriber-specific secret must sign the delivery"
    );
}

#[tokio::test]
async fn signature_does_not_verify_with_wrong_secret() {
    let receiver = Receiver::start(vec![]).await;
    let (state, app) = build_with_url(&receiver.url()).await;
    let _ = submit_quote(&app).await;

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    let _ = worker.tick(Utc::now()).await;

    let last = receiver.last().unwrap();
    let sig = last.headers.get("icp-signature").unwrap().to_str().unwrap();
    assert!(
        !verify("a different secret", sig, &last.body),
        "verify with wrong secret must fail"
    );
}

#[tokio::test]
async fn server_5xx_marks_failed_and_schedules_retry() {
    // Receiver returns 503 forever — every attempt fails.
    let receiver = Receiver::start(vec![503; 10]).await;
    let (state, app) = build_with_url(&receiver.url()).await;
    let _ = submit_quote(&app).await;

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    let report = worker.tick(Utc::now()).await;
    assert_eq!(report.due, 1);
    assert_eq!(report.delivered, 0);
    assert_eq!(report.failed, 1);

    let stored = state.service.webhook_outbox.list_recent(10);
    assert_eq!(stored[0].status.wire_name(), "failed");
    assert_eq!(stored[0].attempts, 1);
    assert_eq!(stored[0].last_status_code, Some(503));
    // Backoff scheduled — next_attempt_at must be in the future.
    assert!(stored[0].next_attempt_at > Utc::now());
}

#[tokio::test]
async fn exceeding_max_attempts_dead_letters() {
    let receiver = Receiver::start(vec![500; 100]).await;
    let (state, app) = build_with_url(&receiver.url()).await;
    let _ = submit_quote(&app).await;

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());

    // Force 5 ticks (the default max_attempts). Force `now` past the
    // backoff schedule each time so the same row gets re-attempted.
    let far_future = Utc::now() + chrono::Duration::days(1);
    for attempt in 1..=5u32 {
        let r = worker.tick(far_future).await;
        if attempt < 5 {
            assert_eq!(r.failed, 1, "attempt {attempt}: failure recorded");
            assert_eq!(r.dead_lettered, 0);
        } else {
            // Final attempt transitions to dead_lettered.
            assert_eq!(r.dead_lettered, 1, "final attempt dead-letters");
        }
    }
    let stored = state.service.webhook_outbox.list_recent(10);
    assert_eq!(stored[0].status.wire_name(), "dead_lettered");
    assert_eq!(stored[0].attempts, 5);

    // Dead-lettered rows are not picked up again.
    let r = worker.tick(far_future).await;
    assert_eq!(r.due, 0);
}

#[tokio::test]
async fn delivery_survives_restart_via_persistent_outbox() {
    let receiver = Receiver::start(vec![]).await;
    let path = format!("/tmp/icp_webhook_test_{}.db", uuid::Uuid::new_v4().simple());
    let mut cfg = Config::for_test();
    cfg.state_db_path = path.clone();
    cfg.webhook_url = Some(receiver.url());
    cfg.webhook_secret = Some(SECRET.to_string());

    // Phase 1: handler boots, ingests an intent, enqueues a delivery,
    // then "crashes" before the worker drains.
    {
        let state = build_app_state(&cfg).await.unwrap();
        let app = build_router(state);
        let _ = submit_quote(&app).await;
    }
    assert_eq!(receiver.count(), 0, "no deliveries yet — worker hasn't run");

    // Phase 2: handler restarts against the same DB. Outbox row is
    // still there, worker drains it.
    let (state2, _app2) = {
        let s = build_app_state(&cfg).await.unwrap();
        let r = build_router(s.clone());
        (s, r)
    };
    let recent = state2.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 1, "delivery survived restart");
    assert_eq!(recent[0].status.wire_name(), "pending");

    let worker = WebhookWorker::new(state2.service.webhook_outbox.clone(), SECRET.to_string());
    let report = worker.tick(Utc::now()).await;
    assert_eq!(report.delivered, 1);
    assert_eq!(receiver.count(), 1);

    // Cleanup.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-shm"));
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

#[tokio::test]
async fn list_and_get_endpoints_expose_outbox() {
    let receiver = Receiver::start(vec![]).await;
    let (_, app) = build_with_url(&receiver.url()).await;
    let _ = submit_quote(&app).await;

    // GET /icp/v1/webhook_deliveries
    let req = Request::builder()
        .method("GET")
        .uri("/icp/v1/webhook_deliveries")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", DEMO_AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    let id = data[0]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("del_"));
    assert_eq!(data[0]["status"], "pending");

    // GET /icp/v1/webhook_deliveries/:id
    let req = Request::builder()
        .method("GET")
        .uri(format!("/icp/v1/webhook_deliveries/{id}"))
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", DEMO_AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let one: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(one["id"], id);
}

#[tokio::test]
async fn no_webhook_url_means_no_outbox_writes() {
    // Default Config::for_test has webhook_url = None — service should
    // not enqueue.
    let state = build_app_state(&Config::for_test()).await.unwrap();
    let app = build_router(state.clone());
    let _ = submit_quote(&app).await;
    assert_eq!(
        state.service.webhook_outbox.len(),
        0,
        "no webhook_url → no outbox writes"
    );
}

#[tokio::test]
async fn read_only_intents_do_not_enqueue() {
    let receiver = Receiver::start(vec![]).await;
    let (state, app) = build_with_url(&receiver.url()).await;

    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", DEMO_AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.search",
                "agent_id": DEMO_AGENT,
                "params": { "query": "widget" }
            })
            .to_string(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // intent.search is a read; it should NOT enqueue a webhook.
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 0, "read-only intents must not enqueue");
}

#[tokio::test]
async fn outbox_in_memory_basic_construction() {
    // Sanity for the in-memory backend (the SQLite backend is exercised
    // by the persistence test above).
    let outbox = WebhookOutbox::in_memory();
    assert_eq!(outbox.len(), 0);
    assert!(outbox.is_empty());
    assert_eq!(outbox.list_recent(10).len(), 0);
    assert_eq!(outbox.list_due(Utc::now(), 10).len(), 0);
    assert!(outbox.get("missing").is_none());
}

#[tokio::test]
async fn background_run_loop_drains_pending_deliveries() {
    let receiver = Receiver::start(vec![]).await;
    let (state, app) = build_with_url(&receiver.url()).await;
    let _ = submit_quote(&app).await;
    assert_eq!(receiver.count(), 0);

    let worker = WebhookWorker::new(state.service.webhook_outbox.clone(), SECRET.to_string());
    let task = tokio::spawn(stateset_icp_handler::webhook::run_loop(
        worker,
        StdDuration::from_millis(50),
    ));

    let started = std::time::Instant::now();
    while receiver.count() == 0 {
        if started.elapsed() > StdDuration::from_secs(2) {
            task.abort();
            panic!("background webhook worker did not deliver within 2s");
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    task.abort();
    assert_eq!(receiver.count(), 1);
}
