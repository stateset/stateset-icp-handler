//! UCP (Universal Commerce Protocol) compatibility surface.
//!
//! Implements a subset of [StateSet UCP `2026-01-11`][ucp] sufficient
//! for a UCP-native agent to complete checkout on an ICP handler without
//! modification.
//!
//! # Endpoint mapping
//!
//! | UCP path                                             | ICP intent                |
//! |------------------------------------------------------|---------------------------|
//! | `POST /checkout-sessions`                            | `intent.quote`            |
//! | `GET  /checkout-sessions/:id`                        | *read*                    |
//! | `PUT  /checkout-sessions/:id`                        | `intent.authorize` merge  |
//! | `POST /checkout-sessions/:id/complete`               | `intent.buy`              |
//! | `POST /checkout-sessions/:id/cancel`                 | `intent.return`           |
//!
//! UCP differs from ACP in three main ways: path casing
//! (`/checkout-sessions` vs `/checkout_sessions`), verb for updates
//! (`PUT` vs `POST`), and a richer response envelope wrapping each
//! resource in a `ucp` meta block that advertises the negotiated
//! capability set.
//!
//! Identity + auth follow the same self-mandate model as ACP: the
//! tenant's bearer token authorizes the operation, and the handler
//! synthesizes `did:stateset:agent:ucp-<tenant>` as the agent for
//! receipts and event correlation.
//!
//! [ucp]: https://github.com/stateset/stateset-ucp-handler

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
};
use crate::service::IntentInput;
use crate::AppState;

pub const UCP_VERSION: &str = "2026-01-11";

