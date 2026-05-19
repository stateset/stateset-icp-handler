//! Webhook delivery + subscriber CRUD.
//!
//! All endpoints in this module are tenant-scoped — cross-tenant ids
//! surface as 404 so id space isn't enumerable across tenant boundaries.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};

use crate::errors::ApiError;
use crate::AppState;

use super::{resolve_tenant, validate_webhook_url};

// --------------------------------------------------------------------------
// Deliveries
// --------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct ListDeliveriesQuery {
    /// Optional `status` filter — one of `pending`, `in_flight`,
    /// `delivered`, `failed`, `dead_lettered`. Anything else returns
    /// 400 (rather than silently empty) so a typo surfaces fast.
    pub status: Option<String>,
    /// Page size cap. Defaults to 100, max 500 — keeps a single
    /// request from pulling unbounded payloads into memory.
    pub limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/icp/v1/webhook_deliveries",
    tag = "ICP Core",
    params(
        ("status" = Option<String>, Query, description = "Filter: pending|in_flight|delivered|failed|dead_lettered"),
        ("limit" = Option<usize>, Query, description = "Page size, default 100, max 500"),
    ),
    responses(
        (status = 200, description = "Recent outbound webhook delivery attempts for the caller's tenant."),
        (status = 400, description = "Unknown status filter."),
    ),
)]
pub async fn list_webhook_deliveries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListDeliveriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let status = match q.status.as_deref() {
        None | Some("") => None,
        Some(s) => Some(parse_status_filter(s)?),
    };
    let limit = q.limit.unwrap_or(100).min(500);
    let recent =
        state
            .service
            .webhook_outbox
            .list_recent_for_tenant(&tenant.tenant_id, limit, status);
    Ok(Json(serde_json::json!({
        "data": recent,
        "count": recent.len(),
    })))
}

fn parse_status_filter(s: &str) -> Result<crate::webhook::DeliveryStatus, ApiError> {
    use crate::webhook::DeliveryStatus;
    match s {
        "pending" => Ok(DeliveryStatus::Pending),
        "in_flight" => Ok(DeliveryStatus::InFlight),
        "delivered" => Ok(DeliveryStatus::Delivered),
        "failed" => Ok(DeliveryStatus::Failed),
        "dead_lettered" => Ok(DeliveryStatus::DeadLettered),
        other => Err(ApiError::InvalidRequest(format!(
            "unknown status filter '{other}' — expected one of pending|in_flight|delivered|failed|dead_lettered"
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/icp/v1/webhook_deliveries/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Delivery ID")),
    responses(
        (status = 200, description = "Single webhook delivery record."),
        (status = 404, description = "Delivery not found (or belongs to a different tenant — existence is not leaked)."),
    ),
)]
pub async fn get_webhook_delivery(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let d = state
        .service
        .webhook_outbox
        .get_for_tenant(&id, &tenant.tenant_id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_delivery {id}")))?;
    Ok(Json(serde_json::to_value(d)?))
}

/// Operator-initiated retry of a `failed` or `dead_lettered` webhook
/// delivery. Tenant-scoped: another tenant's delivery id surfaces as
/// 404, identical to a missing row, so existence isn't leaked across
/// tenants. Maps `RetryError` variants to the spec-aligned HTTP error
/// shape: `NotFound` → 404, the three "wrong state" variants → 412.
#[utoipa::path(
    post,
    path = "/icp/v1/webhook_deliveries/{id}/retry",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Delivery ID")),
    responses(
        (status = 200, description = "Delivery reset to pending; will be picked up on the next worker tick.", body = crate::webhook::WebhookDelivery),
        (status = 404, description = "Delivery not found (or belongs to a different tenant)."),
        (status = 412, description = "Delivery is in a state that can't be retried (already pending, in flight, or already delivered)."),
    ),
)]
pub async fn retry_webhook_delivery(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    match state.service.webhook_outbox.reset_for_retry_for_tenant(
        &id,
        &tenant.tenant_id,
        chrono::Utc::now(),
    ) {
        Ok(d) => Ok(Json(serde_json::to_value(d)?)),
        Err(crate::webhook::RetryError::NotFound) => {
            Err(ApiError::ResourceNotFound(format!("webhook_delivery {id}")))
        }
        Err(e) => Err(ApiError::PreconditionFailed(e.message().to_string())),
    }
}

// --------------------------------------------------------------------------
// Subscribers
// --------------------------------------------------------------------------

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct CreateSubscriberBody {
    pub url: String,
    pub secret: String,
}

/// Register a new per-tenant webhook subscriber. Tenant scope is
/// implicit — the row's `tenant_id` is taken from the bearer key, so
/// the caller can never create subscribers for a tenant they don't
/// authenticate as. Validates the URL is a non-empty `http(s)://`
/// string and the secret is non-empty.
#[utoipa::path(
    post,
    path = "/icp/v1/webhook_subscribers",
    tag = "ICP Core",
    request_body = CreateSubscriberBody,
    responses(
        (status = 200, description = "Subscriber created. The `secret` is returned ONCE here for the caller to store; subsequent reads redact it.", body = crate::webhook::WebhookSubscriber),
        (status = 400, description = "Invalid URL (must be http(s)://) or empty secret."),
        (status = 401, description = "Missing or invalid bearer key."),
    ),
)]
pub async fn create_webhook_subscriber(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateSubscriberBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let url = body.url.trim();
    validate_webhook_url(url, state.config.allow_insecure_urls)?;
    if body.secret.trim().is_empty() {
        return Err(ApiError::InvalidRequest(
            "secret must be non-empty — events are HMAC-signed and the receiver needs the same secret to verify".into(),
        ));
    }
    let now = chrono::Utc::now();
    let sub = crate::webhook::WebhookSubscriber {
        id: format!("whsub_{}", uuid::Uuid::new_v4().simple()),
        tenant_id: tenant.tenant_id.clone(),
        url: url.to_string(),
        secret: Some(body.secret),
        active: true,
        created_at: now,
        updated_at: now,
    };
    state.service.webhook_subscribers.insert(sub.clone());
    Ok(Json(serde_json::to_value(sub)?))
}

