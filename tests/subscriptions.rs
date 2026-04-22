//! Subscription lifecycle integration tests.
//!
//! Covers the four subscription intents (`subscribe`, `renew`, `pause`,
//! `cancel_subscription`) end-to-end through the real router:
//!
//!   - Initial subscribe creates a `Subscription` aggregate AND a
//!     completed charge transaction in the same response.
//!   - `renew` advances the period and creates a new charge.
//!   - `pause` and `cancel_subscription` flip status and emit a signed
//!     receipt over a synthesized pseudo-transaction.
//!   - Subscription is retrievable via `GET /icp/v1/subscriptions/:id`.
//!   - Discovery and MCP `tools/list` now advertise all four intents.
//!   - Mandate scope `subscribe` gates the operations as expected.

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
const DEMO_AGENT: &str = "did:stateset:agent:sub-test";

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
        .header("ICP-Agent-Id", DEMO_AGENT);
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

fn subscribe_body(cadence: &str) -> Value {
    json!({
        "intent": "intent.subscribe",
        "agent_id": DEMO_AGENT,
        "params": {
            "items": [{
                "sku": "PLAN-PRO",
                "quantity": 1,
                "unit_price_hint": { "amount_minor": 4999, "currency": "USD" }
            }],
            "buyer": { "first_name": "Alice", "email": "alice@example.com" },
            "cadence": cadence,
            "payment": { "method": "card", "token": "tok_sub" }
        },
        "context": { "currency": "USD" }
    })
}

fn alg_none_mandate(scopes: &[&str]) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let now = Utc::now().timestamp();
    let payload = json!({
        "iss": "did:buyer:test",
        "sub": DEMO_AGENT,
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
async fn subscribe_creates_subscription_and_charges() {
    let app = setup().await;
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("monthly")),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    // Subscription block present.
    let sub = &body["subscription"];
    assert!(sub.is_object(), "subscription field missing");
    assert!(sub["id"].as_str().unwrap().starts_with("sub_"));
    assert_eq!(sub["status"], "active");
    assert_eq!(sub["cadence"], "monthly");
    assert_eq!(sub["currency"], "USD");
    assert_eq!(sub["charges_completed"], 1);
    assert_eq!(sub["agent_id"], DEMO_AGENT);

    // Period dates set; next charge equals current_period_end.
    assert_eq!(sub["next_charge_at"], sub["current_period_end"]);
    assert!(sub["current_period_start"].is_string());

    // Charge transaction also returned, in completed state with the
    // priced totals.
    let txn = &body["transaction"];
    assert_eq!(txn["state"], "completed");
    assert_eq!(txn["totals"]["subtotal"]["amount_minor"], 4999);
    // 8.75% tax on 4999 = 437.4 → 437 (integer truncation).
    assert_eq!(txn["totals"]["tax"]["amount_minor"], 437);
    assert_eq!(txn["totals"]["total"]["amount_minor"], 5436);

    // last_transaction_id on sub points at the charge txn.
    assert_eq!(sub["last_transaction_id"], txn["id"]);

    // Receipt signed.
    assert!(body["receipt"]["jti"]
        .as_str()
        .unwrap()
        .starts_with("rcpt_"));
}

#[tokio::test]
async fn renew_advances_period_and_creates_new_charge() {
    let app = setup().await;

    let (_s, sub_resp) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("weekly")),
        &[],
    )
    .await;
    let sub_id = sub_resp["subscription"]["id"].as_str().unwrap().to_string();
    let initial_period_end = sub_resp["subscription"]["current_period_end"]
        .as_str()
        .unwrap()
        .to_string();
    let initial_txn_id = sub_resp["transaction"]["id"].as_str().unwrap().to_string();

    let (status, renew) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.renew",
            "agent_id": DEMO_AGENT,
            "params": {
                "subscription_id": sub_id,
                "payment": { "method": "card", "token": "tok_renew" }
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={renew}");

    let sub_after = &renew["subscription"];
    assert_eq!(sub_after["id"], sub_id);
    assert_eq!(sub_after["status"], "active");
    assert_eq!(sub_after["charges_completed"], 2);

    // New period starts at previous period end (no drift).
    assert_eq!(
        sub_after["current_period_start"], initial_period_end,
        "renew must anchor new period on previous period end"
    );

    // New charge transaction is distinct from the initial one.
    let new_txn_id = renew["transaction"]["id"].as_str().unwrap().to_string();
    assert_ne!(new_txn_id, initial_txn_id);
    assert_eq!(sub_after["last_transaction_id"], new_txn_id);
    assert_eq!(renew["transaction"]["state"], "completed");
}

