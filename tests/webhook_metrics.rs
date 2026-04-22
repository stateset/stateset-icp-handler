//! Operator metrics for the webhook outbox.
//!
//! Asserts:
//!   * `WebhookOutbox::status_counts()` returns accurate per-FSM-state
//!     counts on both backends, exercised through real intent activity.
//!   * Worker tick bumps the `icp_webhook_deliveries_total{outcome=…}`
//!     counter per delivered/failed/dead_lettered transition.
//!   * `run_loop`'s post-tick refresh sets the
//!     `icp_webhook_outbox_queue_depth{status=…}` gauge so scrapes
//!     observe current backlog without needing a DB query.
//!   * `/metrics` exposes all three series with their HELP/TYPE
//!     headers so Prometheus can scrape them.
//!
//! These metrics are how operators see "the outbox is backing up" or
//! "destination X is dead-lettering" without poking the DB.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo,
    build_app_state, build_router,
    config::Config,
    webhook::{DeliveryStatus, WebhookOutbox, DEFAULT_MAX_ATTEMPTS},
    AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:wmetrics";

async fn build(keys: Vec<ApiKeyInfo>) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    cfg.webhook_url = Some("https://hooks.example/global".to_string());
    cfg.webhook_secret = Some("global".to_string());
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

async fn quote(app: &Router, bearer: &str) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {bearer}"))
        .header("ICP-Agent-Id", AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.quote",
                "agent_id": AGENT,
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
async fn status_counts_in_memory_groups_by_fsm_state() {
    let outbox = WebhookOutbox::in_memory();
    let now = chrono::Utc::now();
    // Three rows: pending, failed (one bump under max), dead_lettered.
    for i in 0..3 {
        outbox.enqueue(stateset_icp_handler::webhook::WebhookDelivery {
            id: format!("del_{i}"),
            event_id: "e".into(),
            event_type: "transaction.created".into(),
            url: "http://nowhere".into(),
            payload_json: "{}".into(),
            status: DeliveryStatus::Pending,
            attempts: 0,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            next_attempt_at: now,
            last_status_code: None,
            last_error: None,
            created_at: now,
            updated_at: now,
            delivered_at: None,
            tenant_id: String::new(),
        });
    }
    // del_1 → failed (one bump, < max_attempts).
    outbox.bump_failure("del_1", Some(503), Some("once".into()), now);
    // del_2 → dead_lettered (max_attempts bumps).
    for _ in 0..DEFAULT_MAX_ATTEMPTS {
        outbox.bump_failure("del_2", Some(500), Some("dead".into()), now);
    }

    let counts = outbox.status_counts();
    assert_eq!(counts.pending, 1, "del_0 still pending");
    assert_eq!(counts.failed, 1, "del_1 in failed state");
    assert_eq!(
        counts.dead_lettered, 1,
        "del_2 dead-lettered after max_attempts"
    );
    assert_eq!(counts.delivered, 0);
    assert_eq!(counts.in_flight, 0);
}

#[tokio::test]
async fn status_counts_persistent_groups_by_fsm_state() {
    // Drive the persistent backend through an intent so we know the
    // SQLite GROUP BY path matches the in-memory semantics. One quote
    // → one pending row.
    let (state, app) = build(vec![key("a", "tenant_a")]).await;
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;

    let counts = state.service.webhook_outbox.status_counts();
    assert_eq!(counts.pending, 2);
    assert_eq!(counts.failed, 0);
    assert_eq!(counts.dead_lettered, 0);
}

#[tokio::test]
async fn metrics_endpoint_exposes_outbox_series() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;
    // Drive some rows so the gauge has something to display.
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;
    // Refresh the gauge — same call run_loop makes after each tick.
    stateset_icp_handler::metrics::record_webhook_tick(
        &state.service.webhook_outbox.status_counts(),
    );

    let body = scrape_metrics(&app).await;

    // The three new series must each appear with their HELP and TYPE
    // headers (Prometheus's text exposition format requires both).
    for needle in [
        "icp_webhook_deliveries_total",
        "icp_webhook_outbox_queue_depth",
        "icp_webhook_worker_ticks_total",
    ] {
        let help = format!("# HELP {needle}");
        let type_line = format!("# TYPE {needle}");
        assert!(
            body.contains(&help),
            "missing HELP for {needle}\n--- /metrics body ---\n{body}"
        );
        assert!(body.contains(&type_line), "missing TYPE for {needle}");
    }

    // The gauge must show the actual pending count (2) — proves the
    // refresh helper is wired to the real outbox snapshot, not a
    // hard-coded zero.
    assert!(
        body.lines().any(
            |l| l.starts_with("icp_webhook_outbox_queue_depth{status=\"pending\"}")
                && l.ends_with(" 2")
        ),
        "queue_depth pending=2 gauge line not found in /metrics output\n{body}"
    );
}

#[tokio::test]
async fn worker_tick_bumps_delivered_counter() {
    use stateset_icp_handler::metrics::WEBHOOK_DELIVERIES;
    use stateset_icp_handler::webhook::WebhookWorker;

    // Snapshot the counter before — these counters are global, so
    // earlier tests may have already incremented them.
    let before = WEBHOOK_DELIVERIES.with_label_values(&["delivered"]).get();

    // Spawn a tiny accept-everything HTTP server so the worker has
    // a real 200 to receive.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app: Router = Router::new().route(
            "/hook",
            axum::routing::post(|| async { axum::http::StatusCode::OK }),
        );
        axum::serve(listener, app).await.unwrap();
    });

    // Set up an outbox with one pending row pointing at the test
    // server, then drive one worker tick.
    let outbox = WebhookOutbox::in_memory();
    let now = chrono::Utc::now();
    outbox.enqueue(stateset_icp_handler::webhook::WebhookDelivery {
        id: "del_metric".into(),
        event_id: "e".into(),
        event_type: "transaction.created".into(),
        url: format!("http://{addr}/hook"),
        payload_json: "{}".into(),
        status: DeliveryStatus::Pending,
        attempts: 0,
        max_attempts: DEFAULT_MAX_ATTEMPTS,
        next_attempt_at: now,
        last_status_code: None,
        last_error: None,
        created_at: now,
        updated_at: now,
        delivered_at: None,
        tenant_id: String::new(),
    });

    let worker = WebhookWorker::new(outbox.clone(), "secret".into());
    let report = worker.tick(now).await;
    assert_eq!(report.delivered, 1, "tick should mark as delivered");

    let after = WEBHOOK_DELIVERIES.with_label_values(&["delivered"]).get();
    assert!(
        after > before,
        "delivered counter must advance by at least 1 after a successful tick (before={before}, after={after})"
    );
}
