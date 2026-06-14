//! Read endpoints: transactions, subscriptions, peer quotes, receipts,
//! mandate usage. Every endpoint here is tenant-scoped — cross-tenant
//! reads surface as 404 (identical to a missing row) so id space isn't
//! enumerable across tenant boundaries.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};

use crate::errors::ApiError;
use crate::AppState;

use super::resolve_tenant;

// --------------------------------------------------------------------------
// Transactions
// --------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct ListTransactionsQuery {
    /// Optional `state` filter — one of `draft|quoted|authorized|captured|fulfilled|completed|reversed|canceled|expired`.
    /// Unknown values return 400 (rather than silently empty) so a
    /// typo surfaces fast.
    pub state: Option<String>,
    /// Page size cap. Defaults to 100, max 500.
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/icp/v1/transactions",
    tag = "ICP Core",
    params(
        ("state" = Option<String>, Query, description = "Filter: draft|quoted|authorized|captured|fulfilled|completed|reversed|canceled|expired"),
        ("limit" = Option<usize>, Query, description = "Page size, default 100, max 500"),
    ),
    responses(
        (status = 200, description = "Transactions belonging to the caller's tenant, newest first."),
        (status = 400, description = "Unknown state filter."),
    ),
)]
pub async fn list_transactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListTransactionsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let state_filter = match q.state.as_deref() {
        None | Some("") => None,
        Some(s) => Some(parse_transaction_state_filter(s)?),
    };
    let limit = q.limit.unwrap_or(100).min(500);
    let rows = state
        .service
        .transactions
        .list_for_tenant(&tenant.tenant_id, limit, state_filter);
    Ok(Json(serde_json::json!({
        "data": rows,
        "count": rows.len(),
    })))
}

fn parse_transaction_state_filter(s: &str) -> Result<crate::models::TransactionState, ApiError> {
    use crate::models::TransactionState;
    match s {
        "draft" => Ok(TransactionState::Draft),
        "quoted" => Ok(TransactionState::Quoted),
        "authorized" => Ok(TransactionState::Authorized),
        "captured" => Ok(TransactionState::Captured),
        "fulfilled" => Ok(TransactionState::Fulfilled),
        "completed" => Ok(TransactionState::Completed),
        "reversed" => Ok(TransactionState::Reversed),
        "canceled" => Ok(TransactionState::Canceled),
        "expired" => Ok(TransactionState::Expired),
        other => Err(ApiError::InvalidRequest(format!(
            "unknown state filter '{other}' — expected one of draft|quoted|authorized|captured|fulfilled|completed|reversed|canceled|expired"
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/icp/v1/transactions/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Transaction ID")),
    responses(
        (status = 200, description = "Transaction aggregate.", body = crate::models::Transaction),
        (status = 404, description = "Transaction not found."),
    ),
)]
pub async fn get_transaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    // Cross-tenant ids surface as 404 (not 403) — same shape as a
    // missing row, so callers can't enumerate other tenants' ids.
    // Legacy rows written before `tenant_id` was stamped have
    // `tenant_id == ""` and remain visible to operators querying
    // with an empty bearer tenant; they're invisible to any real
    // tenant — the desired isolation property.
    let txn = match state.service.transactions.get(&id) {
        Some(t) if t.tenant_id == tenant.tenant_id => t,
        _ => return Err(ApiError::ResourceNotFound(format!("transaction {id}"))),
    };
    Ok(Json(serde_json::to_value(txn)?))
}

// --------------------------------------------------------------------------
// Subscriptions
// --------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct ListSubscriptionsQuery {
    /// Optional `status` filter — one of `active|paused|canceled|past_due`.
    /// Unknown values return 400.
    pub status: Option<String>,
    /// Page size cap. Defaults to 100, max 500.
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/icp/v1/subscriptions",
    tag = "ICP Core",
    params(
        ("status" = Option<String>, Query, description = "Filter: active|paused|canceled|past_due"),
        ("limit" = Option<usize>, Query, description = "Page size, default 100, max 500"),
    ),
    responses(
        (status = 200, description = "Subscriptions belonging to the caller's tenant, newest first."),
        (status = 400, description = "Unknown status filter."),
    ),
)]
pub async fn list_subscriptions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListSubscriptionsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let status_filter = match q.status.as_deref() {
        None | Some("") => None,
        Some(s) => Some(parse_subscription_status_filter(s)?),
    };
    let limit = q.limit.unwrap_or(100).min(500);
    let rows = state
        .service
        .subscriptions
        .list_for_tenant(&tenant.tenant_id, limit, status_filter);
    Ok(Json(serde_json::json!({
        "data": rows,
        "count": rows.len(),
    })))
}

