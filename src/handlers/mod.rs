//! HTTP request handlers, grouped by concern.
//!
//! Each submodule covers one slice of the ICP HTTP surface:
//!
//! - [`ops`] — `/`, `/health`, `/ready`, `/metrics`
//! - [`discovery`] — `/.well-known/icp`, `/.well-known/icp/jwks.json`
//! - [`intents`] — `POST /icp/v1/intents` (the core write path)
//! - [`queries`] — read endpoints for transactions, subscriptions, peer quotes,
//!   receipts, mandate usage
//! - [`webhook_admin`] — webhook delivery + subscriber CRUD
//! - [`events`] — SSE event stream
//!
//! Shared cross-handler helpers (auth, header parsing, body stamping) live in
//! this module.

pub mod discovery;
pub mod events;
pub mod intents;
pub mod ops;
pub mod queries;
pub mod webhook_admin;

use axum::http::{HeaderMap, HeaderValue};

use crate::agent::{AgentIdentifier, ApiKeyInfo, ApiKeyStore};
use crate::constants::headers;
use crate::errors::ApiError;

pub(crate) fn resolve_tenant(
    headers: &HeaderMap,
    keys: &ApiKeyStore,
) -> Result<ApiKeyInfo, ApiError> {
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::AuthenticationFailed("Bearer token required".into()))?;
    let tenant = keys
        .lookup(bearer)
        .ok_or_else(|| ApiError::AuthenticationFailed("unknown API key".into()))?;
    if tenant.is_expired_at(chrono::Utc::now()) {
        return Err(ApiError::AuthenticationFailed("API key expired".into()));
    }
    Ok(tenant)
}

pub(crate) fn resolve_agent(headers: &HeaderMap) -> Result<AgentIdentifier, ApiError> {
    let v = headers
        .get(headers::ICP_AGENT_ID)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::AuthenticationFailed("ICP-Agent-Id header required".into()))?;
    Ok(AgentIdentifier::parse(v))
}

pub(crate) fn ensure_agent_allowed(
    tenant: &ApiKeyInfo,
    agent: &AgentIdentifier,
) -> Result<(), ApiError> {
    if tenant.permits_agent(&agent.raw) {
        Ok(())
    } else {
        Err(ApiError::AuthenticationFailed(format!(
            "agent `{}` is not allowed for this API key",
            agent.raw
        )))
    }
}

pub(crate) fn stamp_receipt_headers(out_headers: &mut HeaderMap, body_json: &serde_json::Value) {
    let Some(receipt) = body_json.get("receipt") else {
        return;
    };
    if let Some(jws) = receipt.get("jws").and_then(|v| v.as_str()) {
        if !jws.is_empty() {
            if let Ok(v) = HeaderValue::from_str(jws) {
                out_headers.insert(headers::ICP_RECEIPT, v);
            }
        }
    }
    if let Some(kid) = receipt.get("kid").and_then(|v| v.as_str()) {
        if !kid.is_empty() {
            if let Ok(v) = HeaderValue::from_str(kid) {
                out_headers.insert(headers::ICP_RECEIPT_KID, v);
            }
        }
    }
}

pub(crate) fn validate_webhook_url(url: &str, allow_insecure: bool) -> Result<(), ApiError> {
    crate::webhook::validate_destination_url(url, allow_insecure).map_err(ApiError::InvalidRequest)
}

/// Extract a client identifier suitable for the pre-auth rate-limit
/// bucket key. Reads `X-Forwarded-For` (the canonical proxy header) and
/// returns the first segment — the originating client IP per RFC 7239.
/// `X-Real-IP` is checked as a fallback (nginx convention). When
/// neither is present, all requests share a single `direct` bucket;
/// that's the right behavior for a directly-exposed handler since you
/// can't distinguish callers anyway.
pub(crate) fn client_ip_for_rate_limit(headers: &HeaderMap, trust_proxy_headers: bool) -> String {
    if trust_proxy_headers {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = xff.split(',').next() {
                let trimmed = first.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
        if let Some(real_ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let trimmed = real_ip.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "direct".to_string()
}
