//! Integration tests for the ICP handler.
//!
//! These drive the real `axum::Router` returned by `build_router` in
//! process — no network, no SQLite file. Every test constructs its own
//! `AppState` so the tests are independent and can run in parallel.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde_json::{json, Value};
use stateset_icp_handler::{build_app_state, build_router, config::Config};
use tower::ServiceExt;
use uuid::Uuid;

// --------------------------------------------------------------------------
// Test harness
// --------------------------------------------------------------------------

const DEMO_KEY: &str = "icp_demo_key_123";
const DEMO_AGENT: &str = "did:stateset:agent:test";

async fn setup() -> Router {
    setup_with(Config::for_test()).await
}

async fn setup_with(config: Config) -> Router {
    let state = build_app_state(&config).await.expect("build_app_state");
    build_router(state)
}

/// Build a test config with the embedded iCommerce engine enabled at a
/// per-test temporary SQLite path. SQLite creates the file lazily, so
/// the random suffix gives every concurrent test its own isolated DB.
fn config_with_engine() -> Config {
    let mut cfg = Config::for_test();
    cfg.commerce_enabled = true;
    cfg.commerce_db_path = format!("/tmp/icp_test_{}.db", Uuid::new_v4().simple());
    cfg
}

fn req(method: &str, path: &str) -> RequestBuilder {
    RequestBuilder {
        method: method.to_string(),
        path: path.to_string(),
        headers: vec![
            ("Authorization".into(), format!("Bearer {DEMO_KEY}")),
            ("ICP-Agent-Id".into(), DEMO_AGENT.into()),
        ],
        body: None,
    }
}

struct RequestBuilder {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

impl RequestBuilder {
    fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    fn without_header(mut self, k: &str) -> Self {
        self.headers.retain(|(name, _)| name != k);
        self
    }

    fn json_body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    fn build(self) -> Request<Body> {
        let mut b = Request::builder()
            .method(self.method.as_str())
            .uri(&self.path);
        for (k, v) in self.headers {
            b = b.header(k, v);
        }
        let body = match self.body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        b.body(body).expect("build request")
    }
}

async fn send(app: &Router, rb: RequestBuilder) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(rb.build()).await.expect("send request");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect body");
    let v: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

fn make_mandate_jws(scopes: &[&str], amount_minor: i64, per_txn: Option<i64>) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let now = Utc::now().timestamp();
    let payload = json!({
        "iss": "did:buyer:alice",
        "sub": DEMO_AGENT,
        "iat": now,
        "nbf": now - 60,
        "exp": now + 3600,
        "jti": format!("m_{}", Uuid::new_v4().simple()),
        "icp": {
            "version": "2026-04-21",
            "scope": scopes,
            "budget": {
                "currency": "USD",
                "amount_minor": amount_minor,
                "per_transaction": per_txn,
                "period": "P1D",
            },
            "merchants": ["*"],
        },
    });
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    format!("{header}.{payload_b64}.")
}

fn quote_body() -> Value {
    json!({
        "intent": "intent.quote",
        "agent_id": DEMO_AGENT,
        "params": {
            "items": [
                { "sku": "WIDGET-001", "quantity": 2,
                  "unit_price_hint": { "amount_minor": 2999, "currency": "USD" } }
            ],
            "buyer": { "first_name": "Alice", "last_name": "Smith",
                       "email": "alice@example.com" }
        },
        "context": { "currency": "USD", "jurisdiction": "US-CA" }
    })
}

// --------------------------------------------------------------------------
// Service endpoints
// --------------------------------------------------------------------------

#[tokio::test]
async fn root_endpoint_returns_banner() {
    let app = setup().await;
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let s = std::str::from_utf8(&bytes).unwrap();
    assert!(s.contains("ICP"));
}

