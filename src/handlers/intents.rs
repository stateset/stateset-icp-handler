//! `POST /icp/v1/intents` — the core ICP write path.
//!
//! Wraps the intent service with pre-auth + per-tenant rate limiting,
//! mandate enforcement (delegated to the service), idempotency, and
//! receipt stamping. Compatibility surfaces (ACP, UCP, MCP) call into
//! the same [`crate::service::IcpService::handle_intent`] but skip the
//! HTTP-shaped concerns this handler owns.

use std::time::Instant;

use axum::{extract::State, http::HeaderMap, response::IntoResponse, Json};
use uuid::Uuid;

use crate::constants::headers;
use crate::errors::ApiError;
use crate::idempotency::{CachedResponse, IdempotencyStore, LookupOutcome};
use crate::models::IntentEnvelope;
use crate::service::IntentInput;
use crate::AppState;

use super::{
    client_ip_for_rate_limit, ensure_agent_allowed, resolve_agent, resolve_tenant,
    stamp_receipt_headers,
};

#[utoipa::path(
    post,
    path = "/icp/v1/intents",
    tag = "ICP Core",
    request_body = IntentEnvelope,
    responses(
        (status = 200, description = "Intent processed; response carries signed receipt.", body = crate::models::IntentResponseBody),
        (status = 400, description = "Malformed envelope or invalid intent parameters."),
        (status = 401, description = "Missing or invalid Bearer token / ICP-Agent-Id."),
        (status = 402, description = "Mandate budget exceeded."),
        (status = 403, description = "Mandate scope / merchant / jurisdiction rejection."),
        (status = 409, description = "`ICP-Idempotency-Key` reused with a different request body."),
    ),
)]
pub async fn submit_intent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(envelope): Json<IntentEnvelope>,
) -> Result<axum::response::Response, ApiError> {
    // Pre-auth per-IP rate limit. Fires BEFORE bearer resolution so a
    // flood of fake API keys can't burn unbounded CPU on keystore
    // lookups + 401 logging.
    let client_ip = client_ip_for_rate_limit(&headers, state.config.trust_proxy_headers);
    if let crate::rate_limit::RateLimitDecision::Denied {
        limit,
        retry_after_secs,
    } = state.service.check_pre_auth_rate_limit(&client_ip).await?
    {
        let mut response = ApiError::RateLimited.into_response();
        let h = response.headers_mut();
        if let Ok(v) = http::HeaderValue::from_str(&retry_after_secs.to_string()) {
            h.insert("retry-after", v.clone());
            h.insert("x-ratelimit-reset", v);
        }
        if let Ok(v) = http::HeaderValue::from_str(&limit.to_string()) {
            h.insert("x-ratelimit-limit", v);
        }
        h.insert("x-ratelimit-remaining", http::HeaderValue::from_static("0"));
        h.insert(
            "x-ratelimit-scope",
            http::HeaderValue::from_static("pre-auth"),
        );
        return Ok(response);
    }

    // Resolve tenant (bearer) + agent (ICP-Agent-Id) inline so that a single
    // endpoint can enforce ICP-specific auth semantics without an auth
    // middleware that does not know about intent scopes.
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let agent = resolve_agent(&headers)?;
    ensure_agent_allowed(&tenant, &agent)?;

    if state.config.require_icp_version {
        let got = headers
            .get(headers::ICP_VERSION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::InvalidRequest("ICP-Version header required".into()))?;
        if got != state.config.icp_version {
            return Err(ApiError::InvalidRequest(format!(
                "ICP-Version `{got}` not supported; expected `{}`",
                state.config.icp_version
            )));
        }
    }

    // Per-tenant rate limit.
    let rl_decision = state.service.check_tenant_rate_limit(&tenant).await?;
    let mut rl_headers: Vec<(http::HeaderName, http::HeaderValue)> = Vec::new();
    match rl_decision {
        crate::rate_limit::RateLimitDecision::Allowed {
            limit,
            remaining,
            reset_in_secs,
        } => {
            for (name, value) in [
                ("x-ratelimit-limit", limit.to_string()),
                ("x-ratelimit-remaining", remaining.to_string()),
                ("x-ratelimit-reset", reset_in_secs.to_string()),
            ] {
                if let (Ok(n), Ok(v)) = (
                    http::HeaderName::from_bytes(name.as_bytes()),
                    http::HeaderValue::from_str(&value),
                ) {
                    rl_headers.push((n, v));
                }
            }
        }
        crate::rate_limit::RateLimitDecision::Denied {
            limit,
            retry_after_secs,
        } => {
            let mut response = ApiError::RateLimited.into_response();
            let h = response.headers_mut();
            if let Ok(v) = http::HeaderValue::from_str(&retry_after_secs.to_string()) {
                h.insert("retry-after", v.clone());
                h.insert("x-ratelimit-reset", v);
            }
            if let Ok(v) = http::HeaderValue::from_str(&limit.to_string()) {
                h.insert("x-ratelimit-limit", v);
            }
            h.insert("x-ratelimit-remaining", http::HeaderValue::from_static("0"));
            return Ok(response);
        }
    }

    let mandate_jws = headers
        .get(headers::ICP_MANDATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let request_id = headers
        .get(headers::ICP_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if request_id.is_none() && state.config.require_request_id {
        return Err(ApiError::InvalidRequest(
            "ICP-Request-Id header required for this handler".into(),
        ));
    }
    let request_id = request_id.unwrap_or_else(|| format!("req_{}", Uuid::new_v4().simple()));
    let trace_id = headers
        .get(headers::ICP_TRACE_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Idempotency (ICP spec §13).
    let idempotency_key = headers
        .get(headers::ICP_IDEMPOTENCY_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if idempotency_key.is_none() && state.config.require_idempotency_key {
        return Err(ApiError::InvalidRequest(
            "ICP-Idempotency-Key header required for this handler".into(),
        ));
    }

    // Hash the JCS-canonicalized envelope so retries are matched by
    // *semantic* equivalence, not by raw byte equality.
    let canonical =
        serde_jcs::to_vec(&envelope).map_err(|e| ApiError::ProcessingError(format!("jcs: {e}")))?;
    let request_digest = IdempotencyStore::digest_request(&canonical);

    let tenant_id = tenant.tenant_id.clone();
    let _idempotency_guard = if let Some(key) = idempotency_key.as_deref() {
        Some(state.service.lock_idempotency_key(&tenant_id, key).await)
    } else {
        None
    };
    if let Some(key) = idempotency_key.as_deref() {
        let outcome =
            state
                .service
                .idempotency
                .lookup(&tenant_id, key, &request_digest, chrono::Utc::now());
        match outcome {
            LookupOutcome::Replay(body) => {
                let status =
                    http::StatusCode::from_u16(body.status).unwrap_or(http::StatusCode::OK);
                let body_json: serde_json::Value = serde_json::from_slice(&body.body_json)
                    .map_err(|e| ApiError::ProcessingError(format!("cached response JSON: {e}")))?;
                let mut response = (status, Json(body_json.clone())).into_response();
                let h = response.headers_mut();
                h.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                stamp_receipt_headers(h, &body_json);
                h.insert(
                    "idempotent-replayed",
                    http::HeaderValue::from_static("true"),
                );
                if let Ok(v) = http::HeaderValue::from_str(key) {
                    h.insert("idempotent-key", v);
                }
                // Surface the *current* rate-limit window state on the
                // replay too — the cached response is logically the
                // same intent but the rate counter has moved on.
                for (name, value) in &rl_headers {
                    h.insert(name.clone(), value.clone());
                }
                return Ok(response);
            }
            LookupOutcome::Conflict => {
                return Err(ApiError::IdempotencyConflict(format!(
                    "ICP-Idempotency-Key `{key}` was used previously with a different request body"
                )));
            }
            LookupOutcome::Miss => {}
        }
    }

    let input = IntentInput::for_icp(
        envelope,
        agent,
        tenant,
        mandate_jws.as_deref(),
        request_id,
        trace_id,
    );

    let started = Instant::now();
    let body = state.service.handle_intent(input).await?;
    let intent_name = body.intent.clone();
    let body_json = serde_json::to_value(&body)?;
    crate::metrics::record_intent(intent_name.as_str(), "ok");
    crate::metrics::record_http("/icp/v1/intents", 200, started.elapsed().as_secs_f64());

    // Cache only successful responses — a transient 5xx from the
    // pipeline shouldn't poison the idempotency cache.
    if let Some(key) = idempotency_key.as_deref() {
        let body_bytes = serde_json::to_vec(&body_json)
            .map_err(|e| ApiError::ProcessingError(format!("serialize for cache: {e}")))?;
        state.service.idempotency.store(
            &tenant_id,
            key,
            &request_digest,
            CachedResponse {
                status: 200,
                body_json: body_bytes,
            },
            chrono::Utc::now(),
        );
    }

    let mut response = (http::StatusCode::OK, Json(body_json)).into_response();
    let h = response.headers_mut();
    if !body.receipt.jws.is_empty() {
        if let Ok(v) = http::HeaderValue::from_str(&body.receipt.jws) {
            h.insert(headers::ICP_RECEIPT, v);
        }
        if let Ok(v) = http::HeaderValue::from_str(&body.receipt.kid) {
            h.insert(headers::ICP_RECEIPT_KID, v);
        }
    }
    for (name, value) in rl_headers {
        h.insert(name, value);
    }
    Ok(response)
}
