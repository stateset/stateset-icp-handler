//! ACP (Agentic Commerce Protocol) compatibility surface.
//!
//! Implements a subset of [OpenAI ACP `2025-09-29`][acp] sufficient for
//! an ACP-native agent (ChatGPT Instant Checkout and similar) to complete
//! a full checkout on an ICP handler without modification.
//!
//! # Endpoint mapping
//!
//! | ACP path                                              | ICP intent                |
//! |-------------------------------------------------------|---------------------------|
//! | `POST /checkout_sessions`                             | `intent.quote`            |
//! | `GET  /checkout_sessions/:id`                         | *read*                    |
//! | `POST /checkout_sessions/:id`                         | `intent.authorize` merge  |
//! | `POST /checkout_sessions/:id/complete`                | `intent.buy`              |
//! | `POST /checkout_sessions/:id/cancel`                  | `intent.return`           |
//!
//! # Identity
//!
//! ACP does not carry an `ICP-Agent-Id` header. The handler synthesizes
//! `did:stateset:agent:acp-<tenant>` as the agent for accounting and
//! receipts. Mandate enforcement is bypassed because the merchant's own
//! bearer key is already on the request — this is the "self-mandate" that
//! `docs/interop.md` describes.
//!
//! [acp]: https://platform.openai.com/docs/agentic-commerce

use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::agent::AgentIdentifier;
use crate::errors::ApiError;
use crate::models::{
    Address as IcpAddress, Buyer as IcpBuyer, IntentContext, IntentEnvelope, LineItem,
    TransactionState,
};
use crate::service::IntentInput;
use crate::AppState;

const ACP_API_VERSION: &str = "2025-09-29";

// --------------------------------------------------------------------------
// ACP wire types (subset)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AcpBuyer {
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AcpAddress {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub line_one: Option<String>,
    #[serde(default)]
    pub line_two: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcpItem {
    pub id: String,
    pub quantity: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreateSessionBody {
    #[serde(default)]
    pub buyer: Option<AcpBuyer>,
    #[serde(default)]
    pub items: Vec<AcpItem>,
    #[serde(default)]
    pub fulfillment_address: Option<AcpAddress>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateSessionBody {
    #[serde(default)]
    pub buyer: Option<AcpBuyer>,
    #[serde(default)]
    pub items: Option<Vec<AcpItem>>,
    #[serde(default)]
    pub fulfillment_address: Option<AcpAddress>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompleteSessionBody {
    #[serde(default)]
    pub buyer: Option<AcpBuyer>,
    #[serde(default)]
    pub payment_data: Option<AcpPaymentData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcpPaymentData {
    pub token: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub billing_address: Option<AcpAddress>,
}

// --------------------------------------------------------------------------
// ACP response shape
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub status: &'static str,
    pub currency: String,
    pub buyer: Value,
    pub line_items: Vec<SessionLineItem>,
    pub fulfillment_address: Option<Value>,
    pub totals: Vec<SessionTotal>,
    pub messages: Vec<Value>,
    pub payment_provider: Value,
    pub order: Option<Value>,
    pub links: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionLineItem {
    pub id: String,
    pub item: Value,
    pub base_amount: i64,
    pub discount: i64,
    pub subtotal: i64,
    pub tax: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTotal {
    #[serde(rename = "type")]
    pub type_: &'static str,
    pub display_text: &'static str,
    pub amount: i64,
}

// --------------------------------------------------------------------------
// Handlers
// --------------------------------------------------------------------------

pub async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateSessionBody>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;

    if body.items.is_empty() {
        return Err(ApiError::InvalidRequest(
            "checkout_sessions: items required".into(),
        ));
    }

    let buyer = body.buyer.map(map_buyer).unwrap_or_default();
    let ship_to = body.fulfillment_address.map(map_address);

    let params = json!({
        "items": body.items.iter().map(|i| json!({
            "sku": i.id,
            "quantity": i.quantity,
            // No price hints on the ACP path — let the engine price.
        })).collect::<Vec<_>>(),
        "buyer": buyer,
        "ship_to": ship_to,
    });

    let quote_body = run_intent(&state, &ctx, "intent.quote", params).await?;
    let txn = quote_body["transaction"].clone();

    // Store the ACP session_id ↔ txn_id mapping under external_refs.
    stamp_external_ref(&state, &txn, "acp_session_id");

    Ok(acp_response(
        StatusCode::CREATED,
        &ctx.request_id,
        session_view(&txn, &state.config.public_base_url),
    ))
}

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;
    let txn = state
        .service
        .transactions
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("checkout_session {id}")))?;
    let v = serde_json::to_value(&txn)?;
    Ok(acp_response(
        StatusCode::OK,
        &ctx.request_id,
        session_view(&v, &state.config.public_base_url),
    ))
}

pub async fn update_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateSessionBody>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;

    // ACP updates merge — we map "session has buyer + address now" to
    // `intent.authorize` so the transaction advances to `authorized`.
    let buyer = body.buyer.map(map_buyer);
    let ship_to = body.fulfillment_address.map(map_address);

    let mut params = json!({ "transaction_id": id });
    if let Some(b) = buyer {
        params["buyer"] = serde_json::to_value(b)?;
    }
    if let Some(addr) = ship_to {
        params["ship_to"] = serde_json::to_value(addr)?;
    }

    let body_val = run_intent(&state, &ctx, "intent.authorize", params).await?;
    let txn = body_val["transaction"].clone();
    Ok(acp_response(
        StatusCode::OK,
        &ctx.request_id,
        session_view(&txn, &state.config.public_base_url),
    ))
}

pub async fn complete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CompleteSessionBody>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;

    let payment = body.payment_data.map(|p| {
        // ACP delegates payment: the caller hands us a vault token we
        // redeem later. Map directly to the ICP delegated-vault
        // instrument.
        json!({
            "method": "delegated_vault",
            "token": p.token,
            "provider": p.provider,
        })
    });

    let params = json!({
        "transaction_id": id,
        "payment": payment.unwrap_or_else(|| json!({
            "method": "delegated_vault",
            "token": "acp_unknown",
        })),
    });

    let body_val = run_intent(&state, &ctx, "intent.buy", params).await?;
    let txn = body_val["transaction"].clone();
    let order = body_val.get("order").cloned();

    let mut view = session_view(&txn, &state.config.public_base_url);
    view.order = order.filter(|v| !v.is_null());
    Ok(acp_response(StatusCode::OK, &ctx.request_id, view))
}

