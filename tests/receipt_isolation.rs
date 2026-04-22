//! Tenant scoping for `GET /icp/v1/receipts/:jti`.
//!
//! Receipts don't carry a `tenant_id` of their own — the signed
//! claims shape is wire-stable and changing it would break receipt
//! verifiers. Instead the handler derives ownership at read time
//! via `claims.icp.transaction_id` → transaction lookup. Asserts:
//!   * Same-tenant read of a receipt produced by that tenant works.
//!   * Cross-tenant read returns **404** (not 403) — same shape as
//!     a missing jti, so existence isn't leakable across tenants.
//!   * Receipts whose backing transaction has been GC'd or has no
//!     `tenant_id` are unreadable to any real tenant — safe default.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo, build_app_state, build_router, config::Config, AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:rcpt-iso";

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

/// Returns `(receipt_jti, transaction_id)` from a vanilla quote.
async fn quote_returns_rcpt_and_txn(app: &Router, bearer: &str) -> (String, String) {
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
    (
        body["receipt"]["jti"].as_str().unwrap().to_string(),
        body["transaction"]["id"].as_str().unwrap().to_string(),
    )
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn same_tenant_can_read_its_own_receipt() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (jti, txn_id) = quote_returns_rcpt_and_txn(&app, "k_a").await;

    let (status, body) = send(&app, "GET", &format!("/icp/v1/receipts/{jti}"), "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["jti"], jti);
    assert_eq!(
        body["claims"]["icp"]["transaction_id"], txn_id,
        "claim's transaction_id is the join key the handler uses for ownership"
    );
}

#[tokio::test]
async fn cross_tenant_receipt_read_is_404_not_403() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;
    let (jti_a, _) = quote_returns_rcpt_and_txn(&app, "k_a").await;

    // Tenant B attempts to read tenant A's receipt → must 404 (not
    // 403). Surfacing 403 would confirm the jti exists, letting B
    // probe A's jti space.
    let (status, _) = send(
        &app,
        "GET",
        &format!("/icp/v1/receipts/{jti_a}"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant receipt read must look identical to a missing jti"
    );

    // Sanity: a genuinely-unknown jti also 404s — the path shape
    // matches.
    let (status, _) = send(&app, "GET", "/icp/v1/receipts/rcpt_nope", "k_b", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn receipt_whose_transaction_lacks_tenant_id_is_invisible_to_real_tenants() {
    use stateset_icp_handler::models::{Buyer, Totals, Transaction, TransactionState};
    use stateset_icp_handler::receipts::StoredReceipt;
    use stateset_icp_handler::signing::{ReceiptClaims, ReceiptIcp};

    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // Hand-craft a legacy artifact pair: a transaction with no
    // tenant_id (mirrors a row written by a pre-multi-tenant handler
    // before the field existed) plus a receipt that signs over it.
    let txn_id = "txn_legacy_orphan".to_string();
    let jti = "rcpt_legacy_orphan".to_string();
    let now = chrono::Utc::now();
    let legacy_txn = Transaction {
        id: txn_id.clone(),
        state: TransactionState::Completed,
        agent_id: AGENT.to_string(),
        tenant_id: String::new(), // ← legacy: no tenant stamped
        mandate_jti: None,
        currency: "USD".to_string(),
        jurisdiction: None,
        buyer: Buyer::default(),
        ship_to: None,
        bill_to: None,
        line_items: Vec::new(),
        totals: Totals::default(),
        order_id: None,
        quote_expires_at: None,
        created_at: now,
        updated_at: now,
        external_refs: Default::default(),
    };
    state.service.transactions.insert(legacy_txn);

    let claims = ReceiptClaims {
        iss: "icp://test-handler".into(),
        aud: AGENT.to_string(),
        iat: now.timestamp(),
        jti: jti.clone(),
        icp: ReceiptIcp {
            version: "2026-04-21".into(),
            intent: "intent.buy".into(),
            transaction_id: txn_id.clone(),
            order_id: None,
            mandate_jti: None,
            body_digest: "sha-256=legacy".into(),
            body_canonicalization: "JCS".into(),
        },
    };
    state.service.receipts.insert(StoredReceipt {
        jti: jti.clone(),
        kid: "test-key".into(),
        jws: "legacy.jws.bytes".into(),
        body_digest: "sha-256=legacy".into(),
        claims,
    });

    // Tenant A (any real tenant) reading the orphan receipt → 404.
    // Conservative default: rather than expose every legacy receipt
    // to whoever asks first, we hide them entirely.
    let (status, _) = send(&app, "GET", &format!("/icp/v1/receipts/{jti}"), "k_a", None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "legacy receipt with no tenant_id on its transaction must be invisible to real tenants"
    );
}

#[tokio::test]
async fn list_receipts_returns_only_callers_tenant_rows() {
    let (_state, app) = build(vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;

    // Two A receipts, one B receipt. Receipts come from real intent
    // activity so we know the join-via-transaction path works
    // end-to-end (rather than hand-crafting receipts that bypass
    // the production code path).
    let _ = quote_returns_rcpt_and_txn(&app, "k_a").await;
    let _ = quote_returns_rcpt_and_txn(&app, "k_a").await;
    let _ = quote_returns_rcpt_and_txn(&app, "k_b").await;

    let (status, body) = send(&app, "GET", "/icp/v1/receipts", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 2, "tenant A sees only its own 2 receipts");

    let (_, body) = send(&app, "GET", "/icp/v1/receipts", "k_b", None).await;
    assert_eq!(body["count"], 1);
}

#[tokio::test]
async fn list_receipts_intent_filter_narrows() {
    use stateset_icp_handler::models::{Buyer, Totals, Transaction, TransactionState};
    use stateset_icp_handler::receipts::StoredReceipt;
    use stateset_icp_handler::signing::{ReceiptClaims, ReceiptIcp};

    let (state, app) = build(vec![key("a", "tenant_a")]).await;

    // One quote receipt from real activity.
    let _ = quote_returns_rcpt_and_txn(&app, "k_a").await;

    // Hand-craft a buy receipt + matching transaction so we have
    // distinct intents to filter on.
    let now = chrono::Utc::now();
    let buy_txn = Transaction {
        id: "txn_buy_a".into(),
        state: TransactionState::Completed,
        agent_id: AGENT.to_string(),
        tenant_id: "tenant_a".into(),
        mandate_jti: None,
        currency: "USD".into(),
        jurisdiction: None,
        buyer: Buyer::default(),
        ship_to: None,
        bill_to: None,
        line_items: Vec::new(),
        totals: Totals::default(),
        order_id: None,
        quote_expires_at: None,
        created_at: now,
        updated_at: now,
        external_refs: Default::default(),
    };
    state.service.transactions.insert(buy_txn);
    state.service.receipts.insert(StoredReceipt {
        jti: "rcpt_buy_a".into(),
        kid: "test-key".into(),
        jws: "buy.jws".into(),
        body_digest: "sha-256=buy".into(),
        claims: ReceiptClaims {
            iss: "icp://test-handler".into(),
            aud: AGENT.into(),
            iat: now.timestamp(),
            jti: "rcpt_buy_a".into(),
            icp: ReceiptIcp {
                version: "2026-04-21".into(),
                intent: "intent.buy".into(),
                transaction_id: "txn_buy_a".into(),
                order_id: None,
                mandate_jti: None,
                body_digest: "sha-256=buy".into(),
                body_canonicalization: "JCS".into(),
            },
        },
    });

    // Without filter: 2 receipts.
    let (_, body) = send(&app, "GET", "/icp/v1/receipts", "k_a", None).await;
    assert_eq!(body["count"], 2);

    // ?intent=intent.quote → 1.
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/receipts?intent=intent.quote",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["claims"]["icp"]["intent"], "intent.quote");

    // ?intent=intent.buy → 1.
    let (_, body) = send(
        &app,
        "GET",
        "/icp/v1/receipts?intent=intent.buy",
        "k_a",
        None,
    )
    .await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["claims"]["icp"]["intent"], "intent.buy");

    // ?intent=intent.refund_request → 0 (no such receipt). Empty
    // list is success, not 404 — same shape as the other list
    // endpoints.
    let (status, body) = send(
        &app,
        "GET",
        "/icp/v1/receipts?intent=intent.refund_request",
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn list_receipts_empty_is_200_not_404() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (status, body) = send(&app, "GET", "/icp/v1/receipts", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn list_receipts_unauthenticated_is_401() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let req = Request::builder()
        .method("GET")
        .uri("/icp/v1/receipts")
        .header("ICP-Agent-Id", AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unauthenticated_receipt_read_is_401() {
    let (_state, app) = build(vec![key("a", "tenant_a")]).await;
    let (jti, _) = quote_returns_rcpt_and_txn(&app, "k_a").await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/icp/v1/receipts/{jti}"))
        .header("ICP-Agent-Id", AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
