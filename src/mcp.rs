//! MCP (Model Context Protocol) surface.
//!
//! Exposes every ICP intent as a discoverable [MCP][mcp] tool over an
//! HTTP JSON-RPC 2.0 endpoint at `POST /mcp`. Any MCP client (Claude
//! Desktop, Cursor, custom agents, …) pointed at this URL can:
//!
//!   1. Call `initialize` to negotiate capability.
//!   2. Call `tools/list` to discover the catalog.
//!   3. Call `tools/call` with `{ name, arguments }` to execute an
//!      intent; the tool call result wraps the full ICP response body,
//!      signed receipt included.
//!
//! # Transport
//!
//! This is the **streamable HTTP** MCP transport: a single POST that
//! returns the JSON-RPC response directly (no SSE). stdio transport is
//! deferred to a future release; the tool catalog is identical so an
//! stdio shim would be a thin adapter around the same dispatcher.
//!
//! # Auth
//!
//! The same tenant bearer key used for ICP is required. An MCP client
//! sends `Authorization: Bearer <key>` on the POST. We treat the
//! bearer as the self-mandate (same model as ACP/UCP compat), synthesize
//! `did:stateset:agent:mcp-<tenant>` for accounting, and route every
//! tool call through `IcpService::handle_intent`.
//!
//! [mcp]: https://modelcontextprotocol.io

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::agent::AgentIdentifier;
use crate::intent::Intent;
use crate::models::{IntentContext, IntentEnvelope};
use crate::service::IntentInput;
use crate::AppState;

/// Protocol version we implement. Clients negotiate down from their
/// advertised version during `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// --------------------------------------------------------------------------
// JSON-RPC 2.0
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// --------------------------------------------------------------------------
// Tool catalog
// --------------------------------------------------------------------------

/// Build the list of tools advertised in `tools/list`. One tool per
/// implemented ICP intent; name is `icp_<intent>` so it namespaces
/// cleanly in MCP clients that flatten tool catalogs across servers.
///
/// Tools for unimplemented intents are intentionally omitted — MCP
/// clients surface tool catalogs to LLMs, and advertising tools that
/// only return errors poisons the agent's planning.
pub fn tool_catalog() -> Vec<Value> {
    Intent::CORE
        .iter()
        .filter(|intent| intent.is_implemented())
        .map(|intent| tool_for_intent(*intent))
        .collect()
}

fn tool_for_intent(intent: Intent) -> Value {
    let name = format!("icp_{}", intent.wire_name().trim_start_matches("intent."));
    json!({
        "name": name,
        "description": description_for(intent),
        "inputSchema": input_schema_for(intent),
    })
}

fn description_for(intent: Intent) -> &'static str {
    match intent {
        Intent::Search => "Search products in the merchant's catalog. Read-only; does not require a mandate.",
        Intent::Describe => "Fetch a product's full description by SKU or product id. Read-only.",
        Intent::Quote => "Price a basket: returns a `quoted` transaction with totals (subtotal + tax + total). Scope: `quote`.",
        Intent::Negotiate => "Counter-offer pricing on an existing quote. Scope: `quote`.",
        Intent::Authorize => "Authorize an existing quoted transaction for payment. Scope: `buy`.",
        Intent::Buy => "Complete purchase of an authorized transaction. Requires `payment`. Scope: `buy`.",
        Intent::Pay => "Alias for intent.buy.",
        Intent::Subscribe => "Start a recurring subscription against the transaction. Scope: `subscribe`.",
        Intent::Renew => "Renew an existing subscription. Scope: `subscribe`.",
        Intent::Pause => "Pause an active subscription. Scope: `subscribe`.",
        Intent::CancelSubscription => "Cancel a subscription. Scope: `subscribe`.",
        Intent::Track => "Fetch current transaction/order status (shipping, fulfillment). Read-only.",
        Intent::ConfirmReceipt => "Acknowledge physical receipt of goods (triggers escrow release for A2A/stablecoin).",
        Intent::Return => "Initiate a return against a completed transaction. Scope: `return`.",
        Intent::RefundRequest => "Request a refund against a completed order. Scope: `return`.",
        Intent::A2aPay => "Pay another agent directly (peer commerce). Scope: `pay_peer`.",
        Intent::A2aQuote => "Ask another agent to quote a service. Scope: `pay_peer`.",
    }
}