pub async fn cancel_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;
    let params = json!({ "transaction_id": id });
    let body_val = run_intent(&state, &ctx, "intent.return", params).await?;
    let txn = body_val["transaction"].clone();
    Ok(acp_response(
        StatusCode::OK,
        &ctx.request_id,
        session_view(&txn, &state.config.public_base_url),
    ))
}

// --------------------------------------------------------------------------
// Support
// --------------------------------------------------------------------------

struct CompatContext {
    tenant: crate::agent::ApiKeyInfo,
    agent: AgentIdentifier,
    request_id: String,
    trace_id: Option<String>,
}

fn build_context(state: &AppState, headers: &HeaderMap) -> Result<CompatContext, ApiError> {
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::AuthenticationFailed("Bearer token required".into()))?;
    let tenant = state
        .keys
        .lookup(bearer)
        .ok_or_else(|| ApiError::AuthenticationFailed("unknown API key".into()))?;

    // Optional but validated when present.
    if let Some(v) = headers.get("api-version").and_then(|v| v.to_str().ok()) {
        if v != ACP_API_VERSION {
            return Err(ApiError::InvalidRequest(format!(
                "API-Version `{v}` not supported by ACP compat; expected `{ACP_API_VERSION}`"
            )));
        }
    }

    let agent_raw = format!("did:stateset:agent:acp-{}", tenant.tenant_id);
    let agent = AgentIdentifier::parse(&agent_raw);

    let request_id = headers
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("req_{}", Uuid::new_v4().simple()));
    let trace_id = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    Ok(CompatContext {
        tenant,
        agent,
        request_id,
        trace_id,
    })
}

async fn run_intent(
    state: &AppState,
    ctx: &CompatContext,
    intent: &str,
    params: Value,
) -> Result<Value, ApiError> {
    let envelope = IntentEnvelope {
        intent: intent.to_string(),
        intent_id: None,
        transaction_id: None,
        agent_id: ctx.agent.raw.clone(),
        mandate_jti: None,
        params,
        context: IntentContext::default(),
    };
    let input = IntentInput::for_compat(
        envelope,
        ctx.agent.clone(),
        ctx.tenant.clone(),
        ctx.request_id.clone(),
        ctx.trace_id.clone(),
    );
    let body = state.service.handle_intent(input).await?;
    Ok(serde_json::to_value(body)?)
}

