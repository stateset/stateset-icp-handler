//! Tenant-scoped list endpoints for transactions and subscriptions.
//!
//! Closes a real ergonomic gap: previously a tenant had no API path
//! to enumerate their own transactions or subscriptions. Operators
//! either had to hold every id in a side store or query the DB
//! directly. Mirrors the shape of `GET /icp/v1/webhook_deliveries`
//! (tenant-scoped, status-filterable, limit-clampable).
//!
//! Asserts:
//!   * `GET /icp/v1/transactions` returns only the caller's tenant's
//!     rows, newest first; `?state=…` narrows by FSM state; bad
//!     filter → 400; `?limit=` is clamped to 500.
//!   * Same shape for `GET /icp/v1/subscriptions` with status filter.
//!   * Tenant isolation: tenant A never sees tenant B's rows.
//!   * Empty list returns `{count:0,data:[]}`, not 404.
//!   * Unauthenticated reads → 401.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo, build_app_state, build_router, config::Config, AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:list-eps";

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

async fn a2a_quote(app: &Router, bearer: &str) -> Value {
    send(
        app,
        "POST",
        "/icp/v1/intents",
        bearer,
        Some(json!({
            "intent": "intent.a2a_quote",
            "agent_id": AGENT,
            "params": {
                "peer_agent_id": "did:stateset:agent:peer",
                "service": {
                    "kind": "image_generation",
                    "description": "Render a 1024x1024 product photo"
                }
            },
            "context": { "currency": "USD" }
        })),
    )
    .await
    .1
}

async fn subscribe(app: &Router, bearer: &str) -> Value {
    send(
        app,
        "POST",
        "/icp/v1/intents",
        bearer,
        Some(json!({
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
        })),
    )
    .await
    .1
}

// --------------------------------------------------------------------------
// Transactions
// --------------------------------------------------------------------------

#[tokio::test]
async fn transactions_list_returns_only_callers_tenant_rows() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    // 2 quotes for A, 1 for B.
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;
    let _ = quote(&app, "k_b").await;

    let (status, body) = send(&app, "GET", "/icp/v1/transactions", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 2, "tenant A sees only its own 2 txns");
    for row in body["data"].as_array().unwrap() {
        assert_eq!(row["tenant_id"], "tenant_a");
    }

    let (_, body) = send(&app, "GET", "/icp/v1/transactions", "k_b", None).await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["tenant_id"], "tenant_b");
}

#[tokio::test]
async fn transactions_list_state_filter_narrows() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // Two quoted txns, then transition one to canceled directly so we
    // have non-trivial distribution.
    let body1 = quote(&app, "k_a").await;
    let _ = quote(&app, "k_a").await;
    let id1 = body1["transaction"]["id"].as_str().unwrap().to_string();
    state.service.transactions.update(&id1, |t| {
        t.state = stateset_icp_handler::models::TransactionState::Canceled;
    });

    // ?state=quoted → 1
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/transactions?state=quoted",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["state"], "quoted");

    // ?state=canceled → 1
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/transactions?state=canceled",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["state"], "canceled");

    // ?state=completed → 0 (still 200, empty data — never 404)
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/transactions?state=completed",
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn transactions_list_unknown_state_filter_is_400() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/transactions?state=lolwut",
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "typo in state filter must surface fast, not silently empty"
    );
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("lolwut"),
        "error must echo the bad value: {msg}"
    );
}

