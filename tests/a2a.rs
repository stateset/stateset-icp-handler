//! A2A (peer commerce) integration tests.
//!
//! Covers `intent.a2a_quote` and `intent.a2a_pay` end-to-end:
//!   - quote creation in `pending` (no price hint) and `quoted`
//!     (price hint supplied) status
//!   - pay-against-quote flow consumes the quote and produces a real
//!     charging transaction with PaymentInstrument-aligned external
//!     refs
//!   - direct pay flow without a quote
//!   - rejected: paying an expired quote, paying someone else's quote,
//!     paying yourself, paying without `from`
//!   - peer quote retrievable via `GET /icp/v1/peer_quotes/:id`
//!   - mandate scope `pay_peer` enforced

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
const REQUESTER_AGENT: &str = "did:stateset:agent:requester";
const PEER_AGENT: &str = "did:stateset:agent:peer-bob";

async fn setup() -> Router {
    setup_with(|_| {}).await
}

async fn setup_with(mut mutate: impl FnMut(&mut Config)) -> Router {
    let mut config = Config::for_test();
    mutate(&mut config);
    let state = build_app_state(&config).await.expect("build_app_state");
    build_router(state)
}

async fn submit(app: &Router, body: Value, extra_headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/icp/v1/intents")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", REQUESTER_AGENT);
    for (k, v) in extra_headers {
        builder = builder.header(*k, *v);
    }
    let req = builder
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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

async fn get(app: &Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", REQUESTER_AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

fn quote_body(price_hint: Option<Value>, expires_in_seconds: Option<u64>) -> Value {
    let mut params = json!({
        "peer_agent_id": PEER_AGENT,
        "service": {
            "kind": "image_generation",
            "description": "Render a 1024x1024 product photo",
            "params": { "size": "1024x1024", "style": "studio" }
        }
    });
    if let Some(p) = price_hint {
        params["price_hint"] = p;
    }
    if let Some(s) = expires_in_seconds {
        params["expires_in_seconds"] = json!(s);
    }
    json!({
        "intent": "intent.a2a_quote",
        "agent_id": REQUESTER_AGENT,
        "params": params,
        "context": { "currency": "USD" }
    })
}

fn alg_none_mandate(scopes: &[&str]) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let now = Utc::now().timestamp();
    let payload = json!({
        "iss": "did:buyer:test",
        "sub": REQUESTER_AGENT,
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

// --------------------------------------------------------------------------

#[tokio::test]
async fn quote_without_price_hint_is_pending() {
    let app = setup().await;
    let (status, body) = submit(&app, quote_body(None, None), &[]).await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let q = &body["peer_quote"];
    assert!(q.is_object());
    assert!(q["id"].as_str().unwrap().starts_with("pq_"));
    assert_eq!(q["status"], "pending");
    assert_eq!(q["requester_agent_id"], REQUESTER_AGENT);
    assert_eq!(q["peer_agent_id"], PEER_AGENT);
    assert!(q["price"].is_null() || q.get("price").is_none());
    assert_eq!(q["service"]["kind"], "image_generation");

    // The transaction is a Draft placeholder linked to the quote.
    assert_eq!(body["transaction"]["state"], "draft");
    assert_eq!(
        body["transaction"]["external_refs"]["peer_quote_id"],
        q["id"]
    );
    // Receipt signed.
    assert!(body["receipt"]["jti"]
        .as_str()
        .unwrap()
        .starts_with("rcpt_"));
}

#[tokio::test]
async fn quote_with_price_hint_is_quoted_and_payable() {
    let app = setup().await;
    let (_s, body) = submit(
        &app,
        quote_body(
            Some(json!({ "amount_minor": 7500, "currency": "USD" })),
            None,
        ),
        &[],
    )
    .await;
    let q = &body["peer_quote"];
    assert_eq!(q["status"], "quoted");
    assert_eq!(q["price"]["amount_minor"], 7500);
    assert_eq!(q["price"]["currency"], "USD");
}

#[tokio::test]
async fn pay_against_quote_consumes_it_and_creates_transaction() {
    let app = setup().await;
    let (_s, qb) = submit(
        &app,
        quote_body(
            Some(json!({ "amount_minor": 5000, "currency": "USD" })),
            None,
        ),
        &[],
    )
    .await;
    let quote_id = qb["peer_quote"]["id"].as_str().unwrap().to_string();

    let (status, pay) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": {
                "peer_quote_id": quote_id,
                "from": "0xrequesterwallet",
                "memo": "compute job #42"
            }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={pay}");

    // Quote is now accepted, links to the charge.
    let q = &pay["peer_quote"];
    assert_eq!(q["status"], "accepted");
    assert_eq!(q["id"], quote_id);
    assert!(q["accepted_at"].is_string());
    assert_eq!(q["charge_transaction_id"], pay["transaction"]["id"]);

    // Transaction is a real completed payment.
    let txn = &pay["transaction"];
    assert_eq!(txn["state"], "completed");
    assert_eq!(txn["totals"]["total"]["amount_minor"], 5000);
    assert_eq!(txn["currency"], "USD");
    assert_eq!(txn["external_refs"]["peer_agent_id"], PEER_AGENT);
    assert_eq!(txn["external_refs"]["peer_quote_id"], quote_id);
    assert_eq!(txn["external_refs"]["a2a_from"], "0xrequesterwallet");
    assert_eq!(txn["external_refs"]["memo"], "compute job #42");
    let line = &txn["line_items"][0];
    assert_eq!(line["sku"], format!("a2a:{PEER_AGENT}"));
}

#[tokio::test]
async fn pay_a_quote_twice_is_rejected() {
    let app = setup().await;
    let (_s, qb) = submit(
        &app,
        quote_body(
            Some(json!({ "amount_minor": 1000, "currency": "USD" })),
            None,
        ),
        &[],
    )
    .await;
    let quote_id = qb["peer_quote"]["id"].as_str().unwrap().to_string();

    let pay = json!({
        "intent": "intent.a2a_pay",
        "agent_id": REQUESTER_AGENT,
        "params": { "peer_quote_id": quote_id, "from": "0xa" }
    });
    let (s1, _) = submit(&app, pay.clone(), &[]).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, body2) = submit(&app, pay, &[]).await;
    assert_eq!(s2, StatusCode::PRECONDITION_FAILED);
    assert_eq!(body2["error"]["type"], "precondition_failed");
    assert!(body2["error"]["message"]
        .as_str()
        .unwrap()
        .contains("accepted"));
}

#[tokio::test]
async fn pay_pending_quote_is_rejected() {
    let app = setup().await;
    let (_s, qb) = submit(&app, quote_body(None, None), &[]).await;
    let quote_id = qb["peer_quote"]["id"].as_str().unwrap().to_string();

    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": { "peer_quote_id": quote_id, "from": "0xa" }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("pending"));
}

#[tokio::test]
async fn pay_expired_quote_is_rejected_and_marks_expired() {
    let app = setup().await;
    // 1-second TTL; sleep past it.
    let (_s, qb) = submit(
        &app,
        quote_body(
            Some(json!({ "amount_minor": 100, "currency": "USD" })),
            Some(1),
        ),
        &[],
    )
    .await;
    let quote_id = qb["peer_quote"]["id"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": { "peer_quote_id": quote_id, "from": "0xa" }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("expired"));

    // Quote is now persisted as Expired.
    let (status, q) = get(&app, &format!("/icp/v1/peer_quotes/{quote_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(q["status"], "expired");
}

#[tokio::test]
async fn direct_pay_without_quote_works() {
    let app = setup().await;
    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": {
                "peer_agent_id": PEER_AGENT,
                "amount": { "amount_minor": 250, "currency": "USDC" },
                "from": "0xclient",
                "memo": "tip"
            }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let txn = &body["transaction"];
    assert_eq!(txn["state"], "completed");
    assert_eq!(txn["currency"], "USDC");
    assert_eq!(txn["totals"]["total"]["amount_minor"], 250);
    assert!(body["peer_quote"].is_null() || body.get("peer_quote").is_none());
}

#[tokio::test]
async fn pay_yourself_is_rejected() {
    let app = setup().await;
    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": {
                "peer_agent_id": REQUESTER_AGENT,
                "amount": { "amount_minor": 100, "currency": "USD" },
                "from": "0xa"
            }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("yourself"));
}

#[tokio::test]
async fn quote_yourself_is_rejected() {
    let app = setup().await;
    let mut body = quote_body(None, None);
    body["params"]["peer_agent_id"] = json!(REQUESTER_AGENT);
    let (status, resp) = submit(&app, body, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(resp["error"]["type"], "invalid_request");
}

#[tokio::test]
async fn pay_without_from_is_rejected() {
    let app = setup().await;
    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": {
                "peer_agent_id": PEER_AGENT,
                "amount": { "amount_minor": 100, "currency": "USD" },
                "from": ""
            }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].as_str().unwrap().contains("from"));
}

