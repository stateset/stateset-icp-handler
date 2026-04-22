//! UCP compatibility integration tests.
//!
//! Drives the handler via UCP's native `/checkout-sessions` wire format
//! and asserts that:
//!   - session lifecycle works (create → update → complete)
//!   - UCP responses carry the `ucp.version` + `capabilities` meta block
//!     and native `UCP-Version` + `Request-Id` headers
//!   - UCP uses PUT for updates (distinguishing it from ACP)
//!   - `/.well-known/ucp` discovery advertises the shopping capability
//!   - disabling UCP (`ICP_UCP_COMPAT_ENABLED=false`) removes the routes
//!   - ACP and UCP can coexist without interfering

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
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {DEMO_KEY}"));
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
        "line_items": [
            { "item": { "id": "WIDGET-001" }, "quantity": 2 }
        ],
        "buyer": {
            "first_name": "Alice",
            "last_name": "Smith",
            "email": "alice@example.com"
        },
        "currency": "USD",
        "payment": {},
        "fulfillment": {
            "address": {
                "name": "Alice Smith",
                "line_one": "1 Market St",
                "city": "San Francisco",
                "state": "CA",
                "postal_code": "94105",
                "country": "US"
            }
        }
    })
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn ucp_well_known_discovery_advertises_shopping_capability() {
    let app = setup(|_| {}).await;
    let (status, _h, body) = send_json(&app, "GET", "/.well-known/ucp", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ucp"]["version"], "2026-01-11");
    let capabilities = body["ucp"]["capabilities"].as_array().unwrap();
    assert!(capabilities.iter().any(|c| c["name"] == "dev.ucp.shopping"));
    assert!(
        body["ucp"]["services"]["dev.ucp.shopping"]["rest"]["endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/checkout-sessions")
    );
}

#[tokio::test]
async fn ucp_create_checkout_maps_to_intent_quote() {
    let app = setup(|_| {}).await;
    let (status, headers, body) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        headers.get("ucp-version").unwrap().to_str().unwrap(),
        "2026-01-11"
    );
    assert!(headers.get("request-id").is_some());

    // UCP response envelope shape
    assert_eq!(body["ucp"]["version"], "2026-01-11");
    let caps = body["ucp"]["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c["name"] == "dev.ucp.shopping"));

    // Session identity + state
    assert!(body["id"].as_str().unwrap().starts_with("txn_"));
    assert_eq!(body["status"], "incomplete");
    assert_eq!(body["currency"], "USD");

    // Line item shape (UCP-native: item.id + item.title + item.price)
    let li = body["line_items"].as_array().unwrap();
    assert_eq!(li.len(), 1);
    assert_eq!(li[0]["item"]["id"], "WIDGET-001");
    assert_eq!(li[0]["quantity"], 2);
    let li_totals = li[0]["totals"].as_array().unwrap();
    assert!(li_totals.iter().any(|t| t["type"] == "subtotal"));
    assert!(li_totals.iter().any(|t| t["type"] == "total"));

    // Top-level totals array
    let totals = body["totals"].as_array().unwrap();
    assert!(totals.iter().any(|t| t["type"] == "subtotal"));
    assert!(totals.iter().any(|t| t["type"] == "total"));
}

#[tokio::test]
async fn ucp_full_lifecycle_create_update_complete() {
    let app = setup(|_| {}).await;

    // Create
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["status"], "incomplete");

    // Update — UCP uses PUT (not POST like ACP).
    let (status, _h, updated) = send_json(
        &app,
        "PUT",
        &format!("/checkout-sessions/{id}"),
        Some(json!({ "buyer": { "email": "alice@example.com" } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["status"], "ready_for_complete");

    // Complete with delegated-vault token
    let (status, _h, completed) = send_json(
        &app,
        "POST",
        &format!("/checkout-sessions/{id}/complete"),
        Some(json!({
            "payment_data": {
                "type": "delegated_vault",
                "token": "vault_tok_abc",
                "handler_id": "stripe_delegated"
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(completed["status"], "completed");
}

#[tokio::test]
async fn ucp_update_rejects_post_returns_405() {
    // UCP updates are PUT-only; a POST should not match any route.
    let app = setup(|_| {}).await;
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let req = Request::builder()
        .method("POST")
        .uri(format!("/checkout-sessions/{id}"))
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // axum returns 405 METHOD_NOT_ALLOWED when the path matches a route
    // that doesn't accept the verb.
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn ucp_cancel_marks_session_canceled() {
    let app = setup(|_| {}).await;
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _h, canceled) = send_json(
        &app,
        "POST",
        &format!("/checkout-sessions/{id}/cancel"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canceled["status"], "canceled");
}

#[tokio::test]
async fn ucp_get_checkout_returns_current_state() {
    let app = setup(|_| {}).await;
    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _h, fetched) =
        send_json(&app, "GET", &format!("/checkout-sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], id);
    assert_eq!(fetched["status"], "incomplete");
}

#[tokio::test]
async fn ucp_routes_disabled_when_compat_off() {
    let app = setup(|cfg| cfg.ucp_compat_enabled = false).await;
    let (status, _h, _body) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _h, _body) = send_json(&app, "GET", "/.well-known/ucp", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ucp_rejects_unknown_version_header() {
    let app = setup(|_| {}).await;
    let req = Request::builder()
        .method("POST")
        .uri("/checkout-sessions")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("UCP-Version", "1999-01-01")
        .header("content-type", "application/json")
        .body(Body::from(create_body().to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ucp_complete_persists_to_icp_transaction_store() {
    // A UCP completion MUST still result in an ICP-addressable, signed
    // transaction — same "uniform audit" guarantee we test for ACP.
    let app = setup(|_| {}).await;

    let (_s, _h, created) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    send_json(
        &app,
        "PUT",
        &format!("/checkout-sessions/{id}"),
        Some(json!({ "buyer": { "email": "a@b.co" } })),
    )
    .await;
    send_json(
        &app,
        "POST",
        &format!("/checkout-sessions/{id}/complete"),
        Some(json!({
            "payment_data": { "type": "delegated_vault", "token": "v_tok" }
        })),
    )
    .await;

    // Read through the ICP read path.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/icp/v1/transactions/{id}"))
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", "did:stateset:agent:reader")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let txn: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(txn["state"], "completed");
    assert_eq!(txn["agent_id"], "did:stateset:agent:ucp-merchant_demo");
}

#[tokio::test]
async fn acp_and_ucp_coexist_without_interference() {
    // Both compat surfaces enabled; sessions created via each are
    // distinct and each reports its own status vocabulary.
    let app = setup(|_| {}).await;

    // ACP session
    let (status, _h, acp_created) = send_json(
        &app,
        "POST",
        "/checkout_sessions",
        Some(json!({
            "items": [{ "id": "WIDGET-001", "quantity": 1 }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(acp_created["status"], "not_ready_for_payment"); // ACP vocab

    // UCP session
    let (status, _h, ucp_created) =
        send_json(&app, "POST", "/checkout-sessions", Some(create_body())).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ucp_created["status"], "incomplete"); // UCP vocab

    // IDs differ.
    assert_ne!(acp_created["id"], ucp_created["id"]);
}
