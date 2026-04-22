//! MCP surface integration tests.
//!
//! Drives the handler via JSON-RPC 2.0 at `POST /mcp` and asserts that:
//!   - `initialize` returns capability + server info
//!   - `tools/list` advertises every ICP intent as a tool
//!   - `tools/call` routes through the intent pipeline and produces
//!     signed receipts on state-changing calls
//!   - `ping`, `resources/list`, `prompts/list` return sensible defaults
//!   - disabling MCP (`ICP_MCP_ENABLED=false`) removes the route
//!   - unknown methods return -32601 (method not found)
//!   - invalid JSON-RPC returns -32600

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use stateset_icp_handler::{build_app_state, build_router, config::Config};
use tower::ServiceExt;

const DEMO_KEY: &str = "icp_demo_key_123";

async fn setup(mut mutate: impl FnMut(&mut Config)) -> Router {
    let mut config = Config::for_test();
    mutate(&mut config);
    let state = build_app_state(&config).await.expect("build_app_state");
    build_router(state)
}

async fn rpc(app: &Router, req_body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

// --------------------------------------------------------------------------

#[tokio::test]
async fn mcp_initialize_returns_capabilities() {
    let app = setup(|_| {}).await;
    let (status, body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1.0" }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["jsonrpc"], "2.0");
    // Per JSON-RPC 2.0 §5: response.id MUST mirror request.id.
    assert_eq!(body["id"], 1);
    let result = &body["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(result["serverInfo"]["name"], "stateset-icp-handler");
    assert_eq!(result["serverInfo"]["icp_version"], "2026-04-21");
}

#[tokio::test]
async fn mcp_tools_list_advertises_every_icp_intent() {
    let app = setup(|_| {}).await;
    let (status, body) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tools = body["result"]["tools"].as_array().expect("tools array");
    // MCP tool catalog mirrors discovery: all 17 catalog intents
    // are implemented at the icp-full tier.
    assert_eq!(
        tools.len(),
        17,
        "17 implemented intents advertised as tools (icp-full tier)"
    );

    // Spot-check tools across the catalog.
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"icp_quote"));
    assert!(names.contains(&"icp_buy"));
    assert!(names.contains(&"icp_return"));
    assert!(names.contains(&"icp_subscribe"));
    assert!(names.contains(&"icp_renew"));
    assert!(names.contains(&"icp_a2a_quote"));
    assert!(names.contains(&"icp_a2a_pay"));
    assert!(names.contains(&"icp_negotiate"));
    assert!(names.contains(&"icp_confirm_receipt"));

    // Each tool carries a description and inputSchema.
    for t in tools {
        assert!(t["name"].is_string());
        assert!(t["description"].is_string());
        assert_eq!(t["inputSchema"]["type"], "object");
    }
}

#[tokio::test]
async fn mcp_tools_call_quote_runs_through_pipeline() {
    let app = setup(|_| {}).await;
    let (status, body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "icp_quote",
                "arguments": {
                    "items": [
                        { "sku": "WIDGET-001", "quantity": 2,
                          "unit_price_hint": { "amount_minor": 2999, "currency": "USD" } }
                    ],
                    "buyer": { "email": "alice@example.com" },
                    "currency": "USD"
                }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result = &body["result"];
    assert_eq!(result["isError"], false);
    assert!(result["content"].is_array());
    let summary = result["content"][0]["text"].as_str().unwrap();
    assert!(summary.contains("intent=intent.quote"));
    assert!(summary.contains("state=quoted"));

    // structuredContent carries the full ICP response body including the
    // signed receipt.
    let structured = &result["structuredContent"];
    assert_eq!(structured["intent"], "intent.quote");
    assert_eq!(structured["transaction"]["state"], "quoted");
    assert!(structured["receipt"]["jti"]
        .as_str()
        .unwrap()
        .starts_with("rcpt_"));
}

#[tokio::test]
async fn mcp_full_flow_quote_authorize_buy_through_tool_calls() {
    let app = setup(|_| {}).await;

    // 1. Quote via MCP
    let (_s, quote_body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "icp_quote",
                "arguments": {
                    "items": [{ "sku": "W-1", "quantity": 1,
                                "unit_price_hint": { "amount_minor": 5000, "currency": "USD" } }],
                    "buyer": { "email": "alice@example.com" },
                    "currency": "USD"
                }
            }
        }),
    )
    .await;
    let txn_id = quote_body["result"]["structuredContent"]["transaction"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. Authorize
    let (_s, auth_body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "icp_authorize",
                "arguments": { "transaction_id": txn_id }
            }
        }),
    )
    .await;
    assert_eq!(
        auth_body["result"]["structuredContent"]["transaction"]["state"],
        "authorized"
    );

    // 3. Buy
    let (_s, buy_body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "icp_buy",
                "arguments": {
                    "transaction_id": txn_id,
                    "payment": { "method": "card", "token": "tok_demo" }
                }
            }
        }),
    )
    .await;
    assert_eq!(
        buy_body["result"]["structuredContent"]["transaction"]["state"],
        "completed"
    );
    assert_eq!(buy_body["result"]["isError"], false);
}