#[tokio::test]
async fn health_reports_version_and_status() {
    let app = setup().await;
    let (status, body) = send(&app, req("GET", "/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "healthy");
    assert_eq!(body["icp_version"], "2026-04-21");
    assert_eq!(body["service"], "stateset-icp-handler");
}

#[tokio::test]
async fn icp_version_header_stamped_on_every_response() {
    let app = setup().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let header = resp
        .headers()
        .get("icp-version")
        .expect("ICP-Version header set");
    assert_eq!(header.to_str().unwrap(), "2026-04-21");
}

#[tokio::test]
async fn discovery_document_advertises_full_catalog() {
    let app = setup().await;
    let (status, body) = send(&app, req("GET", "/.well-known/icp")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["icp_version"], "2026-04-21");
    let intents = body["intents"].as_array().expect("intents array");
    // All 17 catalog intents are now implemented (icp-full tier).
    assert_eq!(intents.len(), 17, "17 implemented intents advertised");
    assert!(intents.iter().any(|v| v == "intent.buy"));
    assert!(intents.iter().any(|v| v == "intent.quote"));
    assert!(intents.iter().any(|v| v == "intent.subscribe"));
    assert!(intents.iter().any(|v| v == "intent.a2a_pay"));
    assert!(intents.iter().any(|v| v == "intent.negotiate"));
    assert!(intents.iter().any(|v| v == "intent.confirm_receipt"));

    // Compatibility surfaces present.
    assert!(body["compatibility"]["acp"].is_object());
    assert!(body["compatibility"]["ucp"].is_object());
    assert!(body["compatibility"]["mcp"].is_object());
    assert!(body["compatibility"]["a2a"].is_object());

    // Signing keys advertised.
    let keys = body["signing_keys"].as_array().expect("signing keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["alg"], "EdDSA");
    assert_eq!(keys[0]["crv"], "Ed25519");
}

#[tokio::test]
async fn jwks_endpoint_returns_ed25519_key() {
    let app = setup().await;
    let (status, body) = send(&app, req("GET", "/.well-known/icp/jwks.json")).await;
    assert_eq!(status, StatusCode::OK);
    let keys = body["keys"].as_array().expect("keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["kty"], "OKP");
    assert_eq!(keys[0]["alg"], "EdDSA");
    assert_eq!(keys[0]["use"], "sig");
    let x = keys[0]["x"].as_str().expect("x");
    // Ed25519 pubkey is 32 bytes = 43 chars base64url (no padding).
    assert_eq!(x.len(), 43);
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text() {
    let app = setup().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .expect("content-type")
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.contains("text/plain"), "got content-type: {ct}");
}

// --------------------------------------------------------------------------
// Intent flow
// --------------------------------------------------------------------------

#[tokio::test]
async fn intent_quote_produces_quoted_transaction_with_totals() {
    let app = setup().await;
    let (status, body) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["intent"], "intent.quote");
    assert_eq!(body["transaction"]["state"], "quoted");
    assert_eq!(body["transaction"]["currency"], "USD");
    assert_eq!(
        body["transaction"]["line_items"].as_array().unwrap().len(),
        1
    );
    // subtotal = 2 * 2999 = 5998
    assert_eq!(
        body["transaction"]["totals"]["subtotal"]["amount_minor"],
        5998
    );
    // receipt on state-changing response
    assert!(body["receipt"]["jti"]
        .as_str()
        .unwrap()
        .starts_with("rcpt_"));
    assert_eq!(body["receipt"]["kid"], "icp-test-key");
    assert!(body["receipt"]["jws"].as_str().unwrap().contains('.'));
}

#[tokio::test]
async fn full_intent_flow_quote_authorize_buy() {
    let app = setup().await;

    // 1. Quote
    let (_s, quote) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;
    let txn_id = quote["transaction"]["id"].as_str().unwrap().to_string();
    assert_eq!(quote["transaction"]["state"], "quoted");

    // 2. Authorize
    let (_s, auth) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.authorize",
            "agent_id": DEMO_AGENT,
            "params": { "transaction_id": txn_id },
        })),
    )
    .await;
    assert_eq!(auth["transaction"]["state"], "authorized");
    assert_eq!(auth["transaction"]["id"], txn_id);

    // 3. Buy
    let (_s, buy) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.buy",
            "agent_id": DEMO_AGENT,
            "params": {
                "transaction_id": txn_id,
                "payment": {
                    "method": "card",
                    "token": "tok_demo",
                    "last_digits": "4242",
                    "brand": "visa",
                },
            },
        })),
    )
    .await;
    assert_eq!(buy["transaction"]["state"], "completed");
    let rcpt_jti = buy["receipt"]["jti"].as_str().unwrap().to_string();

    // 4. Retrieve transaction by id
    let (status, txn) = send(&app, req("GET", &format!("/icp/v1/transactions/{txn_id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(txn["state"], "completed");

    // 5. Retrieve receipt
    let (status, rcpt) = send(&app, req("GET", &format!("/icp/v1/receipts/{rcpt_jti}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rcpt["jti"], rcpt_jti);
    assert_eq!(rcpt["kid"], "icp-test-key");
    assert!(rcpt["body_digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(rcpt["claims"]["icp"]["intent"], "intent.buy");
    assert_eq!(rcpt["claims"]["icp"]["transaction_id"], txn_id);
}

#[tokio::test]
async fn buy_with_engine_persists_real_order() {
    // With the embedded iCommerce engine enabled, completing a buy MUST
    // populate the response `order` field with an engine-generated id
    // and order_number — proving the auto-product-seed → customer →
    // order pipeline in `commerce.rs` actually runs end-to-end against
    // a real SQLite database.
    let app = setup_with(config_with_engine()).await;

    let (_s, q) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;
    let txn_id = q["transaction"]["id"].as_str().unwrap().to_string();

    send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.authorize",
            "agent_id": DEMO_AGENT,
            "params": { "transaction_id": txn_id },
        })),
    )
    .await;

    let (status, buy) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.buy",
            "agent_id": DEMO_AGENT,
            "params": {
                "transaction_id": txn_id,
                "payment": { "method": "card", "token": "tok" }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(buy["transaction"]["state"], "completed");

    let order = &buy["order"];
    assert!(
        order.is_object(),
        "buy response should carry an `order` object when engine is enabled, got {order}"
    );
    assert!(
        order["order_number"].as_str().unwrap().starts_with("ORD-"),
        "engine-issued order_number should start with ORD-, got {}",
        order["order_number"]
    );
    assert_eq!(order["total"]["amount_minor"].as_i64().unwrap(), 6522);
    assert_eq!(order["total"]["currency"], "USD");
    assert_eq!(order["status"], "created");
    // The id is a real engine UUID, not our `txn_…` opaque string.
    assert!(!order["id"].as_str().unwrap().starts_with("txn_"));
}

#[tokio::test]
async fn track_requires_existing_transaction() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.track",
            "agent_id": DEMO_AGENT,
            "params": { "transaction_id": "txn_does_not_exist" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "resource_not_found");
}

#[tokio::test]
async fn buy_rejects_nonexistent_transaction() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.buy",
            "agent_id": DEMO_AGENT,
            "params": {
                "transaction_id": "txn_missing",
                "payment": { "method": "card", "token": "t" },
            },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "resource_not_found");
}

#[tokio::test]
async fn buy_rejects_after_completion() {
    let app = setup().await;

    let (_s, q) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;
    let txn_id = q["transaction"]["id"].as_str().unwrap().to_string();

    // authorize + buy once
    send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.authorize",
            "agent_id": DEMO_AGENT,
            "params": { "transaction_id": txn_id },
        })),
    )
    .await;
    send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.buy",
            "agent_id": DEMO_AGENT,
            "params": { "transaction_id": txn_id,
                        "payment": { "method": "card", "token": "t" } },
        })),
    )
    .await;

    // second buy should be rejected
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.buy",
            "agent_id": DEMO_AGENT,
            "params": { "transaction_id": txn_id,
                        "payment": { "method": "card", "token": "t" } },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(body["error"]["type"], "precondition_failed");
}