// --------------------------------------------------------------------------
// UCP request types (subset)
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ItemRef {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LineItemInput {
    pub item: ItemRef,
    pub quantity: i32,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UcpBuyer {
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
pub struct UcpAddress {
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FulfillmentInput {
    #[serde(default)]
    pub address: Option<UcpAddress>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PaymentRequestInput {
    #[serde(default)]
    pub selected_instrument_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCheckoutBody {
    pub line_items: Vec<LineItemInput>,
    #[serde(default)]
    pub buyer: Option<UcpBuyer>,
    pub currency: String,
    #[serde(default)]
    pub payment: Option<PaymentRequestInput>,
    #[serde(default)]
    pub fulfillment: Option<FulfillmentInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateCheckoutBody {
    #[serde(default)]
    pub line_items: Option<Vec<LineItemInput>>,
    #[serde(default)]
    pub buyer: Option<UcpBuyer>,
    #[serde(default)]
    pub fulfillment: Option<FulfillmentInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompleteCheckoutBody {
    pub payment_data: PaymentData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaymentData {
    #[serde(rename = "type")]
    pub instrument_type: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub handler_id: Option<String>,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub last_digits: Option<String>,
}

// --------------------------------------------------------------------------
// UCP response types
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct UcpResponseMeta {
    pub version: String,
    pub capabilities: Vec<CapabilityRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityRef {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckoutResponse {
    pub ucp: UcpResponseMeta,
    pub id: String,
    pub status: &'static str,
    pub currency: String,
    pub line_items: Vec<LineItemResponse>,
    pub totals: Vec<Total>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buyer: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fulfillment: Option<Value>,
    pub payment: Value,
    pub links: Vec<Link>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LineItemResponse {
    pub id: String,
    pub item: ItemResponseBrief,
    pub quantity: i64,
    pub totals: Vec<Total>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemResponseBrief {
    pub id: String,
    pub title: String,
    pub price: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Total {
    #[serde(rename = "type")]
    pub total_type: &'static str,
    pub display_text: &'static str,
    pub amount: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Link {
    #[serde(rename = "type")]
    pub link_type: &'static str,
    pub url: String,
}

// --------------------------------------------------------------------------
// Handlers
// --------------------------------------------------------------------------

pub async fn create_checkout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateCheckoutBody>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;

    if body.line_items.is_empty() {
        return Err(ApiError::InvalidRequest(
            "checkout: line_items must be non-empty".into(),
        ));
    }

    let buyer = body.buyer.map(map_buyer).unwrap_or_default();
    let ship_to = body
        .fulfillment
        .as_ref()
        .and_then(|f| f.address.as_ref())
        .cloned()
        .map(map_address);

    let items: Vec<Value> = body
        .line_items
        .iter()
        .map(|li| {
            json!({
                "sku": li.item.id,
                "quantity": li.quantity as i64,
            })
        })
        .collect();

    let params = json!({
        "items": items,
        "buyer": buyer,
        "ship_to": ship_to,
    });

    let mut envelope = base_envelope(&ctx, "intent.quote", params);
    envelope.context.currency = Some(body.currency.clone());

    let quote_body = run_with_envelope(&state, &ctx, envelope).await?;
    let txn = quote_body["transaction"].clone();
    stamp_external_ref(&state, &txn, "ucp_session_id");

    Ok(ucp_response(
        StatusCode::CREATED,
        &ctx.request_id,
        checkout_response(&txn, &state.config.public_base_url),
    ))
}

pub async fn get_checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;
    let txn = state
        .service
        .transactions
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("checkout-session {id}")))?;
    let v = serde_json::to_value(&txn)?;
    Ok(ucp_response(
        StatusCode::OK,
        &ctx.request_id,
        checkout_response(&v, &state.config.public_base_url),
    ))
}

pub async fn update_checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateCheckoutBody>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;

    let buyer = body.buyer.map(map_buyer);
    let ship_to = body.fulfillment.and_then(|f| f.address).map(map_address);

    let mut params = json!({ "transaction_id": id });
    if let Some(b) = buyer {
        params["buyer"] = serde_json::to_value(b)?;
    }
    if let Some(addr) = ship_to {
        params["ship_to"] = serde_json::to_value(addr)?;
    }

    let body_val = run_intent(&state, &ctx, "intent.authorize", params).await?;
    let txn = body_val["transaction"].clone();
    Ok(ucp_response(
        StatusCode::OK,
        &ctx.request_id,
        checkout_response(&txn, &state.config.public_base_url),
    ))
}

pub async fn complete_checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CompleteCheckoutBody>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;

    // UCP payment_data.type drives the ICP PaymentInstrument variant.
    let payment = match body.payment_data.instrument_type.as_str() {
        "delegated_vault" | "vault_token" => json!({
            "method": "delegated_vault",
            "token": body.payment_data.token.unwrap_or_default(),
            "provider": body.payment_data.handler_id,
        }),
        "stablecoin" => json!({
            "method": "stablecoin",
            "asset": "USDC",
            "chain": body.payment_data.handler_id.unwrap_or_else(|| "base".into()),
            "from": body.payment_data.token.unwrap_or_default(),
        }),
        // Default — treat as a card token (covers "card", "credit_card").
        _ => json!({
            "method": "card",
            "token": body.payment_data.token,
            "last_digits": body.payment_data.last_digits,
            "brand": body.payment_data.brand,
        }),
    };

    let params = json!({ "transaction_id": id, "payment": payment });
    let body_val = run_intent(&state, &ctx, "intent.buy", params).await?;
    let txn = body_val["transaction"].clone();
    let order = body_val.get("order").cloned();

    let mut resp = checkout_response(&txn, &state.config.public_base_url);
    resp.order = order.filter(|v| !v.is_null());
    Ok(ucp_response(StatusCode::OK, &ctx.request_id, resp))
}

pub async fn cancel_checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = build_context(&state, &headers)?;
    let params = json!({ "transaction_id": id });
    let body_val = run_intent(&state, &ctx, "intent.return", params).await?;
    let txn = body_val["transaction"].clone();
    Ok(ucp_response(
        StatusCode::OK,
        &ctx.request_id,
        checkout_response(&txn, &state.config.public_base_url),
    ))
}

/// `GET /.well-known/ucp` — minimal UCP-shaped discovery advertising the
/// checkout-session capability. Full UCP service negotiation is deferred
/// to v0.2.
pub async fn discovery(State(state): State<AppState>) -> impl IntoResponse {
    let base = &state.config.public_base_url;
    Json(json!({
        "ucp": {
            "profile": "https://spec.ucp.dev/profile/shopping/2026-01-11",
            "version": UCP_VERSION,
            "capabilities": [
                { "name": "dev.ucp.shopping", "version": UCP_VERSION }
            ],
            "services": {
                "dev.ucp.shopping": {
                    "version": UCP_VERSION,
                    "spec": "https://spec.ucp.dev/shopping",
                    "rest": {
                        "endpoint": format!("{base}/checkout-sessions"),
                    },
                    "intents": [
                        "intent.quote", "intent.authorize", "intent.buy",
                        "intent.track", "intent.return",
                    ],
                }
            },
        },
        "compat": {
            "icp_base_url": base,
            "acp_base_url": base,
        }
    }))
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

    if let Some(v) = headers.get("ucp-version").and_then(|v| v.to_str().ok()) {
        if v != UCP_VERSION {
            return Err(ApiError::InvalidRequest(format!(
                "UCP-Version `{v}` not supported; expected `{UCP_VERSION}`"
            )));
        }
    }

    let agent_raw = format!("did:stateset:agent:ucp-{}", tenant.tenant_id);
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

fn base_envelope(ctx: &CompatContext, intent: &str, params: Value) -> IntentEnvelope {
    IntentEnvelope {
        intent: intent.to_string(),
        intent_id: None,
        transaction_id: None,
        agent_id: ctx.agent.raw.clone(),
        mandate_jti: None,
        params,
        context: IntentContext::default(),
    }
}

async fn run_intent(
    state: &AppState,
    ctx: &CompatContext,
    intent: &str,
    params: Value,
) -> Result<Value, ApiError> {
    run_with_envelope(state, ctx, base_envelope(ctx, intent, params)).await
}

async fn run_with_envelope(
    state: &AppState,
    ctx: &CompatContext,
    envelope: IntentEnvelope,
) -> Result<Value, ApiError> {
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

fn map_buyer(b: UcpBuyer) -> IcpBuyer {
    IcpBuyer {
        first_name: b.first_name,
        last_name: b.last_name,
        email: b.email,
        phone_number: b.phone_number,
        principal_did: None,
    }
}

fn map_address(a: UcpAddress) -> IcpAddress {
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

fn ucp_status_for(state: &str) -> &'static str {
    match state {
        "draft" | "quoted" => "incomplete",
        "authorized" => "ready_for_complete",
        "captured" | "fulfilled" | "completed" => "completed",
        "reversed" | "canceled" | "expired" => "canceled",
        _ => "incomplete",
    }
}

fn checkout_response(txn: &Value, _base: &str) -> CheckoutResponse {
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
    let state_str = txn.get("state").and_then(|v| v.as_str()).unwrap_or("draft");
    let status = ucp_status_for(state_str);

    let line_items: Vec<LineItemResponse> = txn
        .get("line_items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(map_line_item)
        .collect();

    let totals = build_totals(txn.get("totals").cloned().unwrap_or_else(|| json!({})));

    let buyer = txn
        .get("buyer")
        .cloned()
        .filter(|v| !v.is_null() && !is_empty_object(v));
    let fulfillment = txn
        .get("ship_to")
        .cloned()
        .filter(|v| !v.is_null())
        .map(|addr| json!({ "methods": [], "available_methods": [], "address": addr }));

    CheckoutResponse {
        ucp: UcpResponseMeta {
            version: UCP_VERSION.to_string(),
            capabilities: vec![CapabilityRef {
                name: "dev.ucp.shopping".into(),
                version: UCP_VERSION.into(),
            }],
        },
        id,
        status,
        currency,
        line_items,
        totals,
        buyer,
        fulfillment,
        payment: json!({
            "handlers": [
                { "id": "card", "type": "card" },
                { "id": "delegated_vault", "type": "delegated_vault" }
            ],
            "instruments": []
        }),
        links: Vec::new(),
        order: None,
    }
}

fn map_line_item(v: Value) -> Option<LineItemResponse> {
    let li: LineItem = serde_json::from_value(v).ok()?;
    let quantity = li.quantity;
    let total_minor = li.total.amount_minor;
    let subtotal_minor = li.subtotal.amount_minor;
    let tax_minor = li.tax.as_ref().map(|m| m.amount_minor).unwrap_or(0);
    let mut totals = vec![Total {
        total_type: "subtotal",
        display_text: "Subtotal",
        amount: subtotal_minor,
    }];
    if tax_minor > 0 {
        totals.push(Total {
            total_type: "tax",
            display_text: "Tax",
            amount: tax_minor,
        });
    }
    totals.push(Total {
        total_type: "total",
        display_text: "Total",
        amount: total_minor,
    });
    Some(LineItemResponse {
        id: li.id,
        item: ItemResponseBrief {
            id: li.sku.clone(),
            title: li.name,
            price: li.unit_price.amount_minor,
        },
        quantity,
        totals,
    })
}

fn build_totals(totals: Value) -> Vec<Total> {
    let mut out = Vec::new();
    let pick = |key: &str| -> Option<i64> {
        totals
            .get(key)
            .and_then(|v| v.get("amount_minor"))
            .and_then(|v| v.as_i64())
    };
    if let Some(v) = pick("subtotal") {
        out.push(Total {
            total_type: "subtotal",
            display_text: "Subtotal",
            amount: v,
        });
    }
    if let Some(v) = pick("shipping") {
        out.push(Total {
            total_type: "shipping",
            display_text: "Shipping",
            amount: v,
        });
    }
    if let Some(v) = pick("tax") {
        out.push(Total {
            total_type: "tax",
            display_text: "Tax",
            amount: v,
        });
    }
    if let Some(v) = pick("total") {
        out.push(Total {
            total_type: "total",
            display_text: "Total",
            amount: v,
        });
    }
    out
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.values().all(|x| x.is_null()))
}

fn stamp_external_ref(state: &AppState, txn: &Value, key: &str) {
    let Some(id) = txn.get("id").and_then(|v| v.as_str()) else {
        return;
    };
    state.service.transactions.update(id, |t| {
        t.external_refs.insert(key.to_string(), t.id.clone());
        t.updated_at = Utc::now();
    });
}

fn ucp_response(
    status: StatusCode,
    request_id: &str,
    view: CheckoutResponse,
) -> (StatusCode, [(&'static str, HeaderValue); 2], Json<Value>) {
    let ucp_version = HeaderValue::from_static(UCP_VERSION);
    let req_id = HeaderValue::from_str(request_id).unwrap_or(HeaderValue::from_static("unknown"));
    (
        status,
        [("ucp-version", ucp_version), ("request-id", req_id)],
        Json(serde_json::to_value(view).unwrap_or(Value::Null)),
    )
}