fn parse_subscription_status_filter(
    s: &str,
) -> Result<crate::models::SubscriptionStatus, ApiError> {
    use crate::models::SubscriptionStatus;
    match s {
        "trialing" => Ok(SubscriptionStatus::Trialing),
        "active" => Ok(SubscriptionStatus::Active),
        "paused" => Ok(SubscriptionStatus::Paused),
        "canceled" => Ok(SubscriptionStatus::Canceled),
        "past_due" => Ok(SubscriptionStatus::PastDue),
        other => Err(ApiError::InvalidRequest(format!(
            "unknown status filter '{other}' — expected one of trialing|active|paused|canceled|past_due"
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/icp/v1/subscriptions/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Subscription ID")),
    responses(
        (status = 200, description = "Subscription aggregate.", body = crate::models::Subscription),
        (status = 404, description = "Subscription not found."),
    ),
)]
pub async fn get_subscription(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let sub = match state.service.subscriptions.get(&id) {
        Some(s) if s.tenant_id == tenant.tenant_id => s,
        _ => return Err(ApiError::ResourceNotFound(format!("subscription {id}"))),
    };
    Ok(Json(serde_json::to_value(sub)?))
}

// --------------------------------------------------------------------------
// Peer quotes
// --------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct ListPeerQuotesQuery {
    /// Optional `status` filter — one of `pending|quoted|accepted|expired|rejected`.
    /// Unknown values return 400.
    pub status: Option<String>,
    /// Page size cap. Defaults to 100, max 500.
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/icp/v1/peer_quotes",
    tag = "ICP Core",
    params(
        ("status" = Option<String>, Query, description = "Filter: pending|quoted|accepted|expired|rejected"),
        ("limit" = Option<usize>, Query, description = "Page size, default 100, max 500"),
    ),
    responses(
        (status = 200, description = "Peer quotes belonging to the caller's tenant, newest first."),
        (status = 400, description = "Unknown status filter."),
    ),
)]
pub async fn list_peer_quotes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListPeerQuotesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let status_filter = match q.status.as_deref() {
        None | Some("") => None,
        Some(s) => Some(parse_peer_quote_status_filter(s)?),
    };
    let limit = q.limit.unwrap_or(100).min(500);
    let rows = state
        .service
        .peer_quotes
        .list_for_tenant(&tenant.tenant_id, limit, status_filter);
    Ok(Json(serde_json::json!({
        "data": rows,
        "count": rows.len(),
    })))
}

