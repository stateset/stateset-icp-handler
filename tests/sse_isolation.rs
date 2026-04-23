//! Tenant isolation for the HTTP SSE event stream.
//!
//! The in-process event bus is global by design, but public streaming
//! endpoints must filter before serializing. This regression test opens
//! a tenant A SSE stream, emits a tenant B event first, then emits a
//! tenant A event and asserts the first delivered frame is tenant A's.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo, build_app_state, build_router, config::Config, AppState,
};
use tokio::time::timeout;
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:sse-isolation";

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

async fn quote(app: &Router, bearer: &str) -> (StatusCode, Value) {
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
                    "sku": "WIDGET-001",
                    "quantity": 1,
                    "unit_price_hint": { "amount_minor": 1500, "currency": "USD" }
                }] }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn next_sse_data_frame(body: &mut Body) -> String {
    loop {
        let frame = timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("SSE stream should yield a tenant-visible frame")
            .expect("SSE body should not end")
            .expect("SSE frame should be ok");
        let Some(bytes) = frame.data_ref() else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        return String::from_utf8(bytes.to_vec()).expect("SSE frame is UTF-8");
    }
}

#[tokio::test]
async fn sse_stream_only_delivers_callers_tenant_events() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    let req = Request::builder()
        .method("GET")
        .uri("/icp/v1/events:stream")
        .header("Authorization", "Bearer k_a")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();

    let (b_status, b_body) = quote(&app, "k_b").await;
    assert_eq!(b_status, StatusCode::OK, "{b_body}");

    let (a_status, a_body) = quote(&app, "k_a").await;
    assert_eq!(a_status, StatusCode::OK, "{a_body}");

    let frame = next_sse_data_frame(&mut body).await;
    assert!(
        frame.contains("\"tenant_id\":\"tenant_a\""),
        "tenant A stream should receive tenant A event first: {frame}"
    );
    assert!(
        !frame.contains("\"tenant_id\":\"tenant_b\""),
        "tenant A stream must not receive tenant B event: {frame}"
    );
}
