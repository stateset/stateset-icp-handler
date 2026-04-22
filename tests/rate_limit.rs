//! Per-tenant rate-limit integration tests.
//!
//! Drives the real `submit_intent` path through the in-process router.
//! Asserts:
//!   * Within capacity, requests are accepted and `X-RateLimit-*`
//!     headers are stamped.
//!   * The N+1th request returns HTTP 429 + `Retry-After`.
//!   * Per-tenant overrides (`ApiKeyInfo.rate_limit_per_minute`) take
//!     precedence over the handler default.
//!   * `rate_limit_per_minute = 0` on a tenant disables the limit
//!     (trusted internal client).
//!   * Distinct tenants don't share buckets.
//!   * A pre-emptive bucket reset (clearing the limiter) re-allows the
//!     same tenant — used to simulate the next time window without
//!     sleeping for 60 seconds.
//!   * Rate-limit headers also appear on idempotency replays so a
//!     replayed response reflects the *current* window.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::{ApiKeyInfo, ApiKeyStore},
    build_app_state, build_router,
    config::Config,
    AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:rl-test";

async fn build(default_per_minute: u32, extra_keys: Vec<ApiKeyInfo>) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.rate_limit_per_minute = default_per_minute;
    // Tests bring their own API keys so they can pin
    // `rate_limit_per_minute` per tenant.
    cfg.enable_demo_keys = false;
    if !extra_keys.is_empty() {
        cfg.api_keys_json = Some(serde_json::to_string(&extra_keys).unwrap());
    }
    let state = build_app_state(&cfg).await.expect("state");
    // Override the api key store directly when extra_keys is present —
    // simpler than wrestling with config.
    let state = if extra_keys.is_empty() {
        AppState {
            keys: ApiKeyStore::demo(),
            ..state
        }
    } else {
        AppState {
            keys: ApiKeyStore::new(extra_keys),
            ..state
        }
    };
    let router = build_router(state.clone());
    (state, router)
}

