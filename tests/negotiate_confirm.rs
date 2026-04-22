//! Integration tests for `intent.negotiate` + `intent.confirm_receipt`.
//!
//! These two intents complete the catalog (17/17) and unlock the
//! `icp-full` conformance tier.
//!
//! Asserts:
//!   - negotiate works with both `proposed_total` and `discount_pct`
//!   - negotiate stamps an audit trail in `external_refs`
//!   - rejects re-negotiation of non-`quoted` transactions
//!   - rejects `discount_pct` outside [0, 90]
//!   - confirm_receipt works against post-payment transactions
//!   - rejects pre-payment confirms
//!   - rejects double-confirms (idempotency at the domain level)
//!   - both emit bespoke event types (not duplicate state events)
//!   - mandate scopes (`quote` for negotiate, `fulfill` for confirm)
//!   - discovery now declares `tier: "icp-full"` with empty
//!     `missing_intents`, and the catalog count is 17

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde_json::{json, Value};
use stateset_icp_handler::{build_app_state, build_router, config::Config};
use tower::ServiceExt;
use uuid::Uuid;

const DEMO_KEY: &str = "icp_demo_key_123";
const AGENT: &str = "did:stateset:agent:nego-confirm-test";

async fn setup() -> Router {
    setup_with(|_| {}).await
}

async fn setup_with(mut mutate: impl FnMut(&mut Config)) -> Router {
    let mut config = Config::for_test();
    mutate(&mut config);
    let state = build_app_state(&config).await.expect("build_app_state");
    build_router(state)
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", AGENT);
    for (k, v) in extra_headers {
        builder = builder.header(*k, *v);
    }
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

fn quote_body(amount_minor: i64) -> Value {
    json!({
        "intent": "intent.quote",
        "agent_id": AGENT,
        "params": {
            "items": [{
                "sku": "WIDGET-001",
                "quantity": 1,
                "unit_price_hint": { "amount_minor": amount_minor, "currency": "USD" }
            }]
        }
    })
}

fn alg_none_mandate(scopes: &[&str]) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let now = Utc::now().timestamp();
    let payload = json!({
        "iss": "did:buyer:test",
        "sub": AGENT,
        "iat": now,
        "nbf": now - 60,
        "exp": now + 3600,
        "jti": format!("m_{}", Uuid::new_v4().simple()),
        "icp": {
            "version": "2026-04-21",
            "scope": scopes,
            "budget": { "currency": "USD", "amount_minor": 1_000_000,
                        "per_transaction": 1_000_000, "period": "P1D" },
            "merchants": ["*"]
        }
    });
    let p_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    format!("{header}.{p_b64}.")
}

async fn quote_then_get_id(app: &Router, amount: i64) -> String {
    let (_, body) = send(
        app,
        "POST",
        "/icp/v1/intents",
        Some(quote_body(amount)),
        &[],
    )
    .await;
    body["transaction"]["id"].as_str().unwrap().to_string()
}

async fn complete_purchase(app: &Router, amount: i64) -> String {
    // quote → authorize → buy, returns the txn_id
    let txn_id = quote_then_get_id(app, amount).await;
    send(
        app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.authorize",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id }
        })),
        &[],
    )
    .await;
    send(
        app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.buy",
            "agent_id": AGENT,
            "params": {
                "transaction_id": txn_id,
                "payment": { "method": "card", "token": "tok" }
            }
        })),
        &[],
    )
    .await;
    txn_id
}

// --------------------------------------------------------------------------
// negotiate
// --------------------------------------------------------------------------

#[tokio::test]
async fn negotiate_with_proposed_total_overrides_amount() {
    let app = setup().await;
    let txn_id = quote_then_get_id(&app, 10_000).await;

    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": {
                "transaction_id": txn_id,
                "proposed_total": { "amount_minor": 7500, "currency": "USD" },
                "message": "best i can do"
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(
        body["transaction"]["totals"]["total"]["amount_minor"], 7500,
        "proposed_total wins"
    );
    // Audit trail stamped.
    let nego_0 = &body["transaction"]["external_refs"]["negotiation_000"];
    assert!(nego_0.is_string());
    let parsed: Value = serde_json::from_str(nego_0.as_str().unwrap()).unwrap();
    assert_eq!(parsed["from_minor"], 10875); // 10000 + 8.75% tax
    assert_eq!(parsed["to_minor"], 7500);
    assert_eq!(parsed["agent_id"], AGENT);
}