#[tokio::test]
async fn unknown_intent_is_rejected() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents").json_body(json!({
            "intent": "intent.not_a_real_thing",
            "agent_id": DEMO_AGENT,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "intent_not_supported");
}

// --------------------------------------------------------------------------
// Auth
// --------------------------------------------------------------------------

#[tokio::test]
async fn missing_bearer_is_rejected() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .without_header("Authorization")
            .json_body(quote_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_failed");
}

#[tokio::test]
async fn invalid_bearer_is_rejected() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .without_header("Authorization")
            .header("Authorization", "Bearer nonsense")
            .json_body(quote_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_failed");
}

#[tokio::test]
async fn missing_agent_id_is_rejected() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .without_header("ICP-Agent-Id")
            .json_body(quote_body()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_failed");
}

// --------------------------------------------------------------------------
// Mandate enforcement
// --------------------------------------------------------------------------

#[tokio::test]
async fn mandate_required_when_config_demands_it() {
    let mut cfg = Config::for_test();
    cfg.require_mandate = true;
    let app = setup_with(cfg).await;

    let (status, body) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "mandate_invalid");
}

#[tokio::test]
async fn mandate_with_wrong_scope_is_rejected() {
    let mut cfg = Config::for_test();
    cfg.require_mandate = true;
    let app = setup_with(cfg).await;

    // Mandate authorizes only `discover`; we attempt `intent.quote` (scope `quote`).
    let mandate = make_mandate_jws(&["discover"], 100_000, Some(50_000));
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .header("ICP-Mandate", mandate)
            .json_body(quote_body()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "mandate_out_of_scope");
}

#[tokio::test]
async fn mandate_with_tight_per_txn_cap_rejects_big_quote() {
    let mut cfg = Config::for_test();
    cfg.require_mandate = true;
    let app = setup_with(cfg).await;

    // Mandate allows `quote` but caps per-txn at $0.50 (50 minor units);
    // the quoted basket is $59.98.
    let mandate = make_mandate_jws(&["quote"], 1_000, Some(50));
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .header("ICP-Mandate", mandate)
            .json_body(quote_body()),
    )
    .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["type"], "mandate_budget_exceeded");
}

