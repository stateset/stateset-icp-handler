use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use stateset_icp_handler::{agent::ApiKeyInfo, build_app_state, build_router, config::Config};
use tower::ServiceExt;

async fn app_with_keys(keys: Vec<ApiKeyInfo>) -> axum::Router {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    cfg.api_keys_json = Some(serde_json::to_string(&keys).unwrap());
    let state = build_app_state(&cfg).await.expect("state");
    build_router(state)
}

fn key(name: &str, allowed_agents: Option<Vec<String>>, expires_delta: Duration) -> ApiKeyInfo {
    ApiKeyInfo {
        key: format!("k_{name}"),
        tenant_id: format!("tenant_{name}"),
        name: name.to_string(),
        rate_limit_per_minute: None,
        allowed_agents,
        expires_at: Some(Utc::now() + expires_delta),
    }
}

async fn submit_quote(app: &axum::Router, bearer: &str, agent_id: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/icp/v1/intents")
                .header("Authorization", format!("Bearer {bearer}"))
                .header("ICP-Agent-Id", agent_id)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "intent": "intent.quote",
                        "agent_id": agent_id,
                        "params": {
                            "items": [{
                                "sku": "WIDGET",
                                "quantity": 1,
                                "unit_price_hint": { "amount_minor": 100, "currency": "USD" }
                            }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn expired_api_key_is_rejected() {
    let app = app_with_keys(vec![key("expired", None, Duration::seconds(-1))]).await;
    let (status, body) = submit_quote(&app, "k_expired", "did:stateset:agent:any").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "authentication_failed");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("expired"));
}

#[tokio::test]
async fn allowed_agents_are_enforced() {
    let allowed = "did:stateset:agent:allowed";
    let app = app_with_keys(vec![key(
        "scoped",
        Some(vec![allowed.to_string()]),
        Duration::hours(1),
    )])
    .await;

    let (status, _) = submit_quote(&app, "k_scoped", allowed).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = submit_quote(&app, "k_scoped", "did:stateset:agent:blocked").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "authentication_failed");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not allowed"));
}

#[tokio::test]
async fn envelope_agent_must_match_header_agent() {
    let app = app_with_keys(vec![key("match", None, Duration::hours(1))]).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/icp/v1/intents")
                .header("Authorization", "Bearer k_match")
                .header("ICP-Agent-Id", "did:stateset:agent:header")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "intent": "intent.quote",
                        "agent_id": "did:stateset:agent:body",
                        "params": {
                            "items": [{
                                "sku": "WIDGET",
                                "quantity": 1,
                                "unit_price_hint": { "amount_minor": 100, "currency": "USD" }
                            }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
