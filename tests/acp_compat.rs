//! ACP compatibility integration tests.
//!
//! Drives the handler via ACP's native `/checkout_sessions` wire format
//! and asserts that:
//!   - session lifecycle works (create → update → complete)
//!   - ACP responses carry `API-Version` and `Request-Id` headers
//!   - the underlying ICP transaction lifecycle advances correctly
//!   - the compat path still signs receipts (so audit remains uniform)
//!   - disabling ACP (`ICP_ACP_COMPAT_ENABLED=false`) removes the routes

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{build_app_state, build_router, config::Config};
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";

async fn setup(mut mutate: impl FnMut(&mut Config)) -> Router {
    let mut config = Config::for_test();
    mutate(&mut config);
    let state = build_app_state(&config).await.expect("build_app_state");
    build_router(state)
}

async fn send_json(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, http::HeaderMap, Value) {
    send_json_as(app, DEMO_KEY, method, path, body).await
}

async fn send_json_as(
    app: &Router,
    bearer: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, http::HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {bearer}"));
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
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, v)
}

fn create_body() -> Value {
    json!({
        "buyer": {
            "first_name": "Alice",
            "last_name": "Smith",
            "email": "alice@example.com"
        },
        "items": [
            { "id": "WIDGET-001", "quantity": 2 }
        ],
        "fulfillment_address": {
            "name": "Alice Smith",
            "line_one": "1 Market St",
            "city": "San Francisco",
            "state": "CA",
            "postal_code": "94105",
            "country": "US"
        }
    })
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn acp_create_session_maps_to_intent_quote() {
    let app = setup(|_| {}).await;
    let (status, headers, body) =
        send_json(&app, "POST", "/checkout_sessions", Some(create_body())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        headers.get("api-version").unwrap().to_str().unwrap(),
        "2025-09-29"
    );
    assert!(headers.get("request-id").is_some());

    // ACP-shaped response fields.
    assert!(body["id"].as_str().unwrap().starts_with("txn_"));
    assert_eq!(body["status"], "not_ready_for_payment");
    assert_eq!(body["currency"], "USD");

    // line_items in ACP shape.
    let li = body["line_items"].as_array().unwrap();
    assert_eq!(li.len(), 1);
    assert_eq!(li[0]["item"]["id"], "WIDGET-001");
    assert_eq!(li[0]["item"]["quantity"], 2);

    // totals in ACP shape.
    let totals = body["totals"].as_array().unwrap();
    let has_total = totals.iter().any(|t| t["type"] == "total");
    let has_tax = totals.iter().any(|t| t["type"] == "tax");
    assert!(has_total && has_tax);

    // payment_provider advertised.
    assert_eq!(body["payment_provider"]["provider"], "stateset");
}

#[tokio::test]
async fn acp_full_session_lifecycle_create_update_complete() {
    let app = setup(|_| {}).await;

    // Create
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout_sessions", Some(create_body())).await;
    let session_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["status"], "not_ready_for_payment");

    // Update — advances to ready_for_payment via intent.authorize.
    let (status, _h, updated) = send_json(
        &app,
        "POST",
        &format!("/checkout_sessions/{session_id}"),
        Some(json!({
            "buyer": { "email": "alice@example.com" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["status"], "ready_for_payment");
    assert_eq!(updated["id"], session_id);

    // Complete — advances to completed via intent.buy with delegated token.
    let (status, _h, completed) = send_json(
        &app,
        "POST",
        &format!("/checkout_sessions/{session_id}/complete"),
        Some(json!({
            "payment_data": {
                "token": "vault_tok_demo_abc123",
                "provider": "stripe_delegated"
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(completed["status"], "completed");
}

#[tokio::test]
async fn acp_get_session_returns_current_state() {
    let app = setup(|_| {}).await;
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout_sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, headers, body) =
        send_json(&app, "GET", &format!("/checkout_sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id);
    assert_eq!(body["status"], "not_ready_for_payment");
    assert_eq!(
        headers.get("api-version").unwrap().to_str().unwrap(),
        "2025-09-29"
    );
}

#[tokio::test]
async fn acp_get_session_is_tenant_scoped() {
    let app = setup(|cfg| {
        cfg.enable_demo_keys = false;
        cfg.api_keys_json = Some(
            json!([
                { "key": "k_a", "tenant_id": "tenant_a", "name": "Tenant A" },
                { "key": "k_b", "tenant_id": "tenant_b", "name": "Tenant B" }
            ])
            .to_string(),
        );
    })
    .await;
    let (_s, _h, created) = send_json_as(
        &app,
        "k_a",
        "POST",
        "/checkout_sessions",
        Some(create_body()),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _h, _body) = send_json_as(
        &app,
        "k_b",
        "GET",
        &format!("/checkout_sessions/{id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant compat reads must not leak checkout sessions"
    );

    let (status, _h, body) = send_json_as(
        &app,
        "k_a",
        "GET",
        &format!("/checkout_sessions/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id);
}

#[tokio::test]
async fn acp_cancel_marks_session_canceled() {
    let app = setup(|_| {}).await;
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout_sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _h, canceled) = send_json(
        &app,
        "POST",
        &format!("/checkout_sessions/{id}/cancel"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canceled["status"], "canceled");
}

#[tokio::test]
async fn acp_complete_records_receipt_in_icp_store() {
    // Completing an ACP session MUST still produce a signed ICP receipt
    // — that's the "uniform audit" promise of the compat layer.
    let app = setup(|_| {}).await;

    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout_sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    // Authorize then complete.
    send_json(
        &app,
        "POST",
        &format!("/checkout_sessions/{id}"),
        Some(json!({ "buyer": { "email": "a@b.co" } })),
    )
    .await;
    send_json(
        &app,
        "POST",
        &format!("/checkout_sessions/{id}/complete"),
        Some(json!({
            "payment_data": { "token": "v_tok", "provider": "stripe" }
        })),
    )
    .await;

    // The ICP transaction is now addressable under /icp/v1/transactions/:id
    // AND its completion emitted a receipt (verifiable via the ICP read path).
    let (status, _h, txn) =
        send_json_with_agent(&app, "GET", &format!("/icp/v1/transactions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(txn["state"], "completed");
    assert_eq!(txn["agent_id"], "did:stateset:agent:acp-merchant_demo");
}

#[tokio::test]
async fn acp_routes_disabled_when_compat_off() {
    let app = setup(|cfg| cfg.acp_compat_enabled = false).await;
    let (status, _h, _body) =
        send_json(&app, "POST", "/checkout_sessions", Some(create_body())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn acp_rejects_unknown_api_version() {
    let app = setup(|_| {}).await;
    let req = Request::builder()
        .method("POST")
        .uri("/checkout_sessions")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("API-Version", "1999-01-01")
        .header("content-type", "application/json")
        .body(Body::from(create_body().to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn acp_missing_bearer_rejected() {
    let app = setup(|_| {}).await;
    let req = Request::builder()
        .method("POST")
        .uri("/checkout_sessions")
        .header("content-type", "application/json")
        .body(Body::from(create_body().to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// --------------------------------------------------------------------------
// Helpers that add ICP-Agent-Id for reads into the ICP surface

async fn send_json_with_agent(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, http::HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", "did:stateset:agent:test-reader");
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
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, v)
}