fn input_schema_for(intent: Intent) -> Value {
    match intent {
        Intent::Search => json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Free-text search query" },
                "filters": { "type": "object", "description": "Structured filters (brand, category, price_min, …)" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                "cursor": { "type": "string" }
            }
        }),
        Intent::Describe => json!({
            "type": "object",
            "properties": {
                "product_id": { "type": "string" },
                "sku": { "type": "string" }
            }
        }),
        Intent::Quote => json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["sku", "quantity"],
                        "properties": {
                            "sku": { "type": "string" },
                            "quantity": { "type": "integer", "minimum": 1 },
                            "unit_price_hint": {
                                "type": "object",
                                "properties": {
                                    "amount_minor": { "type": "integer" },
                                    "currency": { "type": "string" }
                                }
                            }
                        }
                    }
                },
                "buyer": { "type": "object" },
                "ship_to": { "type": "object" },
                "currency": { "type": "string", "default": "USD" },
                "jurisdiction": { "type": "string" }
            }
        }),
        Intent::Authorize => json!({
            "type": "object",
            "required": ["transaction_id"],
            "properties": {
                "transaction_id": { "type": "string" },
                "buyer": { "type": "object" },
                "ship_to": { "type": "object" },
                "bill_to": { "type": "object" }
            }
        }),
        Intent::Buy | Intent::Pay => json!({
            "type": "object",
            "required": ["transaction_id", "payment"],
            "properties": {
                "transaction_id": { "type": "string" },
                "payment": {
                    "type": "object",
                    "required": ["method"],
                    "properties": {
                        "method": {
                            "type": "string",
                            "enum": ["card", "delegated_vault", "stablecoin", "a2a"]
                        },
                        "token": { "type": "string" },
                        "last_digits": { "type": "string" },
                        "brand": { "type": "string" },
                        "provider": { "type": "string" },
                        "asset": { "type": "string" },
                        "chain": { "type": "string" },
                        "from": { "type": "string" },
                        "peer_agent_id": { "type": "string" }
                    }
                }
            }
        }),
        Intent::Track => json!({
            "type": "object",
            "properties": {
                "transaction_id": { "type": "string" },
                "order_id": { "type": "string" }
            }
        }),
        Intent::Return | Intent::RefundRequest => json!({
            "type": "object",
            "required": ["transaction_id"],
            "properties": {
                "transaction_id": { "type": "string" },
                "order_id": { "type": "string" },
                "line_item_ids": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "reason": { "type": "string" }
            }
        }),
        _ => json!({
            "type": "object",
            "description": "Accepts any ICP intent params for this intent; see docs/specification/ICP_SPEC.md"
        }),
    }
}

// --------------------------------------------------------------------------
// Transport-agnostic dispatcher
// --------------------------------------------------------------------------

/// Route a single JSON-RPC request to the matching MCP handler.
///
/// Both the HTTP wrapper ([`handle`]) and the stdio binary
/// (`bin/icp_mcp_stdio`) call this. `headers` carries auth (the bearer
/// token specifically); over stdio it's synthesized from CLI args so
/// the inner code stays uniform.
pub async fn dispatch(
    state: &AppState,
    headers: &HeaderMap,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    let id = req.id.clone();

    if req.jsonrpc != "2.0" {
        return JsonRpcResponse::err(id, -32600, "jsonrpc must be \"2.0\"");
    }

    match req.method.as_str() {
        "initialize" => handle_initialize(id, req.params),
        "initialized" | "notifications/initialized" => {
            // Notification — no response per JSON-RPC spec; we return an
            // empty result so transports that *do* expect a reply (HTTP)
            // get one.
            JsonRpcResponse::ok(id, json!({}))
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        "tools/list" => JsonRpcResponse::ok(id, json!({ "tools": tool_catalog() })),
        "tools/call" => match handle_tools_call(state, headers, req.params).await {
            Ok(result) => JsonRpcResponse::ok(id, result),
            Err((code, message)) => JsonRpcResponse::err(id, code, message),
        },
        "resources/list" => JsonRpcResponse::ok(id, json!({ "resources": [] })),
        "prompts/list" => JsonRpcResponse::ok(id, json!({ "prompts": [] })),
        other => JsonRpcResponse::err(id, -32601, format!("method not found: {other}")),
    }
}

// --------------------------------------------------------------------------
// HTTP handler
// --------------------------------------------------------------------------

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let body = dispatch(&state, &headers, req).await;
    (StatusCode::OK, Json(body))
}

