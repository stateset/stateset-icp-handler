//! OpenAPI 3.1 schema tests.
//!
//! These lock in three properties of the machine-readable contract:
//!
//! 1. `GET /openapi.json` returns a valid OpenAPI 3.x document.
//! 2. Every first-class ICP route and every core model type appears in
//!    the document. If a future refactor accidentally drops the utoipa
//!    annotation from a handler or `ToSchema` from a model, a test
//!    fails rather than the schema silently shrinking.
//! 3. `GET /docs` returns the Swagger UI shell.
//!
//! The goal is that a Python or Go developer can fetch `/openapi.json`
//! from a running handler, feed it to `openapi-generator`, and get a
//! working client — without ever reading `src/models.rs`. That's the
//! substrate test for "someone else can implement ICP from the spec."

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use stateset_icp_handler::{build_app_state, build_router, config::Config};
use tower::ServiceExt;

async fn router() -> axum::Router {
    let state = build_app_state(&Config::for_test())
        .await
        .expect("build_app_state");
    build_router(state)
}

async fn get_json(path: &str) -> (StatusCode, Value) {
    let app = router().await;
    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "expected JSON at {path}, got: {}\nerror: {e}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, json)
}

#[tokio::test]
async fn openapi_json_is_valid_openapi_3() {
    let (status, doc) = get_json("/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    let version = doc["openapi"].as_str().expect("openapi field is a string");
    assert!(
        version.starts_with("3."),
        "expected OpenAPI 3.x, got `{version}`"
    );
    assert_eq!(doc["info"]["title"], "StateSet ICP Handler");
    assert!(doc["info"]["version"].as_str().is_some());
}

#[tokio::test]
async fn openapi_json_includes_every_icp_core_path() {
    let (_, doc) = get_json("/openapi.json").await;
    let paths = doc["paths"].as_object().expect("paths object");

    let expected = [
        "/health",
        "/ready",
        "/metrics",
        "/.well-known/icp",
        "/.well-known/icp/jwks.json",
        "/icp/v1/intents",
        "/icp/v1/transactions/{id}",
        "/icp/v1/subscriptions/{id}",
        "/icp/v1/peer_quotes/{id}",
        "/icp/v1/receipts/{jti}",
        "/icp/v1/mandates/{jti}/usage",
        "/icp/v1/events:stream",
        "/icp/v1/webhook_deliveries",
        "/icp/v1/webhook_deliveries/{id}",
    ];
    for p in expected {
        assert!(
            paths.contains_key(p),
            "OpenAPI doc missing expected path `{p}`. Present paths: {:?}",
            paths.keys().collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn openapi_json_includes_core_commerce_schemas() {
    let (_, doc) = get_json("/openapi.json").await;
    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("components.schemas object");

    // Representative slice — if these 11 types are present, the ToSchema
    // annotations survived and SDK generators will have what they need to
    // model the buy/subscribe/peer-quote flows.
    let expected = [
        "IntentEnvelope",
        "IntentResponseBody",
        "Transaction",
        "TransactionState",
        "Subscription",
        "SubscriptionStatus",
        "PeerQuote",
        "Money",
        "LineItem",
        "ReceiptStub",
        "PaymentInstrument",
    ];
    for name in expected {
        assert!(
            schemas.contains_key(name),
            "OpenAPI doc missing expected schema `{name}`. Present: {:?}",
            schemas.keys().collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn submit_intent_path_declares_request_body_and_error_responses() {
    let (_, doc) = get_json("/openapi.json").await;
    let op = &doc["paths"]["/icp/v1/intents"]["post"];
    assert!(
        op["requestBody"].is_object(),
        "POST /icp/v1/intents should declare a requestBody"
    );
    let responses = op["responses"].as_object().expect("responses object");
    for status in ["200", "400", "401", "402", "403", "409"] {
        assert!(
            responses.contains_key(status),
            "POST /icp/v1/intents missing {status} response. Got: {:?}",
            responses.keys().collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn docs_endpoint_returns_swagger_ui_shell() {
    let app = router().await;
    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/docs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.starts_with("text/html"), "content-type = {ct}");
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("swagger-ui"));
    assert!(html.contains("/openapi.json"));
}