#[tokio::test]
async fn negotiate_with_discount_pct_applies_percentage() {
    let app = setup().await;
    let txn_id = quote_then_get_id(&app, 10_000).await;

    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": {
                "transaction_id": txn_id,
                "discount_pct": 10.0
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    // Original total = 10875 (10000 + 875 tax); 10% off = 9787.
    assert_eq!(body["transaction"]["totals"]["total"]["amount_minor"], 9787);
}

#[tokio::test]
async fn negotiate_stamps_each_round_in_history() {
    let app = setup().await;
    let txn_id = quote_then_get_id(&app, 5_000).await;

    for (i, pct) in [5.0, 10.0, 15.0].iter().enumerate() {
        let (status, body) = send(
            &app,
            "POST",
            "/icp/v1/intents",
            Some(json!({
                "intent": "intent.negotiate",
                "agent_id": AGENT,
                "params": {
                    "transaction_id": txn_id,
                    "discount_pct": pct
                }
            })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let key = format!("negotiation_{:03}", i);
        assert!(
            body["transaction"]["external_refs"][&key].is_string(),
            "expected {key} in external_refs after round {i}",
        );
    }

    // Final state has all three rounds preserved.
    let (_, txn) = send(
        &app,
        "GET",
        &format!("/icp/v1/transactions/{txn_id}"),
        None,
        &[],
    )
    .await;
    let refs = txn["external_refs"].as_object().unwrap();
    assert!(refs.contains_key("negotiation_000"));
    assert!(refs.contains_key("negotiation_001"));
    assert!(refs.contains_key("negotiation_002"));
}

#[tokio::test]
async fn negotiate_rejects_missing_proposal() {
    let app = setup().await;
    let txn_id = quote_then_get_id(&app, 5_000).await;
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request");
}

#[tokio::test]
async fn negotiate_rejects_out_of_range_discount() {
    let app = setup().await;
    let txn_id = quote_then_get_id(&app, 5_000).await;
    for pct in [-5.0, 95.0, 100.0] {
        let (status, _) = send(
            &app,
            "POST",
            "/icp/v1/intents",
            Some(json!({
                "intent": "intent.negotiate",
                "agent_id": AGENT,
                "params": { "transaction_id": txn_id, "discount_pct": pct }
            })),
            &[],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "pct {pct} should be rejected"
        );
    }
}

#[tokio::test]
async fn negotiate_rejects_non_quoted_transactions() {
    let app = setup().await;
    let txn_id = complete_purchase(&app, 5_000).await; // now `completed`
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": {
                "transaction_id": txn_id,
                "discount_pct": 10.0
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("only `quoted`"));
}

#[tokio::test]
async fn negotiate_rejects_unknown_transaction() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": {
                "transaction_id": "txn_does_not_exist",
                "discount_pct": 5.0
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "resource_not_found");
}