#[tokio::test]
async fn mcp_tools_call_reports_errors_with_is_error_true() {
    let app = setup(|_| {}).await;
    let (_s, body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {
                "name": "icp_buy",
                "arguments": {
                    "transaction_id": "txn_does_not_exist",
                    "payment": { "method": "card", "token": "t" }
                }
            }
        }),
    )
    .await;
    // JSON-RPC itself succeeded; MCP tool call signals error via isError.
    assert_eq!(body["result"]["isError"], true);
    assert_eq!(
        body["result"]["structuredContent"]["error"]["type"],
        "resource_not_found"
    );
}

#[tokio::test]
async fn mcp_ping_returns_empty_result() {
    let app = setup(|_| {}).await;
    let (_s, body) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 99, "method": "ping" }),
    )
    .await;
    assert_eq!(body["result"], json!({}));
    assert!(body.get("error").is_none());
}

#[tokio::test]
async fn mcp_unknown_method_returns_method_not_found() {
    let app = setup(|_| {}).await;
    let (_s, body) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 10, "method": "nope/bogus" }),
    )
    .await;
    assert_eq!(body["error"]["code"], -32601);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("nope/bogus"));
}

#[tokio::test]
async fn mcp_unknown_tool_returns_invalid_params() {
    let app = setup(|_| {}).await;
    let (_s, body) = rpc(
        &app,
        json!({
            "jsonrpc": "2.0", "id": 11, "method": "tools/call",
            "params": { "name": "not_an_icp_tool", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(body["error"]["code"], -32602);
}

#[tokio::test]
async fn mcp_invalid_jsonrpc_version_rejected() {
    let app = setup(|_| {}).await;
    let (_s, body) = rpc(
        &app,
        json!({ "jsonrpc": "1.0", "id": 12, "method": "ping" }),
    )
    .await;
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn mcp_tools_call_requires_bearer() {
    let app = setup(|_| {}).await;
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "icp_quote", "arguments": {} }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    // JSON-RPC error code -32001 is an app-defined "unauthorized" per our
    // implementation.
    assert_eq!(body["error"]["code"], -32001);
}

#[tokio::test]
async fn mcp_route_disabled_when_mcp_off() {
    let app = setup(|cfg| cfg.mcp_enabled = false).await;
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Authorization", format!("Bearer {DEMO_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mcp_resources_and_prompts_return_empty_catalogs() {
    let app = setup(|_| {}).await;
    let (_s, res) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
    )
    .await;
    assert_eq!(res["result"]["resources"], json!([]));

    let (_s, pr) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "prompts/list" }),
    )
    .await;
    assert_eq!(pr["result"]["prompts"], json!([]));
}
