//! Tenant scoping for `GET /icp/v1/mandates/:jti/usage`.
//!
//! Mandate usage rows now record the *first* tenant to spend against
//! the jti — that tenant owns the readable view of the tally.
//! Subsequent spend recorded against the same jti from any tenant
//! still consumes the shared budget (protecting the principal who
//! issued the mandate) but does not change the owner and remains
//! invisible via this endpoint to non-owning tenants. Asserts:
//!   * Same-tenant read of an owned mandate returns the tally.
//!   * Cross-tenant reads return **404** (existence not leaked).
//!   * Reads of jtis with no recorded spend return **404** (the
//!     handler doesn't fabricate a "you own this empty bucket"
//!     response — that would let any tenant probe arbitrary ids).
//!   * Direct ledger inserts mirror the API behavior.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde_json::Value;
use stateset_icp_handler::{
    agent::ApiKeyInfo,
    build_app_state, build_router,
    config::Config,
    mandate::{MandateLedger, MandateUsage},
    state_db, AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:mu-iso";

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

async fn get(app: &Router, path: &str, bearer: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("Authorization", format!("Bearer {bearer}"))
        .header("ICP-Agent-Id", AGENT)
        .body(Body::empty())
        .unwrap();
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

// --------------------------------------------------------------------------

#[tokio::test]
async fn first_spender_owns_the_readable_view() {
    let (state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    // Tenant A spends against the mandate first → owns the view.
    let jti = "m_first_spender";
    let now = Utc::now();
    state
        .service
        .mandates
        .record_spend(jti, "tenant_a", 1500, now);

    let (status, body) = get(&app, &format!("/icp/v1/mandates/{jti}/usage"), "k_a").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["jti"], jti);
    assert_eq!(body["spent_minor"], 1500);
    assert!(body["window_start"].is_string());

    // Tenant B reading → 404. Surfacing 200 (with usage) or 403
    // would let B confirm the jti exists.
    let (status, _) = get(&app, &format!("/icp/v1/mandates/{jti}/usage"), "k_b").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-owning tenant must see 404, not 403 or empty 200"
    );
}

#[tokio::test]
async fn second_spender_consumes_budget_but_cannot_read_tally() {
    let (state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    // Tenant A claims ownership with the first spend.
    let jti = "m_shared_budget";
    let now = Utc::now();
    state
        .service
        .mandates
        .record_spend(jti, "tenant_a", 1000, now);
    // Tenant B subsequently spends — counts toward the shared budget
    // (this is the property that protects the principal: a leaked
    // mandate JTI being used by another tenant still gets debited
    // against the same budget bucket and exhausts it).
    state
        .service
        .mandates
        .record_spend(jti, "tenant_b", 2500, now);

    // Direct ledger read: total is the sum, ownership stays with A.
    let usage = state.service.mandates.usage(jti);
    assert_eq!(
        usage.spent_minor, 3500,
        "shared budget must aggregate across tenants"
    );
    assert_eq!(
        usage.tenant_id, "tenant_a",
        "ownership stays with the first spender even after later spends"
    );

    // Tenant A still owns the view → sees the full aggregated tally.
    let (status, body) = get(&app, &format!("/icp/v1/mandates/{jti}/usage"), "k_a").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["spent_minor"], 3500);

    // Tenant B can spend but cannot read.
    let (status, _) = get(&app, &format!("/icp/v1/mandates/{jti}/usage"), "k_b").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-owning tenant must NOT see the aggregated tally even after contributing to it"
    );
}

#[tokio::test]
async fn never_spent_jti_returns_404_not_empty_200() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;

    // No spend recorded → no row → 404. (Previously this returned
    // a 200 with `spent_minor = 0`, which would let any caller probe
    // arbitrary jtis to see the response shape.)
    let (status, _) = get(&app, "/icp/v1/mandates/m_never_existed/usage", "k_a").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "unknown jti must 404 — fabricating a 200 with zeros is enumeration-friendly"
    );
}

#[tokio::test]
async fn unauthenticated_usage_read_is_401() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let req = Request::builder()
        .method("GET")
        .uri("/icp/v1/mandates/anything/usage")
        .header("ICP-Agent-Id", AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn ledger_unit_first_spender_owns_subsequent_spends_aggregate() {
    // Direct ledger test against a fresh in-memory pool — verifies
    // the API-level isolation tests above are exercising the right
    // underlying primitive, not just the route handler's filter.
    let pool = state_db::open(":memory:").expect("pool");
    let ledger = MandateLedger::with_pool(pool);
    let now = Utc::now();
    let jti = "m_ledger_unit";

    // Tenant A first.
    ledger.record_spend(jti, "tenant_a", 100, now);
    // Tenant B contributes.
    ledger.record_spend(jti, "tenant_b", 50, now);
    // Tenant A again.
    ledger.record_spend(jti, "tenant_a", 25, now);

    // Direct usage: aggregate of 175, owner = tenant_a.
    let u = ledger.usage(jti);
    assert_eq!(u.spent_minor, 175);
    assert_eq!(u.tenant_id, "tenant_a");

    // Scoped lookup: A sees it, B doesn't, and a never-spent jti
    // is None for everyone.
    let MandateUsage {
        spent_minor: a_spent,
        tenant_id: a_owner,
        ..
    } = ledger
        .usage_for_tenant(jti, "tenant_a")
        .expect("owner can read");
    assert_eq!(a_spent, 175);
    assert_eq!(a_owner, "tenant_a");

    assert!(
        ledger.usage_for_tenant(jti, "tenant_b").is_none(),
        "non-owner must get None"
    );
    assert!(
        ledger.usage_for_tenant("m_unseen", "tenant_a").is_none(),
        "never-spent jti must get None"
    );
}
