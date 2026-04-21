//! Authentication middleware.
//!
//! Resolves the tenant (via `Authorization: Bearer <key>`) and the agent
//! (via `ICP-Agent-Id`). Writes both into request extensions so downstream
//! handlers can extract them without re-parsing headers.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::agent::{AgentIdentifier, ApiKeyInfo, ApiKeyStore};
use crate::constants::headers;
use crate::errors::{ApiError, ApiErrorBody, ApiErrorPayload};

#[derive(Clone)]
pub struct AuthState {
    pub keys: ApiKeyStore,
    pub require_mandate: bool,
}

pub async fn auth_middleware(
    State(state): State<AuthState>,
    mut req: Request,
    next: Next,
) -> Result<Response, Response> {
    let bearer = extract_bearer(&req).map_err(into_response)?;
    let tenant = state
        .keys
        .lookup(&bearer)
        .ok_or_else(|| into_response(ApiError::AuthenticationFailed(
            "unknown API key".into(),
        )))?;

    let agent_id = req
        .headers()
        .get(headers::ICP_AGENT_ID)
        .and_then(|v| v.to_str().ok())
        .map(AgentIdentifier::parse)
        .ok_or_else(|| {
            into_response(ApiError::AuthenticationFailed(
                "ICP-Agent-Id header required".into(),
            ))
        })?;

    req.extensions_mut().insert(tenant);
    req.extensions_mut().insert(agent_id);

    Ok(next.run(req).await)
}

fn extract_bearer(req: &Request) -> Result<String, ApiError> {
    let value = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::AuthenticationFailed("Authorization header missing".into()))?;
    value
        .strip_prefix("Bearer ")
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::AuthenticationFailed("Authorization must be a Bearer token".into())
        })
}

fn into_response(err: ApiError) -> Response {
    err.into_response()
}

#[allow(dead_code)]
pub fn auth_failure(message: &str) -> (StatusCode, Json<ApiErrorBody>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiErrorBody {
            error: ApiErrorPayload {
                type_: "authentication_failed".into(),
                code: "authentication_failed".into(),
                message: message.into(),
                param: None,
                intent_id: None,
                retriable: false,
                docs_url: None,
            },
        }),
    )
}

#[allow(dead_code)]
pub fn tenant_from_ext(req: &Request) -> Option<ApiKeyInfo> {
    req.extensions().get::<ApiKeyInfo>().cloned()
}
