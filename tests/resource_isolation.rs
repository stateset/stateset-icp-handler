//! Tenant-scoping for the per-resource read endpoints:
//!   * `GET /icp/v1/transactions/:id`
//!   * `GET /icp/v1/subscriptions/:id`
//!   * `GET /icp/v1/peer_quotes/:id`
//!
//! Asserts that:
//!   * Each resource carries its originating `tenant_id` (stamped at
//!     creation time from the bearer key — no caller path).
//!   * Cross-tenant reads return **404**, not 403 — existence is not
//!     leaked across tenant boundaries (so tenant A cannot enumerate
//!     B's id space or confirm a guess).
//!   * Same-tenant reads continue to succeed normally.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo, build_app_state, build_router, config::Config, AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:resource-iso";

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

async fn quote_returns_txn_id(app: &Router, bearer: &str) -> String {
    let (status, body) = send(
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
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "intent.quote should succeed: {body}"
    );
    body["transaction"]["id"].as_str().unwrap().to_string()
}

async fn subscribe_returns_sub_id(app: &Router, bearer: &str) -> String {
    let (status, body) = send(
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
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "intent.subscribe should succeed: {body}"
    );
    body["subscription"]["id"].as_str().unwrap().to_string()
}

async fn a2a_quote_returns_pq_id(app: &Router, bearer: &str) -> String {
    let (status, body) = send(
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
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "intent.a2a_quote should succeed: {body}"
    );
    body["peer_quote"]["id"].as_str().unwrap().to_string()
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn transactions_get_is_tenant_scoped() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let txn_a = quote_returns_txn_id(&app, "k_a").await;

    // Same-tenant read works and exposes the stamped tenant_id.
    let (status, body) = send(
        &app,
        "GET",
        &format!("/icp/v1/transactions/{txn_a}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], txn_a);
    assert_eq!(
        body["tenant_id"], "tenant_a",
        "transaction must be stamped with the originating tenant"
    );

    // Cross-tenant read 404s (not 403). 403 would confirm B that the
    // id exists for some other tenant, letting B enumerate A's ids.
    let (status, _) = send(
        &app,
        "GET",
        &format!("/icp/v1/transactions/{txn_a}"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant transaction read must look identical to a missing row"
    );

    // Genuine miss also 404s (sanity — the path doesn't accidentally
    // turn into a 500 for unknown ids).
    let (status, _) = send(
        &app,
        "GET",
        "/icp/v1/transactions/txn_does_not_exist",
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subscriptions_get_is_tenant_scoped() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let sub_a = subscribe_returns_sub_id(&app, "k_a").await;

    let (status, body) = send(
        &app,
        "GET",
        &format!("/icp/v1/subscriptions/{sub_a}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], sub_a);
    assert_eq!(body["tenant_id"], "tenant_a");

    let (status, _) = send(
        &app,
        "GET",
        &format!("/icp/v1/subscriptions/{sub_a}"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant subscription read must 404"
    );
}

#[tokio::test]
async fn peer_quotes_get_is_tenant_scoped() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let pq_a = a2a_quote_returns_pq_id(&app, "k_a").await;

    let (status, body) = send(
        &app,
        "GET",
        &format!("/icp/v1/peer_quotes/{pq_a}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], pq_a);
    assert_eq!(body["tenant_id"], "tenant_a");

    let (status, _) = send(
        &app,
        "GET",
        &format!("/icp/v1/peer_quotes/{pq_a}"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant peer_quote read must 404"
    );
}

#[tokio::test]
async fn unauthenticated_resource_reads_are_rejected() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let txn_a = quote_returns_txn_id(&app, "k_a").await;

    // No Authorization header → 401 (auth fires before tenant scope).
    for path in [
        format!("/icp/v1/transactions/{txn_a}"),
        "/icp/v1/subscriptions/sub_unknown".to_string(),
        "/icp/v1/peer_quotes/pq_unknown".to_string(),
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(&path)
            .header("ICP-Agent-Id", AGENT)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "unauthenticated read of {path} must 401"
        );
    }
}
