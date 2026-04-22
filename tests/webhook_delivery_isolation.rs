//! Tenant scoping + status filtering for `/icp/v1/webhook_deliveries`.
//!
//! Asserts:
//!   * `GET /icp/v1/webhook_deliveries` returns only the caller's
//!     tenant's deliveries — never another tenant's payloads.
//!   * `GET /icp/v1/webhook_deliveries/:id` on a cross-tenant id
//!     returns 404 (existence not leaked, identical to a missing row).
//!   * `POST /icp/v1/webhook_deliveries/:id/retry` likewise 404s on a
//!     cross-tenant id.
//!   * `?status=` narrows by status; unknown status → 400.
//!   * `?limit=` caps page size and is bounded by an internal max
//!     (so a tenant can't request a million rows in one call).
//!   * Each enqueued delivery row carries the originating `tenant_id`
//!     so the property is observable end-to-end (intent → outbox row).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo, build_app_state, build_router, config::Config, AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:wd-isolation";

async fn build(keys: Vec<ApiKeyInfo>) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    // A global fallback so tenants without subscribers still produce
    // outbox rows — keeps the test signal independent of the
    // per-tenant subscriber CRUD.
    cfg.webhook_url = Some("https://hooks.example/global".to_string());
    cfg.webhook_secret = Some("global-secret".to_string());
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

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    bearer: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {bearer}"))
        .header("ICP-Agent-Id", AGENT);
    let body = match body {
        Some(b) => {
            builder = builder.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let req = builder.body(body).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

async fn quote(app: &Router, bearer: &str) -> Value {
    send(
        app,
        "POST",
        "/icp/v1/intents",
        bearer,
        Some(json!({
            "intent": "intent.quote",
            "agent_id": AGENT,
            "params": { "items": [{
                "sku": "WIDGET-001", "quantity": 1,
                "unit_price_hint": { "amount_minor": 1500, "currency": "USD" }
            }] }
        })),
    )
    .await
    .1
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_only_callers_tenant_rows() {
    let (state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    // Tenant A produces 2 deliveries, tenant B produces 1.
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_b").await;

    // Sanity: outbox has 3 rows total, tagged correctly.
    let all = state.service.webhook_outbox.list_recent(10);
    assert_eq!(all.len(), 3);
    let a_count = all.iter().filter(|d| d.tenant_id == "tenant_a").count();
    let b_count = all.iter().filter(|d| d.tenant_id == "tenant_b").count();
    assert_eq!(a_count, 2, "tenant_id stamped at enqueue time for A");
    assert_eq!(b_count, 1, "tenant_id stamped at enqueue time for B");

    // Tenant A's list shows 2.
    let (status, body) = send(&app, "GET", "/icp/v1/webhook_deliveries", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 2);
    for row in body["data"].as_array().unwrap() {
        assert_eq!(row["tenant_id"], "tenant_a");
    }

    // Tenant B's list shows 1 — and never sees A's payloads.
    let (status, body) = send(&app, "GET", "/icp/v1/webhook_deliveries", "k_b", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["tenant_id"], "tenant_b");
}

#[tokio::test]
async fn cross_tenant_get_returns_404_not_403() {
    let (state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    // Tenant B produces one delivery; capture its id.
    let _ = quote(&app, "k_b").await;
    let b_id = state.service.webhook_outbox.list_recent(10)[0].id.clone();

    // Tenant A tries to read it → must 404, not 403, because
    // surfacing 403 would confirm the id exists for some other tenant
    // and let A enumerate B's id space.
    let (status, _) = send(
        &app,
        "GET",
        &format!("/icp/v1/webhook_deliveries/{b_id}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant get must look identical to a missing row"
    );

    // Tenant B reading it works.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/icp/v1/webhook_deliveries/{b_id}"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], b_id);
}

#[tokio::test]
async fn cross_tenant_retry_returns_404_not_412() {
    let (state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let _ = quote(&app, "k_b").await;
    let b_id = state.service.webhook_outbox.list_recent(10)[0].id.clone();

    // Force the row into the `failed` state so retry would normally
    // be valid for the rightful tenant.
    state.service.webhook_outbox.bump_failure(
        &b_id,
        Some(503),
        Some("test".into()),
        chrono::Utc::now(),
    );

    // Tenant A tries to retry tenant B's failure → 404 (NOT 412
    // "wrong state"), so A can't even tell whether the id exists.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_deliveries/{b_id}/retry"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant retry must surface as 404, not 412"
    );

    // Tenant B's own retry works.
    let (status, body) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_deliveries/{b_id}/retry"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "pending");
}

#[tokio::test]
async fn status_filter_narrows_by_state() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // Three rows for tenant A: one stays pending, one gets bumped to
    // failed, one to dead_lettered.
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;
    let ids: Vec<String> = state
        .service
        .webhook_outbox
        .list_recent(10)
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids.len(), 3);

    // Bump #2 to failed (one failure < max_attempts).
    state.service.webhook_outbox.bump_failure(
        &ids[1],
        Some(503),
        Some("once".into()),
        chrono::Utc::now(),
    );
    // Bump #3 past max_attempts → dead_lettered.
    for _ in 0..stateset_icp_handler::webhook::DEFAULT_MAX_ATTEMPTS {
        state.service.webhook_outbox.bump_failure(
            &ids[2],
            Some(500),
            Some("dead".into()),
            chrono::Utc::now(),
        );
    }

    // ?status=pending → 1
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?status=pending",
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "pending");

    // ?status=failed → 1
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?status=failed",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "failed");

    // ?status=dead_lettered → 1
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?status=dead_lettered",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "dead_lettered");

    // ?status=delivered → 0 (no real delivery happened in this test)
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?status=delivered",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn unknown_status_filter_is_400() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?status=lolwut",
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "typo in status filter must surface fast, not silently empty"
    );
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("lolwut"),
        "error must echo the bad value: {msg}"
    );
}

#[tokio::test]
async fn limit_query_caps_page_size() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // 5 deliveries.
    for _ in 0..5 {
        let _ = quote(&app, "k_a").await;
    }
    assert_eq!(state.service.webhook_outbox.list_recent(100).len(), 5);

    // Default cap (100) returns all 5.
    let (_, body) = send(&app, "GET", "/icp/v1/webhook_deliveries", "k_a", None).await;
    assert_eq!(body["count"], 5);

    // ?limit=2 returns 2.
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?limit=2",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 2);

    // ?limit=10000 is silently clamped to the 500 hard cap (not an
    // error — just bounded so a misbehaving caller can't OOM the
    // handler with a single request).
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/webhook_deliveries?limit=10000",
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // We have 5 rows; the cap doesn't reduce that below the actual
    // count, so we just assert the request succeeded and returned all
    // existing rows.
    assert_eq!(body["count"], 5);
}

#[tokio::test]
async fn unauthenticated_list_is_rejected() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let req = Request::builder()
        .method("GET")
        .uri("/icp/v1/webhook_deliveries")
        .header("ICP-Agent-Id", AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