async fn quote(app: &Router, api_key: &str) -> (StatusCode, http::HeaderMap, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("ICP-Agent-Id", AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.quote",
                "agent_id": AGENT,
                "params": {
                    "items": [{
                        "sku": "WIDGET-001",
                        "quantity": 1,
                        "unit_price_hint": { "amount_minor": 100, "currency": "USD" }
                    }]
                }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

fn header_int(h: &http::HeaderMap, name: &str) -> i64 {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1)
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn within_capacity_succeeds_and_stamps_ratelimit_headers() {
    let key = "icp_rl_key_a";
    let (_state, app) = build(
        3,
        vec![ApiKeyInfo {
            key: key.into(),
            tenant_id: "t_a".into(),
            name: "test".into(),
            rate_limit_per_minute: None,
            allowed_agents: None,
            expires_at: None,
        }],
    )
    .await;

    for i in 1..=3 {
        let (status, headers, _) = quote(&app, key).await;
        assert_eq!(status, StatusCode::OK, "call {i}");
        assert_eq!(header_int(&headers, "x-ratelimit-limit"), 3);
        assert_eq!(
            header_int(&headers, "x-ratelimit-remaining"),
            (3 - i) as i64,
            "remaining after call {i}"
        );
        let reset = header_int(&headers, "x-ratelimit-reset");
        assert!(
            reset > 0 && reset <= 60,
            "reset header should be in [1, 60], got {reset}"
        );
    }
}

#[tokio::test]
async fn n_plus_one_returns_429_with_retry_after() {
    let key = "icp_rl_key_b";
    let (_, app) = build(
        2,
        vec![ApiKeyInfo {
            key: key.into(),
            tenant_id: "t_b".into(),
            name: "test".into(),
            rate_limit_per_minute: None,
            allowed_agents: None,
            expires_at: None,
        }],
    )
    .await;

    let (s1, _, _) = quote(&app, key).await;
    let (s2, _, _) = quote(&app, key).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);

    let (s3, headers, body) = quote(&app, key).await;
    assert_eq!(s3, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["type"], "rate_limited");
    assert_eq!(body["error"]["retriable"], true);
    let retry_after = header_int(&headers, "retry-after");
    assert!(
        retry_after > 0 && retry_after <= 60,
        "retry-after must be in [1, 60], got {retry_after}"
    );
    assert_eq!(header_int(&headers, "x-ratelimit-limit"), 2);
    assert_eq!(header_int(&headers, "x-ratelimit-remaining"), 0);
}

#[tokio::test]
async fn per_tenant_override_takes_precedence_over_default() {
    let key = "icp_rl_key_c";
    // Handler default is 1, but this tenant gets 5 — verify the higher
    // limit applies.
    let (_, app) = build(
        1,
        vec![ApiKeyInfo {
            key: key.into(),
            tenant_id: "t_c".into(),
            name: "premium".into(),
            rate_limit_per_minute: Some(5),
            allowed_agents: None,
            expires_at: None,
        }],
    )
    .await;

    for i in 1..=5 {
        let (status, headers, _) = quote(&app, key).await;
        assert_eq!(status, StatusCode::OK, "call {i}");
        assert_eq!(header_int(&headers, "x-ratelimit-limit"), 5);
    }
    let (s6, _, _) = quote(&app, key).await;
    assert_eq!(s6, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn per_tenant_zero_means_unlimited() {
    let key = "icp_rl_key_d";
    // Handler default is 1 (very tight) but this tenant has 0 → no limit.
    let (_, app) = build(
        1,
        vec![ApiKeyInfo {
            key: key.into(),
            tenant_id: "t_d".into(),
            name: "internal".into(),
            rate_limit_per_minute: Some(0),
            allowed_agents: None,
            expires_at: None,
        }],
    )
    .await;

    for _ in 0..50 {
        let (status, _, _) = quote(&app, key).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "unlimited tenant must always succeed"
        );
    }
}

#[tokio::test]
async fn distinct_tenants_have_distinct_buckets() {
    let key_a = "icp_rl_key_e1";
    let key_b = "icp_rl_key_e2";
    let (_, app) = build(
        1, // default; both tenants share
        vec![
            ApiKeyInfo {
                key: key_a.into(),
                tenant_id: "t_e1".into(),
                name: "a".into(),
                rate_limit_per_minute: None,
                allowed_agents: None,
                expires_at: None,
            },
            ApiKeyInfo {
                key: key_b.into(),
                tenant_id: "t_e2".into(),
                name: "b".into(),
                rate_limit_per_minute: None,
                allowed_agents: None,
                expires_at: None,
            },
        ],
    )
    .await;

    // Tenant A burns its quota.
    let (sa1, _, _) = quote(&app, key_a).await;
    assert_eq!(sa1, StatusCode::OK);
    let (sa2, _, _) = quote(&app, key_a).await;
    assert_eq!(sa2, StatusCode::TOO_MANY_REQUESTS);

    // Tenant B is unaffected.
    let (sb1, _, _) = quote(&app, key_b).await;
    assert_eq!(sb1, StatusCode::OK);
}

#[tokio::test]
async fn next_window_clears_the_counter() {
    let key = "icp_rl_key_f";
    let (state, app) = build(
        1,
        vec![ApiKeyInfo {
            key: key.into(),
            tenant_id: "t_f".into(),
            name: "test".into(),
            rate_limit_per_minute: None,
            allowed_agents: None,
            expires_at: None,
        }],
    )
    .await;

    let (s1, _, _) = quote(&app, key).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _, _) = quote(&app, key).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);

    // Simulate window rollover by clearing the limiter — the
    // alternative is to actually wait 60s, which makes tests slow.
    state.service.rate_limiter.clear();

    let (s3, _, _) = quote(&app, key).await;
    assert_eq!(
        s3,
        StatusCode::OK,
        "next window must allow the same tenant again"
    );
}

#[tokio::test]
async fn ratelimit_headers_appear_on_idempotency_replay() {
    let key = "icp_rl_key_g";
    let (_, app) = build(
        10,
        vec![ApiKeyInfo {
            key: key.into(),
            tenant_id: "t_g".into(),
            name: "test".into(),
            rate_limit_per_minute: None,
            allowed_agents: None,
            expires_at: None,
        }],
    )
    .await;

    // First call — counts against rate limit, populates idempotency.
    let req = |idem: &str| {
        Request::builder()
            .method("POST")
            .uri("/icp/v1/intents")
            .header("Authorization", format!("Bearer {key}"))
            .header("ICP-Agent-Id", AGENT)
            .header("ICP-Idempotency-Key", idem)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "intent": "intent.quote",
                    "agent_id": AGENT,
                    "params": {
                        "items": [{
                            "sku": "WIDGET-001",
                            "quantity": 1,
                            "unit_price_hint": { "amount_minor": 999, "currency": "USD" }
                        }]
                    }
                })
                .to_string(),
            ))
            .unwrap()
    };
    let r1 = app.clone().oneshot(req("idem-rl")).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let h1 = r1.headers().clone();
    assert_eq!(header_int(&h1, "x-ratelimit-remaining"), 9);

    // Replay — same idempotency key, same body. Body is replayed
    // verbatim BUT rate-limit headers reflect the new window state.
    let r2 = app.clone().oneshot(req("idem-rl")).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let h2 = r2.headers().clone();
    assert_eq!(
        h2.get("idempotent-replayed").unwrap().to_str().unwrap(),
        "true"
    );
    // Replay still counts against the rate limit (it's still an HTTP
    // call we did work for) — so remaining should have decreased by 1.
    assert_eq!(
        header_int(&h2, "x-ratelimit-remaining"),
        8,
        "rate limit must count idempotency replays as real requests"
    );
}