/// List the calling tenant's subscribers (active + disabled). Secrets
/// are redacted in the response — the create call is the only time
/// the secret is returned.
#[utoipa::path(
    get,
    path = "/icp/v1/webhook_subscribers",
    tag = "ICP Core",
    responses(
        (status = 200, description = "All subscribers belonging to the caller's tenant (active + disabled). Secrets redacted."),
    ),
)]
pub async fn list_webhook_subscribers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let mut rows = state
        .service
        .webhook_subscribers
        .list_for_tenant(&tenant.tenant_id);
    for s in rows.iter_mut() {
        s.secret = None;
    }
    let count = rows.len();
    Ok(Json(serde_json::json!({ "data": rows, "count": count })))
}

#[utoipa::path(
    get,
    path = "/icp/v1/webhook_subscribers/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Subscriber ID")),
    responses(
        (status = 200, description = "Subscriber detail (secret redacted).", body = crate::webhook::WebhookSubscriber),
        (status = 404, description = "Subscriber not found (or belongs to a different tenant — existence is not leaked)."),
    ),
)]
pub async fn get_webhook_subscriber(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let mut sub = state
        .service
        .webhook_subscribers
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_subscriber {id}")))?;
    if sub.tenant_id != tenant.tenant_id {
        // Don't leak existence across tenants — same response as miss.
        return Err(ApiError::ResourceNotFound(format!(
            "webhook_subscriber {id}"
        )));
    }
    sub.secret = None;
    Ok(Json(serde_json::to_value(sub)?))
}

#[derive(Debug, Default, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateSubscriberBody {
    /// New webhook destination URL. Must be `http(s)://` if supplied.
    /// Omit (or send `null`) to leave the URL unchanged.
    pub url: Option<String>,
    /// New HMAC signing secret. Omit (or send `null`) to leave the
    /// secret unchanged. Sending an empty string is rejected — the
    /// receiver needs *some* secret to verify deliveries.
    pub secret: Option<String>,
}