#[tokio::test]
async fn pause_then_renew_is_rejected() {
    let app = setup().await;
    let (_s, sub_resp) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("monthly")),
        &[],
    )
    .await;
    let sub_id = sub_resp["subscription"]["id"].as_str().unwrap().to_string();

    // Pause.
    let (status, paused) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.pause",
            "agent_id": DEMO_AGENT,
            "params": { "subscription_id": sub_id }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(paused["subscription"]["status"], "paused");
    assert!(paused["subscription"]["paused_at"].is_string());
    // Pause emits a signed receipt over the synthesized pseudo-txn.
    assert!(paused["receipt"]["jti"]
        .as_str()
        .unwrap()
        .starts_with("rcpt_"));
    assert_eq!(
        paused["transaction"]["external_refs"]["subscription_id"],
        sub_id
    );

    // Renew during paused state — must be rejected.
    let (status, err) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.renew",
            "agent_id": DEMO_AGENT,
            "params": {
                "subscription_id": sub_id,
                "payment": { "method": "card", "token": "tok" }
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(err["error"]["type"], "precondition_failed");
    assert!(err["error"]["message"].as_str().unwrap().contains("paused"));
}

#[tokio::test]
async fn cancel_subscription_is_terminal() {
    let app = setup().await;
    let (_s, sub_resp) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("annual")),
        &[],
    )
    .await;
    let sub_id = sub_resp["subscription"]["id"].as_str().unwrap().to_string();

    let (status, canceled) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.cancel_subscription",
            "agent_id": DEMO_AGENT,
            "params": { "subscription_id": sub_id }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canceled["subscription"]["status"], "canceled");
    assert!(canceled["subscription"]["canceled_at"].is_string());

    // Idempotent? No — re-cancel returns precondition_failed.
    let (status, second) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.cancel_subscription",
            "agent_id": DEMO_AGENT,
            "params": { "subscription_id": sub_id }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(second["error"]["type"], "precondition_failed");

    // Renew on a canceled sub must fail.
    let (status, _) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(json!({
            "intent": "intent.renew",
            "agent_id": DEMO_AGENT,
            "params": {
                "subscription_id": sub_id,
                "payment": { "method": "card", "token": "tok" }
            }
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn subscription_retrievable_by_get() {
    let app = setup().await;
    let (_s, sub_resp) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("monthly")),
        &[],
    )
    .await;
    let sub_id = sub_resp["subscription"]["id"].as_str().unwrap().to_string();

    let (status, fetched) = send(
        &app,
        "GET",
        &format!("/icp/v1/subscriptions/{sub_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], sub_id);
    assert_eq!(fetched["status"], "active");
    assert_eq!(fetched["cadence"], "monthly");
}

#[tokio::test]
async fn subscribe_unknown_subscription_returns_404() {
    let app = setup().await;
    for intent in ["intent.renew", "intent.pause", "intent.cancel_subscription"] {
        let body = json!({
            "intent": intent,
            "agent_id": DEMO_AGENT,
            "params": {
                "subscription_id": "sub_does_not_exist",
                "payment": { "method": "card", "token": "tok" }
            }
        });
        let (status, _) = send(&app, "POST", "/icp/v1/intents", Some(body), &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "intent {intent}");
    }
}

#[tokio::test]
async fn subscribe_requires_subscribe_scope_in_mandate() {
    let app = setup_with(|c| c.require_mandate = true).await;

    // Mandate that only authorizes `quote` — subscribe must be rejected.
    let m = alg_none_mandate(&["quote"]);
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("monthly")),
        &[("ICP-Mandate", &m)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "mandate_out_of_scope");
}

#[tokio::test]
async fn subscribe_with_subscribe_scope_accepted() {
    let app = setup_with(|c| c.require_mandate = true).await;

    let m = alg_none_mandate(&["subscribe"]);
    let (status, body) = send(
        &app,
        "POST",
        "/icp/v1/intents",
        Some(subscribe_body("monthly")),
        &[("ICP-Mandate", &m)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["subscription"]["status"], "active");
}

#[tokio::test]
async fn discovery_advertises_all_four_subscription_intents() {
    let app = setup().await;
    let (_s, body) = send(&app, "GET", "/.well-known/icp", None, &[]).await;
    let intents = body["intents"].as_array().unwrap();
    // 17 implemented intents (9 base + 4 subscription + 2 A2A + 2 icp-full).
    assert_eq!(intents.len(), 17, "17 implemented intents advertised");
    for needed in [
        "intent.subscribe",
        "intent.renew",
        "intent.pause",
        "intent.cancel_subscription",
    ] {
        assert!(
            intents.iter().any(|v| v == needed),
            "discovery missing {needed}"
        );
    }
}

#[tokio::test]
async fn mcp_tools_list_includes_subscription_tools() {
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
    assert!(names.contains(&"icp_subscribe"));
    assert!(names.contains(&"icp_renew"));
    assert!(names.contains(&"icp_pause"));
    assert!(names.contains(&"icp_cancel_subscription"));
}