fn handle_initialize(id: Option<Value>, _params: Option<Value>) -> JsonRpcResponse {
    let result = json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "listChanged": false, "subscribe": false },
            "prompts": { "listChanged": false },
            "logging": {},
        },
        "serverInfo": {
            "name": "stateset-icp-handler",
            "version": env!("CARGO_PKG_VERSION"),
            "icp_version": crate::constants::ICP_VERSION,
        },
        "instructions": "Call tools/list to discover every ICP intent as a tool. Each tool accepts the intent's param schema. Results wrap the full ICP response body; state-changing tools include a signed receipt."
    });
    JsonRpcResponse::ok(id, result)
}

async fn handle_tools_call(
    state: &AppState,
    headers: &HeaderMap,
    params: Option<Value>,
) -> Result<Value, (i32, String)> {
    let params = params.ok_or((-32602, "tools/call: params required".into()))?;
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "tools/call: params.name required".into()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Parse the tool name back into an intent.
    let intent_name = name
        .strip_prefix("icp_")
        .ok_or((-32602, format!("unknown tool: {name}")))?;
    let intent = Intent::parse(&format!("intent.{intent_name}"))
        .map_err(|e| (-32602, format!("unknown tool: {e}")))?;

    // Resolve tenant. Allow the client to pass the API key as a header
    // on the HTTP request; fail fast if not present.
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((-32001, "Bearer token required for MCP tool call".into()))?;
    let tenant = state
        .keys
        .lookup(bearer)
        .ok_or((-32001, "unknown API key".into()))?;
    if tenant.is_expired_at(chrono::Utc::now()) {
        return Err((-32001, "API key expired".into()));
    }

    let agent_raw = format!("did:stateset:agent:mcp-{}", tenant.tenant_id);
    let agent = AgentIdentifier::parse(&agent_raw);
    if !tenant.permits_agent(&agent.raw) {
        return Err((
            -32001,
            format!("agent `{}` is not allowed for this API key", agent.raw),
        ));
    }

    // Extract transaction_id + context from arguments (so MCP clients
    // don't have to hand-build the envelope).
    let transaction_id = arguments
        .get("transaction_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut context = IntentContext::default();
    if let Some(currency) = arguments.get("currency").and_then(|v| v.as_str()) {
        context.currency = Some(currency.to_string());
    }
    if let Some(jurisdiction) = arguments.get("jurisdiction").and_then(|v| v.as_str()) {
        context.jurisdiction = Some(jurisdiction.to_string());
    }

    let envelope = IntentEnvelope {
        intent: intent.wire_name().to_string(),
        intent_id: None,
        transaction_id,
        agent_id: agent.raw.clone(),
        mandate_jti: None,
        params: arguments,
        context,
    };

    let input = IntentInput::for_compat(
        envelope,
        agent,
        tenant,
        format!("req_{}", Uuid::new_v4().simple()),
        None,
    );

    match state.service.handle_intent(input).await {
        Ok(body) => {
            let structured = serde_json::to_value(&body).unwrap_or(Value::Null);
            // MCP `tools/call` result: content is an array of content
            // blocks. We return both a text summary and the structured
            // JSON so clients can pick either.
            Ok(json!({
                "content": [
                    { "type": "text", "text": format!(
                        "intent={} transaction={} state={} receipt_jti={}",
                        body.intent,
                        body.transaction.id,
                        transaction_state_str(&body.transaction),
                        body.receipt.jti,
                    ) }
                ],
                "isError": false,
                "structuredContent": structured,
            }))
        }
        Err(err) => {
            let (_status, api_body) = err.into_body();
            Ok(json!({
                "content": [
                    { "type": "text", "text": api_body.error.message.clone() }
                ],
                "isError": true,
                "structuredContent": serde_json::to_value(&api_body).unwrap_or(Value::Null),
            }))
        }
    }
}

fn transaction_state_str(txn: &crate::models::Transaction) -> &'static str {
    use crate::models::TransactionState::*;
    match txn.state {
        Draft => "draft",
        Quoted => "quoted",
        Authorized => "authorized",
        Captured => "captured",
        Fulfilled => "fulfilled",
        Completed => "completed",
        Reversed => "reversed",
        Canceled => "canceled",
        Expired => "expired",
    }
}