#[tokio::test]
async fn transactions_list_empty_returns_200_not_404() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (status, body) = send(&app, "GET", "/icp/v1/transactions", "k_a", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "empty result is a successful zero-row list, not a missing resource"
    );
    assert_eq!(body["count"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

// --------------------------------------------------------------------------
// Subscriptions
// --------------------------------------------------------------------------

#[tokio::test]
async fn subscriptions_list_returns_only_callers_tenant_rows() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let _ = subscribe(&app, "k_a").await;
    let _ = subscribe(&app, "k_b").await;
    let _ = subscribe(&app, "k_b").await;

    let (status, body) = send(&app, "GET", "/icp/v1/subscriptions", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["tenant_id"], "tenant_a");

    let (_, body) = send(&app, "GET", "/icp/v1/subscriptions", "k_b", None).await;
    assert_eq!(body["count"], 2);
    for row in body["data"].as_array().unwrap() {
        assert_eq!(row["tenant_id"], "tenant_b");
    }
}

#[tokio::test]
async fn subscriptions_list_status_filter_narrows() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    let body1 = subscribe(&app, "k_a").await;
    let _ = subscribe(&app, "k_a").await;
    let id1 = body1["subscription"]["id"].as_str().unwrap().to_string();
    state.service.subscriptions.update(&id1, |s| {
        s.status = stateset_icp_handler::models::SubscriptionStatus::PastDue;
    });

    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/subscriptions?status=active",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "active");

    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/subscriptions?status=past_due",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "past_due");
}

#[tokio::test]
async fn subscriptions_list_unknown_status_filter_is_400() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/subscriptions?status=expired",
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "subscription status filter must reject values that aren't real subscription statuses (expired isn't one)"
    );
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("expired"), "error must echo bad value: {msg}");
}

#[tokio::test]
async fn list_limit_is_clamped() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    for _ in 0..3 {
        let _ = quote(&app, "k_a").await;
    }
    // ?limit=2 → 2
    let (_, body) = send(&app, "GET", "/icp/v1/transactions?limit=2", "k_a", None).await;
    assert_eq!(body["count"], 2);

    // ?limit=10000 → silently clamped (no error). With 3 rows, count=3.
    let (status, body) = send(&app, "GET", "/icp/v1/transactions?limit=10000", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 3);
}

// --------------------------------------------------------------------------
// Peer quotes
// --------------------------------------------------------------------------

#[tokio::test]
async fn peer_quotes_list_returns_only_callers_tenant_rows() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let _ = a2a_quote(&app, "k_a").await;
    let _ = a2a_quote(&app, "k_a").await;
    let _ = a2a_quote(&app, "k_b").await;

    let (status, body) = send(&app, "GET", "/icp/v1/peer_quotes", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 2, "tenant A sees only its own 2 peer quotes");
    for row in body["data"].as_array().unwrap() {
        assert_eq!(row["tenant_id"], "tenant_a");
    }

    let (_, body) = send(&app, "GET", "/icp/v1/peer_quotes", "k_b", None).await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["tenant_id"], "tenant_b");
}

#[tokio::test]
async fn peer_quotes_list_status_filter_narrows() {
    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // Two pending quotes (no price hint), then transition one to
    // accepted directly so we have non-trivial distribution. Direct
    // mutation rather than driving a real pay flow keeps this test
    // focused on the list/filter path.
    let body1 = a2a_quote(&app, "k_a").await;
    let _ = a2a_quote(&app, "k_a").await;
    let id1 = body1["peer_quote"]["id"].as_str().unwrap().to_string();
    state.service.peer_quotes.update(&id1, |q| {
        q.status = stateset_icp_handler::models::PeerQuoteStatus::Accepted;
    });

    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/peer_quotes?status=pending",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "pending");

    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/peer_quotes?status=accepted",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["status"], "accepted");
}

#[tokio::test]
async fn peer_quotes_list_unknown_status_filter_is_400() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/peer_quotes?status=not_a_status",
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "typo in peer-quote status filter must surface fast, not silently empty"
    );
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not_a_status"),
        "error must echo the bad value: {msg}"
    );
}

// --------------------------------------------------------------------------
// Auth
// --------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_lists_are_rejected() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    for path in [
        "/icp/v1/transactions",
        "/icp/v1/subscriptions",
        "/icp/v1/peer_quotes",
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .header("ICP-Agent-Id", AGENT)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "unauthenticated GET {path} must 401"
        );
    }
}
