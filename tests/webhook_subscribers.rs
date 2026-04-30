//! Per-tenant webhook subscriber integration tests.
//!
//! Asserts the multi-tenancy properties:
//!   * Tenants can register their own webhook URLs via the admin API.
//!   * Events fan out to the originating tenant's active subscribers.
//!   * Tenants are isolated — tenant A never sees tenant B's
//!     subscribers and never receives tenant B's events.
//!   * Disabling a subscriber stops it from receiving future events
//!     without dropping the row.
//!   * The global `webhook_url` is a *fallback* — used only when a
//!     tenant has no registered subscribers.
//!   * Validation: empty URL / non-http URL / empty secret rejected.
//!   * Cross-tenant access is treated as 404 (existence not leaked).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{
    agent::ApiKeyInfo, build_app_state, build_router, config::Config, AppState,
};
use tower::ServiceExt;

const AGENT: &str = "did:stateset:agent:wsub-test";

async fn build(global_fallback: Option<(&str, &str)>, keys: Vec<ApiKeyInfo>) -> (AppState, Router) {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    if let Some((url, secret)) = global_fallback {
        cfg.webhook_url = Some(url.to_string());
        cfg.webhook_secret = Some(secret.to_string());
    }
    if !keys.is_empty() {
        cfg.api_keys_json = Some(serde_json::to_string(&keys).unwrap());
    }
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

async fn create_sub(app: &Router, bearer: &str, url: &str, secret: &str) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/icp/v1/webhook_subscribers",
        bearer,
        Some(json!({ "url": url, "secret": secret })),
    )
    .await
}