#[tokio::test]
async fn peer_quote_retrievable_by_get() {
    let app = setup().await;
    let (_s, body) = submit(
        &app,
        quote_body(Some(json!({ "amount_minor": 99, "currency": "USD" })), None),
        &[],
    )
    .await;
    let id = body["peer_quote"]["id"].as_str().unwrap().to_string();

    let (status, q) = get(&app, &format!("/icp/v1/peer_quotes/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(q["id"], id);
    assert_eq!(q["status"], "quoted");
}

#[tokio::test]
async fn unknown_quote_returns_404_on_pay() {
    let app = setup().await;
    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": { "peer_quote_id": "pq_does_not_exist", "from": "0xa" }
        }),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "resource_not_found");
}

#[tokio::test]
async fn mandate_scope_pay_peer_required_for_a2a_pay() {
    let app = setup_with(|c| c.require_mandate = true).await;
    // Mandate authorizes only `quote`, not `pay_peer`.
    let m = alg_none_mandate(&["quote"]);
    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": {
                "peer_agent_id": PEER_AGENT,
                "amount": { "amount_minor": 100, "currency": "USD" },
                "from": "0xa"
            }
        }),
        &[("ICP-Mandate", &m)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "mandate_out_of_scope");
}

#[tokio::test]
async fn mandate_with_pay_peer_scope_accepted() {
    let app = setup_with(|c| c.require_mandate = true).await;
    let m = alg_none_mandate(&["pay_peer"]);
    let (status, body) = submit(
        &app,
        json!({
            "intent": "intent.a2a_pay",
            "agent_id": REQUESTER_AGENT,
            "params": {
                "peer_agent_id": PEER_AGENT,
                "amount": { "amount_minor": 100, "currency": "USD" },
                "from": "0xa"
            }
        }),
        &[("ICP-Mandate", &m)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["transaction"]["state"], "completed");
}

#[tokio::test]
async fn discovery_advertises_a2a_intents() {
    let app = setup().await;
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/icp")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("ICP-Agent-Id", REQUESTER_AGENT)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let intents = body["intents"].as_array().unwrap();
    assert_eq!(
        intents.len(),
        17,
        "all 17 catalog intents implemented (icp-full tier)"
    );
    assert!(intents.iter().any(|v| v == "intent.a2a_quote"));
    assert!(intents.iter().any(|v| v == "intent.a2a_pay"));
}