fn map_buyer(b: AcpBuyer) -> IcpBuyer {
    IcpBuyer {
        first_name: b.first_name,
        last_name: b.last_name,
        email: b.email,
        phone_number: b.phone_number,
        principal_did: None,
    }
}

fn map_address(a: AcpAddress) -> IcpAddress {
    IcpAddress {
        name: a.name,
        line_one: a.line_one,
        line_two: a.line_two,
        city: a.city,
        state: a.state,
        postal_code: a.postal_code,
        country: a.country,
        phone_number: None,
        email: None,
    }
}

fn session_view(txn: &Value, _public_base_url: &str) -> SessionView {
    let id = txn
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let currency = txn
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("USD")
        .to_string();

    let status = match txn.get("state").and_then(|v| v.as_str()) {
        Some("quoted") => "not_ready_for_payment",
        Some("authorized") => "ready_for_payment",
        Some("completed") | Some("captured") | Some("fulfilled") => "completed",
        Some("reversed") | Some("canceled") | Some("expired") => "canceled",
        _ => "not_ready_for_payment",
    };

    let line_items = txn
        .get("line_items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(map_line_item)
        .collect();

    let totals_obj = txn.get("totals").cloned().unwrap_or_else(|| json!({}));
    let totals = build_totals(&totals_obj);

    SessionView {
        id,
        status,
        currency,
        buyer: txn.get("buyer").cloned().unwrap_or(Value::Null),
        line_items,
        fulfillment_address: txn.get("ship_to").cloned().filter(|v| !v.is_null()),
        totals,
        messages: Vec::new(),
        payment_provider: json!({
            "provider": "stateset",
            "supported_payment_methods": ["card", "delegated_vault"],
        }),
        order: None,
        links: Vec::new(),
    }
}

fn map_line_item(v: Value) -> Option<SessionLineItem> {
    let li: LineItem = serde_json::from_value(v).ok()?;
    Some(SessionLineItem {
        id: li.id,
        item: json!({ "id": li.sku, "quantity": li.quantity }),
        base_amount: li.unit_price.amount_minor,
        discount: 0,
        subtotal: li.subtotal.amount_minor,
        tax: li.tax.as_ref().map(|m| m.amount_minor).unwrap_or(0),
        total: li.total.amount_minor,
    })
}

fn build_totals(totals: &Value) -> Vec<SessionTotal> {
    let mut out = Vec::new();
    if let Some(sub) = totals
        .get("subtotal")
        .and_then(|v| v.get("amount_minor"))
        .and_then(|v| v.as_i64())
    {
        out.push(SessionTotal {
            type_: "items_base_amount",
            display_text: "Subtotal",
            amount: sub,
        });
    }
    if let Some(tax) = totals
        .get("tax")
        .and_then(|v| v.get("amount_minor"))
        .and_then(|v| v.as_i64())
    {
        out.push(SessionTotal {
            type_: "tax",
            display_text: "Tax",
            amount: tax,
        });
    }
    if let Some(total) = totals
        .get("total")
        .and_then(|v| v.get("amount_minor"))
        .and_then(|v| v.as_i64())
    {
        out.push(SessionTotal {
            type_: "total",
            display_text: "Total",
            amount: total,
        });
    }
    out
}

/// Stamp an `external_refs["acp_session_id"] = txn.id` so later compat
/// calls can correlate.
fn stamp_external_ref(state: &AppState, txn: &Value, key: &str) {
    let Some(id) = txn.get("id").and_then(|v| v.as_str()) else {
        return;
    };
    state.service.transactions.update(id, |t| {
        t.external_refs.insert(key.to_string(), t.id.clone());
        t.updated_at = Utc::now();
    });
}

fn acp_response(
    status: StatusCode,
    request_id: &str,
    view: SessionView,
) -> (StatusCode, [(&'static str, HeaderValue); 2], Json<Value>) {
    let api_version = HeaderValue::from_static(ACP_API_VERSION);
    let req_id = HeaderValue::from_str(request_id).unwrap_or(HeaderValue::from_static("unknown"));
    (
        status,
        [("api-version", api_version), ("request-id", req_id)],
        Json(serde_json::to_value(view).unwrap_or(Value::Null)),
    )
}

// Silence an unused-import warning on `TransactionState` until we use it
// for fulfillment-option mapping in v0.2.
#[allow(dead_code)]
fn _unused(_t: TransactionState) {}