/// Update a subscriber's URL and/or secret in place. Critical for
/// secret rotation and for moving a destination URL without
/// disrupting the verifier configuration on the downstream side
/// (delete + recreate would rotate the id, breaking any caller that
/// holds the existing id). Tenant-scoped: cross-tenant ids 404.
#[utoipa::path(
    patch,
    path = "/icp/v1/webhook_subscribers/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Subscriber ID")),
    request_body = UpdateSubscriberBody,
    responses(
        (status = 200, description = "Subscriber updated. Response includes the updated row with the secret redacted (the rotated secret is in the request, not the response).", body = crate::webhook::WebhookSubscriber),
        (status = 400, description = "Invalid URL or empty secret."),
        (status = 404, description = "Subscriber not found (or belongs to a different tenant)."),
    ),
)]
pub async fn update_webhook_subscriber(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateSubscriberBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;

    // Validate inputs BEFORE checking existence so a malformed
    // request gets the right error class regardless of whether the
    // subscriber exists.
    let url_trimmed = body.url.as_deref().map(str::trim);
    if let Some(u) = url_trimmed {
        validate_webhook_url(u, state.config.allow_insecure_urls)?;
    }
    let secret_trimmed = body.secret.as_deref().map(str::trim);
    if let Some(s) = secret_trimmed {
        if s.is_empty() {
            return Err(ApiError::InvalidRequest(
                "secret must be non-empty — events are HMAC-signed and the receiver needs the same secret to verify".into(),
            ));
        }
    }

    let existing = state
        .service
        .webhook_subscribers
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_subscriber {id}")))?;
    if existing.tenant_id != tenant.tenant_id {
        return Err(ApiError::ResourceNotFound(format!(
            "webhook_subscriber {id}"
        )));
    }

    let mut updated = state
        .service
        .webhook_subscribers
        .patch(&id, url_trimmed, secret_trimmed, chrono::Utc::now())
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_subscriber {id}")))?;
    updated.secret = None;
    Ok(Json(serde_json::to_value(updated)?))
}

/// Soft-disable a subscriber — flips `active` to false.
#[utoipa::path(
    post,
    path = "/icp/v1/webhook_subscribers/{id}/disable",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Subscriber ID")),
    responses(
        (status = 200, description = "Subscriber disabled. `active` is now false; future events skip this destination.", body = crate::webhook::WebhookSubscriber),
        (status = 404, description = "Subscriber not found (or belongs to a different tenant)."),
    ),
)]
pub async fn disable_webhook_subscriber(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    set_webhook_subscriber_active(state, id, headers, false).await
}

/// Re-enable a previously-disabled subscriber. Idempotent.
#[utoipa::path(
    post,
    path = "/icp/v1/webhook_subscribers/{id}/enable",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Subscriber ID")),
    responses(
        (status = 200, description = "Subscriber re-enabled. `active` is now true; future events fan out to this destination again. Secret is unchanged.", body = crate::webhook::WebhookSubscriber),
        (status = 404, description = "Subscriber not found (or belongs to a different tenant)."),
    ),
)]
pub async fn enable_webhook_subscriber(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    set_webhook_subscriber_active(state, id, headers, true).await
}

async fn set_webhook_subscriber_active(
    state: AppState,
    id: String,
    headers: HeaderMap,
    active: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let existing = state
        .service
        .webhook_subscribers
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_subscriber {id}")))?;
    if existing.tenant_id != tenant.tenant_id {
        return Err(ApiError::ResourceNotFound(format!(
            "webhook_subscriber {id}"
        )));
    }
    let mut updated = state
        .service
        .webhook_subscribers
        .set_active(&id, active, chrono::Utc::now())
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_subscriber {id}")))?;
    updated.secret = None;
    Ok(Json(serde_json::to_value(updated)?))
}

#[utoipa::path(
    delete,
    path = "/icp/v1/webhook_subscribers/{id}",
    tag = "ICP Core",
    params(("id" = String, Path, description = "Subscriber ID")),
    responses(
        (status = 200, description = "Subscriber deleted. Use `disable` instead if you want to keep the row for audit."),
        (status = 404, description = "Subscriber not found (or belongs to a different tenant)."),
    ),
)]
pub async fn delete_webhook_subscriber(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let existing = state
        .service
        .webhook_subscribers
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("webhook_subscriber {id}")))?;
    if existing.tenant_id != tenant.tenant_id {
        return Err(ApiError::ResourceNotFound(format!(
            "webhook_subscriber {id}"
        )));
    }
    state.service.webhook_subscribers.delete(&id);
    Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
}
