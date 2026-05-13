//! Pre-auth (per-IP) rate-limit integration tests.
//!
//! Asserts the limiter that fires *before* bearer resolution:
//!   * Burst from one IP is capped, returns 429 + Retry-After + the
//!     `X-RateLimit-Scope: pre-auth` header so callers can distinguish
//!     it from the per-tenant limit.
//!   * Distinct IPs (`X-Forwarded-For` values) don't share buckets.
//!   * Triggered BEFORE auth — even a request with a missing/invalid
//!     bearer counts against the IP bucket. (Without this property,
//!     the limit doesn't actually defend against fake-key floods.)
//!   * `X-Real-IP` is honored as a fallback when `X-Forwarded-For` is
//!     absent.
//!   * GET endpoints (health, discovery) are NOT rate-limited at the
//!     pre-auth layer — monitoring + dashboards stay hammerable.
//!   * Capacity 0 disables the limit (handler config knob respected).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::json;
use stateset_icp_handler::{build_app_state, build_router, config::Config};
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";
const AGENT: &str = "did:stateset:agent:preauth-test";

async fn build(pre_auth_per_minute: u32) -> Router {
    let mut cfg = Config::for_test();
    cfg.pre_auth_rate_limit_per_minute = pre_auth_per_minute;
    // Pin a generous post-auth budget so we don't accidentally trip
    // the wrong limiter.
    cfg.rate_limit_per_minute = 1000;
    let state = build_app_state(&cfg).await.expect("state");
    build_router(state)
}

async fn build_with_config(mut cfg: Config, pre_auth_per_minute: u32) -> Router {
    cfg.pre_auth_rate_limit_per_minute = pre_auth_per_minute;
    cfg.rate_limit_per_minute = 1000;
    let state = build_app_state(&cfg).await.expect("state");
    build_router(state)
}