fn parse_peer_quote_status_filter(s: &str) -> Result<crate::models::PeerQuoteStatus, ApiError> {
    use crate::models::PeerQuoteStatus;
    match s {
        "pending" => Ok(PeerQuoteStatus::Pending),
        "quoted" => Ok(PeerQuoteStatus::Quoted),
        "accepted" => Ok(PeerQuoteStatus::Accepted),
        "expired" => Ok(PeerQuoteStatus::Expired),
        "rejected" => Ok(PeerQuoteStatus::Rejected),
        other => Err(ApiError::InvalidRequest(format!(
            "unknown status filter '{other}' — expected one of pending|quoted|accepted|expired|rejected"
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/icp/v1/peer_quotes/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Peer quote ID")),
    responses(
        (status = 200, description = "Peer quote aggregate.", body = crate::models::PeerQuote),
        (status = 404, description = "Peer quote not found."),
    ),
)]
pub async fn get_peer_quote(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let quote = match state.service.peer_quotes.get(&id) {
        Some(q) if q.tenant_id == tenant.tenant_id => q,
        _ => return Err(ApiError::ResourceNotFound(format!("peer_quote {id}"))),
    };
    Ok(Json(serde_json::to_value(quote)?))
}

// --------------------------------------------------------------------------
// Receipts
// --------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct ListReceiptsQuery {
    /// Optional `intent` filter — narrows to receipts signed for a
    /// specific intent (e.g. `intent.buy`). Useful for audit
    /// dashboards that segment by flow.
    pub intent: Option<String>,
    /// Page size cap. Defaults to 100, max 500 — matches the other
    /// resource list endpoints.
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/icp/v1/receipts",
    tag = "ICP Core",
    params(
        ("intent" = Option<String>, Query, description = "Filter to receipts signed for a specific intent (e.g. intent.buy)"),
        ("limit" = Option<usize>, Query, description = "Page size, default 100, max 500"),
    ),
    responses(
        (status = 200, description = "Receipts belonging to the caller's tenant, newest first by signed-at timestamp."),
    ),
)]
pub async fn list_receipts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListReceiptsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let limit = q.limit.unwrap_or(100).min(500);

    // Receipts don't carry a tenant_id of their own — derive
    // ownership at read time via `claims.icp.transaction_id` →
    // transaction lookup. We over-fetch (3x the requested limit) so
    // that filtering out cross-tenant rows still gives the caller
    // close to a full page back. Bounded by the same 500 hard cap.
    let scan_limit = (limit.saturating_mul(3)).min(1500);
    let raw = state.service.receipts.list_recent(scan_limit);

    let intent_filter = q.intent.as_deref();
    let rows: Vec<serde_json::Value> = raw
        .into_iter()
        .filter(|r| match intent_filter {
            Some(want) => r.claims.icp.intent == want,
            None => true,
        })
        .filter(|r| {
            // Tenant-derive via the backing transaction. Receipts
            // whose backing transaction has been GC'd or pre-dates
            // `tenant_id` stamping (`tenant_id == ""`) are invisible
            // to any real tenant — conservative default that matches
            // the documented isolation behavior of the get endpoint.
            state
                .service
                .transactions
                .get(&r.claims.icp.transaction_id)
                .is_some_and(|t| t.tenant_id == tenant.tenant_id)
        })
        .take(limit)
        .map(|r| {
            serde_json::json!({
                "jti": r.jti,
                "kid": r.kid,
                "jws": r.jws,
                "body_digest": r.body_digest,
                "claims": r.claims,
            })
        })
        .collect();

    let count = rows.len();
    Ok(Json(serde_json::json!({ "data": rows, "count": count })))
}

#[utoipa::path(
    get,
    path = "/icp/v1/receipts/{jti}",
    tag = "ICP Core",
    params(("jti" = String, Path, description = "Receipt JWT ID")),
    responses(
        (status = 200, description = "Signed receipt body + claims."),
        (status = 404, description = "Receipt not found."),
    ),
)]
pub async fn get_receipt(
    State(state): State<AppState>,
    Path(jti): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let r = state
        .service
        .receipts
        .get(&jti)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("receipt {jti}")))?;
    // Receipts don't carry tenant_id directly — they predate the
    // multi-tenant work and the signed claims shape is wire-stable
    // (changing it would break receipt verifiers). Instead, derive
    // ownership via the transaction the receipt was signed over:
    // every receipt embeds `claims.icp.transaction_id`. Cross-tenant
    // requests surface as 404 (identical to a missing receipt) so
    // jti space isn't enumerable across tenants.
    let txn_tenant = state
        .service
        .transactions
        .get(&r.claims.icp.transaction_id)
        .map(|t| t.tenant_id);
    if txn_tenant.as_deref() != Some(tenant.tenant_id.as_str()) {
        return Err(ApiError::ResourceNotFound(format!("receipt {jti}")));
    }
    Ok(Json(serde_json::json!({
        "jti": r.jti,
        "kid": r.kid,
        "jws": r.jws,
        "body_digest": r.body_digest,
        "claims": r.claims,
    })))
}

// --------------------------------------------------------------------------
// Mandate usage
// --------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/icp/v1/mandates/{jti}/usage",
    tag = "ICP Core",
    params(("jti" = String, Path, description = "Mandate JWT ID")),
    responses(
        (status = 200, description = "Current spend accumulated against the mandate's budget window for the calling tenant."),
        (status = 404, description = "No spend recorded for this mandate, or it belongs to a different tenant — existence is not leaked."),
    ),
)]
pub async fn get_mandate_usage(
    State(state): State<AppState>,
    Path(jti): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    // Tenant scope: only the *first* tenant to record spend against
    // this jti can read the tally. Subsequent spends from other
    // tenants still consume the shared budget (protecting the
    // principal who issued the mandate) but are unreadable here.
    let usage = state
        .service
        .mandates
        .try_usage_for_tenant(&jti, &tenant.tenant_id)?
        .ok_or_else(|| ApiError::ResourceNotFound(format!("mandate_usage {jti}")))?;
    Ok(Json(serde_json::json!({
        "jti": jti,
        "spent_minor": usage.spent_minor,
        "window_start": usage.window_start,
    })))
}