async fn quote(app: &Router, bearer: &str) -> Value {
    send(
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
    .await
    .1
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn create_then_list_then_get() {
    let (_state, app) = build(None, vec![key("a", "tenant_a")]).await;

    // Empty list initially.
    let (status, body) = send(&app, "GET", "/icp/v1/webhook_subscribers", "k_a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], 0);

    // Create.
    let (status, created) = create_sub(&app, "k_a", "https://hooks.example/a", "secret-a").await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("whsub_"));
    assert_eq!(created["url"], "https://hooks.example/a");
    assert_eq!(created["tenant_id"], "tenant_a");
    assert_eq!(created["active"], true);
    // Secret is returned ONCE on creation so the caller can store it
    // (e.g. show it in their dashboard).
    assert_eq!(created["secret"], "secret-a");

    // List shows it (with secret redacted on read).
    let (_, body) = send(&app, "GET", "/icp/v1/webhook_subscribers", "k_a", None).await;
    assert_eq!(body["count"], 1);
    assert_eq!(body["data"][0]["id"], id);
    assert!(
        body["data"][0]["secret"].is_null() || body["data"][0].get("secret").is_none(),
        "list responses must redact secrets"
    );

    // Read by id.
    let (status, fetched) = send(
        &app,
        "GET",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], id);
    assert!(
        fetched.get("secret").is_none() || fetched["secret"].is_null(),
        "GET also redacts the secret"
    );
}

#[tokio::test]
async fn events_fan_out_to_a_tenants_active_subscribers() {
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    create_sub(&app, "k_a", "https://hooks.example/one", "s1").await;
    create_sub(&app, "k_a", "https://hooks.example/two", "s2").await;

    // Drive a state-changing intent for tenant_a.
    let _ = quote(&app, "k_a").await;

    // Outbox should now have 2 deliveries (one per active subscriber).
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(
        recent.len(),
        2,
        "fan-out must produce one delivery per subscriber"
    );
    let urls: Vec<&str> = recent.iter().map(|d| d.url.as_str()).collect();
    assert!(urls.contains(&"https://hooks.example/one"));
    assert!(urls.contains(&"https://hooks.example/two"));
}

#[tokio::test]
async fn tenants_are_isolated_from_each_others_subscribers() {
    let (_state, app) = build(None, vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;
    let (_, sub_a) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let (_, sub_b) = create_sub(&app, "k_b", "https://b.example/hook", "sb").await;
    let id_a = sub_a["id"].as_str().unwrap().to_string();
    let id_b = sub_b["id"].as_str().unwrap().to_string();

    // Tenant A's list shows only A's subscribers.
    let (_, list_a) = send(&app, "GET", "/icp/v1/webhook_subscribers", "k_a", None).await;
    assert_eq!(list_a["count"], 1);
    assert_eq!(list_a["data"][0]["id"], id_a);

    // Tenant A trying to GET tenant B's subscriber → 404 (existence
    // not leaked across tenants).
    let (status, _) = send(
        &app,
        "GET",
        &format!("/icp/v1/webhook_subscribers/{id_b}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant read must look like a miss, not a 403"
    );

    // Tenant A trying to disable tenant B's subscriber → 404 (existence
    // not leaked even on writes).
    let (status, _) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_subscribers/{id_b}/disable"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Tenant A trying to delete tenant B's subscriber → 404.
    let (status, _) = send(
        &app,
        "DELETE",
        &format!("/icp/v1/webhook_subscribers/{id_b}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn events_only_fan_out_to_originating_tenants_subscribers() {
    let (state, app) = build(None, vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;
    create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    create_sub(&app, "k_b", "https://b.example/hook", "sb").await;

    // Tenant A's intent → only A's subscriber should receive.
    let _ = quote(&app, "k_a").await;
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].url, "https://a.example/hook");

    // Tenant B's intent → only B's subscriber.
    let _ = quote(&app, "k_b").await;
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 2);
    let urls: Vec<&str> = recent.iter().map(|d| d.url.as_str()).collect();
    assert!(urls.contains(&"https://a.example/hook"));
    assert!(urls.contains(&"https://b.example/hook"));
}

#[tokio::test]
async fn disabling_a_subscriber_stops_future_events() {
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let id = created["id"].as_str().unwrap().to_string();

    // First quote — delivery enqueued.
    let _ = quote(&app, "k_a").await;
    assert_eq!(state.service.webhook_outbox.list_recent(10).len(), 1);

    // Disable.
    let (status, body) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_subscribers/{id}/disable"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);

    // Second quote — no new delivery.
    let _ = quote(&app, "k_a").await;
    assert_eq!(
        state.service.webhook_outbox.list_recent(10).len(),
        1,
        "disabled subscriber must not receive new events"
    );

    // Row still in the list.
    let (_, list) = send(&app, "GET", "/icp/v1/webhook_subscribers", "k_a", None).await;
    assert_eq!(list["count"], 1);
    assert_eq!(list["data"][0]["active"], false);
}

#[tokio::test]
async fn enable_re_activates_a_disabled_subscriber() {
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let id = created["id"].as_str().unwrap().to_string();

    // Disable, confirm no new events flow.
    let (_, _) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_subscribers/{id}/disable"),
        "k_a",
        None,
    )
    .await;
    let _ = quote(&app, "k_a").await;
    assert_eq!(
        state.service.webhook_outbox.list_recent(10).len(),
        0,
        "no fan-out while disabled"
    );

    // Re-enable. The same id stays — critical: a delete+recreate
    // would have rotated the id and the secret, breaking any
    // verifier configuration the operator's downstream system
    // already has cached.
    let (status, body) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_subscribers/{id}/enable"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id);
    assert_eq!(body["active"], true);

    // Next quote DOES fan out.
    let _ = quote(&app, "k_a").await;
    assert_eq!(
        state.service.webhook_outbox.list_recent(10).len(),
        1,
        "events resume after enable"
    );
}

#[tokio::test]
async fn enable_is_idempotent_on_already_active_subscriber() {
    // No state-machine churn for an operator who calls enable
    // twice in quick succession (e.g. an automation script).
    let (_state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["active"], true, "fresh subscriber starts active");

    let (status, body) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_subscribers/{id}/enable"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enable on active is a no-op 200");
    assert_eq!(body["active"], true);
    assert_eq!(body["id"], id);
}

#[tokio::test]
async fn patch_rotates_secret_in_place_without_changing_id() {
    // The whole point of this endpoint: rotating the HMAC secret
    // (security best practice) without forcing a delete + recreate
    // that would rotate the id and orphan the downstream verifier.
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "old-secret").await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        Some(json!({ "secret": "new-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id, "id MUST NOT change on patch");
    assert!(
        body["secret"].is_null() || body.get("secret").is_none(),
        "patch responses redact secret — it's in the request, not the response"
    );

    // The new secret is the one used for HMAC signing on next
    // delivery. Easiest check: the store has it.
    let stored = state
        .service
        .webhook_subscribers
        .get(&id)
        .expect("row still exists");
    assert_eq!(stored.secret.as_deref(), Some("new-secret"));
    assert_eq!(stored.url, "https://a.example/hook", "url unchanged");
}

#[tokio::test]
async fn patch_can_update_url_in_place() {
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://old.example/hook", "secret").await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        Some(json!({ "url": "https://new.example/hook" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["url"], "https://new.example/hook");

    // Secret unchanged — the omitted field stays as-is.
    let stored = state.service.webhook_subscribers.get(&id).unwrap();
    assert_eq!(stored.secret.as_deref(), Some("secret"));
}

#[tokio::test]
async fn patch_with_empty_body_is_a_noop_200() {
    // Property: an operator can PATCH with `{}` and not corrupt
    // anything. Both fields default to None → the helper passes
    // through with no field updates, just a touched updated_at.
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "secret").await;
    let id = created["id"].as_str().unwrap().to_string();
    let original_updated_at = created["updated_at"].as_str().unwrap().to_string();

    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stored = state.service.webhook_subscribers.get(&id).unwrap();
    assert_eq!(stored.url, "https://a.example/hook");
    assert_eq!(stored.secret.as_deref(), Some("secret"));
    assert_ne!(
        stored.updated_at.to_rfc3339(),
        original_updated_at,
        "even a no-op patch refreshes updated_at — that's the audit signal"
    );
}

#[tokio::test]
async fn patch_validates_url_and_secret() {
    let (_state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "secret").await;
    let id = created["id"].as_str().unwrap().to_string();

    // Empty secret → 400. Sending an empty string is a different
    // intent from omitting (which leaves it alone), and an empty
    // secret would silently break HMAC verification.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        Some(json!({ "secret": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Non-http(s) URL → 400.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        Some(json!({ "url": "ftp://nope.example" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Empty URL string → 400.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        Some(json!({ "url": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patch_on_cross_tenant_subscriber_returns_404() {
    let (_state, app) = build(None, vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;
    let (_, sub_a) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let id_a = sub_a["id"].as_str().unwrap().to_string();

    // Tenant B tries to rotate tenant A's secret → 404, not 403.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/icp/v1/webhook_subscribers/{id_a}"),
        "k_b",
        Some(json!({ "secret": "evil" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant patch must look identical to a missing row"
    );
}

#[tokio::test]
async fn enable_on_cross_tenant_subscriber_returns_404() {
    let (_state, app) = build(None, vec![key("a", "tenant_a"), key("b", "tenant_b")]).await;
    let (_, sub_a) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let id_a = sub_a["id"].as_str().unwrap().to_string();

    // Tenant B tries to enable tenant A's subscriber → 404, not 403.
    // Same isolation property as get/disable/delete — existence is
    // never confirmed across tenants.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/icp/v1/webhook_subscribers/{id_a}/enable"),
        "k_b",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cross-tenant enable must look identical to a missing row"
    );
}

#[tokio::test]
async fn delete_removes_the_row_entirely() {
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let (_, created) = create_sub(&app, "k_a", "https://a.example/hook", "sa").await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &app,
        "DELETE",
        &format!("/icp/v1/webhook_subscribers/{id}"),
        "k_a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], true);

    let (_, list) = send(&app, "GET", "/icp/v1/webhook_subscribers", "k_a", None).await;
    assert_eq!(list["count"], 0);
    assert_eq!(state.service.webhook_subscribers.len(), 0);
}

#[tokio::test]
async fn global_fallback_used_when_tenant_has_no_subscribers() {
    let (state, app) = build(
        Some(("https://global.example/hook", "global-secret")),
        vec![key("a", "tenant_a"), key("b", "tenant_b")],
    )
    .await;
    // Tenant A registers a subscriber; tenant B doesn't.
    create_sub(&app, "k_a", "https://a.example/hook", "sa").await;

    // A's event → fans out to A's subscriber (NOT the global fallback).
    let _ = quote(&app, "k_a").await;
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].url, "https://a.example/hook");

    // B's event → no per-tenant subscribers, so the global fallback
    // fires.
    let _ = quote(&app, "k_b").await;
    let recent = state.service.webhook_outbox.list_recent(10);
    assert_eq!(recent.len(), 2);
    let last = recent
        .iter()
        .find(|d| d.url == "https://global.example/hook");
    assert!(
        last.is_some(),
        "tenant B with no subscribers must fall through to the global URL"
    );
}

#[tokio::test]
async fn no_global_no_subscribers_means_no_outbox_writes() {
    let (state, app) = build(None, vec![key("a", "tenant_a")]).await;
    let _ = quote(&app, "k_a").await;
    assert_eq!(
        state.service.webhook_outbox.len(),
        0,
        "no destinations should mean no enqueues at all"
    );
}

#[tokio::test]
async fn validation_rejects_bad_inputs() {
    let (_, app) = build(None, vec![key("a", "tenant_a")]).await;

    // Empty URL.
    let (status, body) = create_sub(&app, "k_a", "", "secret").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request");

    // Non-http(s) URL.
    let (status, _) = create_sub(&app, "k_a", "ftp://nope.example", "secret").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Empty secret.
    let (status, body) = create_sub(&app, "k_a", "https://x.example/h", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("secret"));
}

#[tokio::test]
async fn validation_blocks_private_webhook_hosts_when_insecure_urls_are_disabled() {
    let mut cfg = Config::for_test();
    cfg.enable_demo_keys = false;
    cfg.allow_insecure_urls = false;
    cfg.public_base_url = "https://icp.example".into();
    cfg.api_keys_json = Some(serde_json::to_string(&vec![key("a", "tenant_a")]).unwrap());
    let state = build_app_state(&cfg).await.expect("state");
    let app = build_router(state);

    let (status, body) = create_sub(&app, "k_a", "https://127.0.0.1/hook", "secret").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "invalid_request");

    let (status, _) = create_sub(&app, "k_a", "https://localhost/hook", "secret").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unauthenticated_create_is_rejected() {
    let (_, app) = build(None, vec![key("a", "tenant_a")]).await;

    // No bearer at all.
    let req = Request::builder()
        .method("POST")
        .uri("/icp/v1/webhook_subscribers")
        .header("ICP-Agent-Id", AGENT)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"url": "https://x.example/h", "secret": "s"}).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Bad bearer.
    let (status, _) = create_sub(&app, "k_nonexistent", "https://x.example/h", "s").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