#[tokio::test]
async fn mandate_with_sufficient_budget_accepted() {
    let mut cfg = Config::for_test();
    cfg.require_mandate = true;
    let app = setup_with(cfg).await;

    let mandate = make_mandate_jws(&["quote", "buy"], 100_000, Some(100_000));
    let (status, body) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .header("ICP-Mandate", mandate)
            .json_body(quote_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["transaction"]["state"], "quoted");
}

// --------------------------------------------------------------------------
// Receipts
// --------------------------------------------------------------------------

#[tokio::test]
async fn receipt_body_digest_matches_canonicalized_response() {
    let app = setup().await;

    let (_s, quote) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;

    let receipt_digest = quote["receipt"]["body_digest"]
        .as_str()
        .expect("body_digest")
        .to_string();

    // Reconstruct the body the server hashed: the response body with the
    // receipt stub *cleared* (exactly what the server sees before signing).
    let mut unsigned = quote.clone();
    unsigned["receipt"] = json!({
        "jti": "",
        "kid": "icp-test-key",
        "jws": "",
        "body_digest": "",
    });
    let bytes = serde_jcs::to_vec(&unsigned).expect("jcs");
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(&bytes);
    let computed = format!("sha256:{}", hex::encode(h.finalize()));
    assert_eq!(computed, receipt_digest);
}

#[tokio::test]
async fn receipt_jws_signature_verifies_against_published_jwks() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let app = setup().await;

    // Fetch JWKS.
    let (_s, jwks) = send(&app, req("GET", "/.well-known/icp/jwks.json")).await;
    let x_b64 = jwks["keys"][0]["x"].as_str().unwrap();
    let x_bytes = URL_SAFE_NO_PAD.decode(x_b64).unwrap();
    let pk_bytes: [u8; 32] = x_bytes.as_slice().try_into().unwrap();
    let vk = VerifyingKey::from_bytes(&pk_bytes).unwrap();

    // Produce a receipt.
    let (_s, quote) = send(&app, req("POST", "/icp/v1/intents").json_body(quote_body())).await;
    let jws = quote["receipt"]["jws"].as_str().unwrap().to_string();

    // Compact JWS verification: signing input = header.payload.
    let mut parts = jws.split('.');
    let h = parts.next().unwrap();
    let p = parts.next().unwrap();
    let s = parts.next().unwrap();
    assert!(parts.next().is_none());

    let signing_input = format!("{h}.{p}");
    let sig_bytes = URL_SAFE_NO_PAD.decode(s).unwrap();
    let sig_bytes_fixed: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
    let sig = Signature::from_bytes(&sig_bytes_fixed);
    vk.verify(signing_input.as_bytes(), &sig)
        .expect("receipt signature verifies against JWKS");
}

// --------------------------------------------------------------------------
// Mandate usage ledger
// --------------------------------------------------------------------------

#[tokio::test]
async fn mandate_usage_records_spend_after_buy() {
    let mut cfg = Config::for_test();
    cfg.require_mandate = true;
    let app = setup_with(cfg).await;

    let mandate = make_mandate_jws(&["quote", "buy"], 100_000, Some(100_000));

    // quote
    let (_s, q) = send(
        &app,
        req("POST", "/icp/v1/intents")
            .header("ICP-Mandate", mandate.clone())
            .json_body(quote_body()),
    )
    .await;
    let txn_id = q["transaction"]["id"].as_str().unwrap().to_string();

    // authorize
    send(
        &app,
        req("POST", "/icp/v1/intents")
            .header("ICP-Mandate", mandate.clone())
            .json_body(json!({
                "intent": "intent.authorize",
                "agent_id": DEMO_AGENT,
                "params": { "transaction_id": txn_id },
            })),
    )
    .await;

    // buy
    send(
        &app,
        req("POST", "/icp/v1/intents")
            .header("ICP-Mandate", mandate.clone())
            .json_body(json!({
                "intent": "intent.buy",
                "agent_id": DEMO_AGENT,
                "params": { "transaction_id": txn_id,
                            "payment": { "method": "card", "token": "t" } },
            })),
    )
    .await;

    // Extract jti from the mandate payload.
    let mut parts = mandate.split('.');
    let _ = parts.next();
    let payload_b64 = parts.next().unwrap();
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).unwrap();
    let payload: Value = serde_json::from_slice(&payload_bytes).unwrap();
    let jti = payload["jti"].as_str().unwrap().to_string();

    let (status, usage) = send(&app, req("GET", &format!("/icp/v1/mandates/{jti}/usage"))).await;
    assert_eq!(status, StatusCode::OK);
    // Buy completes with tax added — total > subtotal (5998).
    let spent = usage["spent_minor"].as_i64().unwrap();
    assert!(
        spent > 0,
        "spent_minor should be > 0 after buy, got {spent}"
    );
    assert!(spent >= 5998, "spent should include at least subtotal");
}