async fn post_intent(
    app: &Router,
    bearer: Option<&str>,
    forwarded_for: Option<&str>,
) -> (StatusCode, http::HeaderMap) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("ICP-Agent-Id", AGENT)
        .header("content-type", "application/json");
    if let Some(b) = bearer {
        builder = builder.header("Authorization", format!("Bearer {b}"));
    }
    if let Some(ip) = forwarded_for {
        builder = builder.header("X-Forwarded-For", ip);
    }
    let req = builder
        .body(Body::from(
            json!({
                "intent": "intent.quote",
                "agent_id": AGENT,
                "params": {
                    "items": [{
                        "sku": "WIDGET-001",
                        "quantity": 1,
                        "unit_price_hint": { "amount_minor": 1, "currency": "USD" }
                    }]
                }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, headers)
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn burst_from_one_ip_is_capped() {
    let app = build(3).await;

    // First 3 succeed (with valid bearer + agent).
    for i in 1..=3 {
        let (s, _) = post_intent(&app, Some(DEMO_KEY), Some("203.0.113.42")).await;
        assert_eq!(s, StatusCode::OK, "call {i}");
    }

    // 4th from the same IP — denied at the pre-auth layer.
    let (s, h) = post_intent(&app, Some(DEMO_KEY), Some("203.0.113.42")).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        h.get("x-ratelimit-scope")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "pre-auth",
        "scope header should distinguish pre-auth from per-tenant denial"
    );
    let retry_after = h
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(-1);
    assert!(
        (1..=60).contains(&retry_after),
        "Retry-After must be in [1, 60], got {retry_after}"
    );
}

#[tokio::test]
async fn distinct_ips_do_not_share_buckets() {
    let app = build(1).await;

    // IP A burns its quota.
    let (s_a1, _) = post_intent(&app, Some(DEMO_KEY), Some("198.51.100.1")).await;
    assert_eq!(s_a1, StatusCode::OK);
    let (s_a2, _) = post_intent(&app, Some(DEMO_KEY), Some("198.51.100.1")).await;
    assert_eq!(s_a2, StatusCode::TOO_MANY_REQUESTS);

    // IP B is unaffected.
    let (s_b1, _) = post_intent(&app, Some(DEMO_KEY), Some("198.51.100.2")).await;
    assert_eq!(s_b1, StatusCode::OK);
}

#[tokio::test]
async fn fake_bearer_floods_are_capped_at_pre_auth_layer() {
    // The whole point of pre-auth limiting: requests with bogus
    // bearers should be rejected at IP-level, not at the keystore
    // lookup. Verifies the limiter fires BEFORE auth resolution by
    // confirming the response is 429 (rate_limited) and not the 401
    // (authentication_failed) we'd otherwise see.
    let app = build(2).await;

    let (s1, _) = post_intent(&app, Some("totally_fake_key"), Some("192.0.2.99")).await;
    let (s2, _) = post_intent(&app, Some("totally_fake_key"), Some("192.0.2.99")).await;
    // First two get past the pre-auth limit and are rejected by auth.
    assert_eq!(s1, StatusCode::UNAUTHORIZED);
    assert_eq!(s2, StatusCode::UNAUTHORIZED);

    // Third trips pre-auth — the 429 means we never even tried to
    // look up the key.
    let (s3, h) = post_intent(&app, Some("totally_fake_key"), Some("192.0.2.99")).await;
    assert_eq!(
        s3,
        StatusCode::TOO_MANY_REQUESTS,
        "fake-bearer flood must hit pre-auth ceiling, not infinite 401s"
    );
    assert_eq!(
        h.get("x-ratelimit-scope").unwrap().to_str().unwrap(),
        "pre-auth"
    );
}

#[tokio::test]
async fn x_real_ip_used_when_x_forwarded_for_absent() {
    let app = build(1).await;

    let (s1, _) = {
        let req = Request::builder()
            .method("POST")
            .uri("/icp/v1/intents")
            .header("Authorization", format!("Bearer {DEMO_KEY}"))
            .header("ICP-Agent-Id", AGENT)
            .header("X-Real-IP", "10.0.0.55")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "intent": "intent.quote",
                    "agent_id": AGENT,
                    "params": { "items": [{
                        "sku": "WIDGET-001", "quantity": 1,
                        "unit_price_hint": { "amount_minor": 1, "currency": "USD" }
                    }] }
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let h = resp.headers().clone();
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, h)
    };
    assert_eq!(s1, StatusCode::OK);

    // Same X-Real-IP, second call → denied (capacity is 1).
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", AGENT)
        .header("X-Real-IP", "10.0.0.55")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "intent": "intent.quote",
                "agent_id": AGENT,
                "params": { "items": [{
                    "sku": "WIDGET-001", "quantity": 1,
                    "unit_price_hint": { "amount_minor": 1, "currency": "USD" }
                }] }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn xff_first_segment_is_used() {
    // RFC 7239: client, then proxies. When chained
    // `X-Forwarded-For: 203.0.113.5, 10.0.0.1, 10.0.0.2`, the first
    // value is the originating client. Limiter must key on that, not
    // on the entire string (which would put every request in its own
    // bucket and silently disable the limit).
    let app = build(1).await;
    let (s1, _) = post_intent(
        &app,
        Some(DEMO_KEY),
        Some("203.0.113.5, 10.0.0.1, 10.0.0.2"),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    // Second request, same originating client but a different proxy
    // chain — must hit the same bucket because the FIRST IP is what
    // identifies the caller.
    let (s2, _) = post_intent(&app, Some(DEMO_KEY), Some("203.0.113.5, 10.0.0.99")).await;
    assert_eq!(
        s2,
        StatusCode::TOO_MANY_REQUESTS,
        "limiter must key on the first XFF segment, not the full chain"
    );
}

#[tokio::test]
async fn get_endpoints_are_not_rate_limited_pre_auth() {
    // Pre-auth limiting applies to write paths only. Health checks +
    // discovery + jwks must remain hammerable from monitoring.
    let app = build(1).await;

    for _ in 0..20 {
        let req = Request::builder()
            .method("GET")
            .uri("/health")
            .header("X-Forwarded-For", "203.0.113.99")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "health must always succeed");
    }
    for _ in 0..20 {
        let req = Request::builder()
            .method("GET")
            .uri("/.well-known/icp")
            .header("X-Forwarded-For", "203.0.113.99")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "discovery must always succeed"
        );
    }
}

#[tokio::test]
async fn capacity_zero_disables_pre_auth_limit() {
    let app = build(0).await;
    // 100 burst from the same IP — all succeed.
    for i in 1..=100 {
        let (s, h) = post_intent(&app, Some(DEMO_KEY), Some("203.0.113.7")).await;
        assert_eq!(s, StatusCode::OK, "call {i}");
        // No pre-auth scope header on success path.
        assert!(h.get("x-ratelimit-scope").is_none());
    }
}

#[tokio::test]
async fn missing_xff_lumps_into_direct_bucket() {
    // Without any forwarding headers the handler can't distinguish
    // callers, so it shares a single bucket. Asserts that — without
    // it, a deployment that forgot to terminate behind a proxy would
    // silently disable the protection.
    let app = build(2).await;
    let (s1, _) = post_intent(&app, Some(DEMO_KEY), None).await;
    let (s2, _) = post_intent(&app, Some(DEMO_KEY), None).await;
    let (s3, h) = post_intent(&app, Some(DEMO_KEY), None).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        s3,
        StatusCode::TOO_MANY_REQUESTS,
        "no-XFF callers all share one bucket"
    );
    assert_eq!(
        h.get("x-ratelimit-scope").unwrap().to_str().unwrap(),
        "pre-auth"
    );
}

#[tokio::test]
async fn forwarded_headers_are_ignored_unless_explicitly_trusted() {
    let mut cfg = Config::for_test();
    cfg.trust_proxy_headers = false;
    let app = build_with_config(cfg, 1).await;

    let (s1, _) = post_intent(&app, Some(DEMO_KEY), Some("198.51.100.10")).await;
    assert_eq!(s1, StatusCode::OK);

    // Different spoofed XFF, but proxy trust is off, so both requests
    // share the direct bucket and the second request is denied.
    let (s2, h) = post_intent(&app, Some(DEMO_KEY), Some("198.51.100.11")).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        h.get("x-ratelimit-scope").unwrap().to_str().unwrap(),
        "pre-auth"
    );
}