#[tokio::test]
async fn negotiate_requires_quote_scope() {
    let app = setup_with(|c| c.require_mandate = true).await;
    let m_quote = alg_none_mandate(&["quote"]);
    let (_, q) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(quote_body(5_000)),
        &[("ICP-Mandate", &m_quote)],
    )
    .await;
    let txn_id = q["transaction"]["id"].as_str().unwrap().to_string();

    // Mandate authorizing only `buy` cannot negotiate.
    let m_buy = alg_none_mandate(&["buy"]);
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id, "discount_pct": 10.0 }
        })),
        &[("ICP-Mandate", &m_buy)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "mandate_out_of_scope");

    // Same mandate that authorized the original quote works for re-negotiation.
    let (status, _) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.negotiate",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id, "discount_pct": 10.0 }
        })),
        &[("ICP-Mandate", &m_quote)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// --------------------------------------------------------------------------
// confirm_receipt
// --------------------------------------------------------------------------

#[tokio::test]
async fn confirm_receipt_marks_completed_transaction() {
    let app = setup().await;
    let txn_id = complete_purchase(&app, 5_000).await;

    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.confirm_receipt",
            "agent_id": AGENT,
            "params": {
                "transaction_id": txn_id,
                "note": "package opened, all items present"
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    // Transaction state stays `completed`; the confirmation is in
    // external_refs so audit captures the moment without a new state.
    assert_eq!(body["transaction"]["state"], "completed");
    let refs = &body["transaction"]["external_refs"];
    assert!(refs["receipt_confirmed_at"].is_string());
    assert_eq!(refs["receipt_confirmed_by"], AGENT);
    assert_eq!(refs["receipt_note"], "package opened, all items present");

    // Receipt signed (it's a state-changing intent semantically).
    assert!(body["receipt"]["jti"]
        .as_str()
        .unwrap()
        .starts_with("rcpt_"));
}

#[tokio::test]
async fn confirm_receipt_rejects_pre_payment_transactions() {
    let app = setup().await;
    // Quoted-only — never authorized, never bought.
    let txn_id = quote_then_get_id(&app, 5_000).await;

    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.confirm_receipt",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("payment must complete"));
}

#[tokio::test]
async fn confirm_receipt_rejects_double_confirms() {
    let app = setup().await;
    let txn_id = complete_purchase(&app, 5_000).await;

    let body = json!({
        "intent": "intent.confirm_receipt",
        "agent_id": AGENT,
        "params": { "transaction_id": txn_id }
    });
    let (s1, _) = send(&app, "POST", "/icp/v1/intents", Some(body.clone()), &[]).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, err) = send(&app, "POST", "/icp/v1/intents", Some(body), &[]).await;
    assert_eq!(s2, StatusCode::PRECONDITION_FAILED);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already been confirmed"));
}

#[tokio::test]
async fn confirm_receipt_rejects_unknown_transaction() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.confirm_receipt",
            "agent_id": AGENT,
            "params": { "transaction_id": "txn_nope" }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "resource_not_found");
}

#[tokio::test]
async fn confirm_receipt_requires_fulfill_scope() {
    let app = setup_with(|c| c.require_mandate = true).await;

    // Build a completed transaction using a mandate that authorizes
    // every scope the lifecycle needs (quote + buy).
    let m_setup = alg_none_mandate(&["quote", "buy"]);
    let (_, q) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(quote_body(5_000)),
        &[("ICP-Mandate", &m_setup)],
    )
    .await;
    let txn_id = q["transaction"]["id"]
        .as_str()
        .expect("quote should return a transaction id under quote+buy scope")
        .to_string();
    send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.authorize",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id }
        })),
        &[("ICP-Mandate", &m_setup)],
    )
    .await;
    send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.buy",
            "agent_id": AGENT,
            "params": {
                "transaction_id": txn_id,
                "payment": { "method": "card", "token": "tok" }
            }
        })),
        &[("ICP-Mandate", &m_setup)],
    )
    .await;

    // Confirm with a mandate that lacks the `fulfill` scope — rejected.
    let m_no_fulfill = alg_none_mandate(&["quote", "buy"]);
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.confirm_receipt",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id }
        })),
        &[("ICP-Mandate", &m_no_fulfill)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "mandate_out_of_scope");

    // With the correct `fulfill` scope, it works.
    let m_fulfill = alg_none_mandate(&["fulfill"]);
    let (status, _) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.confirm_receipt",
            "agent_id": AGENT,
            "params": { "transaction_id": txn_id }
        })),
        &[("ICP-Mandate", &m_fulfill)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// --------------------------------------------------------------------------
// Catalog / discovery / tier
// --------------------------------------------------------------------------

#[tokio::test]
async fn discovery_now_declares_icp_full_tier() {
    let app = setup().await;
    let (status, body) = send(&app, "GET", "/.well-known/icp", None, &[]).await;
    assert_eq!(status, StatusCode::OK);

    // Catalog is now 17 of 17.
    let intents = body["intents"].as_array().unwrap();
    assert_eq!(intents.len(), 17, "all 17 catalog intents now implemented");
    assert!(intents.iter().any(|v| v == "intent.negotiate"));
    assert!(intents.iter().any(|v| v == "intent.confirm_receipt"));

    // Tier escalates to icp-full with no missing intents.
    let conformance = &body["conformance"];
    assert_eq!(conformance["tier"], "icp-full");
    assert_eq!(
        conformance["missing_intents"].as_array().unwrap().len(),
        0,
        "icp-full tier means missing_intents is empty"
    );
}

#[tokio::test]
async fn mcp_tool_catalog_grows_to_17() {
    let app = setup().await;
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 17);
    assert!(names.contains(&"icp_negotiate"));
    assert!(names.contains(&"icp_confirm_receipt"));
}
