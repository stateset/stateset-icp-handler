//! ICP error taxonomy.
//!
//! Errors are modeled as the flat JSON structure defined in the ICP spec
//! (§12). `ApiError` implements `IntoResponse` so handlers can `?` into it.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub error: ApiErrorPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorPayload {
    /// High-level category per ICP §12.
    #[serde(rename = "type")]
    pub type_: String,
    /// Machine-readable short code (e.g. `mandate_out_of_scope`).
    pub code: String,
    /// Human-readable message (safe to surface to operators).
    pub message: String,
    /// RFC 9535 JSONPath pointing at the offending field, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    /// Whether the caller can retry without changes.
    pub retriable: bool,
    /// Documentation URL for this error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid_request: {0}")]
    InvalidRequest(String),

    #[error("authentication_failed: {0}")]
    AuthenticationFailed(String),

    #[error("mandate_invalid: {0}")]
    MandateInvalid(String),

    #[error("mandate_out_of_scope: {0}")]
    MandateOutOfScope(String),

    #[error("mandate_budget_exceeded: {0}")]
    MandateBudgetExceeded(String),

    #[error("intent_not_supported: {0}")]
    IntentNotSupported(String),

    #[error("resource_not_found: {0}")]
    ResourceNotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("idempotency_conflict: {0}")]
    IdempotencyConflict(String),

    #[error("precondition_failed: {0}")]
    PreconditionFailed(String),

    #[error("rate_limited")]
    RateLimited,

    #[error("engine_unavailable: {0}")]
    EngineUnavailable(String),

    #[error("processing_error: {0}")]
    ProcessingError(String),
}

impl ApiError {
    fn code_and_status(&self) -> (&'static str, &'static str, StatusCode) {
        match self {
            Self::InvalidRequest(_) => (
                "invalid_request",
                "invalid_request",
                StatusCode::BAD_REQUEST,
            ),
            Self::AuthenticationFailed(_) => (
                "authentication_failed",
                "authentication_failed",
                StatusCode::UNAUTHORIZED,
            ),
            Self::MandateInvalid(_) => (
                "mandate_invalid",
                "mandate_invalid",
                StatusCode::UNAUTHORIZED,
            ),
            Self::MandateOutOfScope(_) => (
                "mandate_out_of_scope",
                "mandate_out_of_scope",
                StatusCode::FORBIDDEN,
            ),
            Self::MandateBudgetExceeded(_) => (
                "mandate_budget_exceeded",
                "mandate_budget_exceeded",
                StatusCode::PAYMENT_REQUIRED,
            ),
            Self::IntentNotSupported(_) => (
                "intent_not_supported",
                "intent_not_supported",
                StatusCode::NOT_FOUND,
            ),
            Self::ResourceNotFound(_) => (
                "resource_not_found",
                "resource_not_found",
                StatusCode::NOT_FOUND,
            ),
            Self::Conflict(_) => ("conflict", "conflict", StatusCode::CONFLICT),
            Self::IdempotencyConflict(_) => {
                ("conflict", "idempotency_conflict", StatusCode::CONFLICT)
            }
            Self::PreconditionFailed(_) => (
                "precondition_failed",
                "precondition_failed",
                StatusCode::PRECONDITION_FAILED,
            ),
            Self::RateLimited => (
                "rate_limited",
                "rate_limited",
                StatusCode::TOO_MANY_REQUESTS,
            ),
            Self::EngineUnavailable(_) => (
                "engine_unavailable",
                "engine_unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            Self::ProcessingError(_) => (
                "processing_error",
                "processing_error",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidRequest(m)
            | Self::AuthenticationFailed(m)
            | Self::MandateInvalid(m)
            | Self::MandateOutOfScope(m)
            | Self::MandateBudgetExceeded(m)
            | Self::IntentNotSupported(m)
            | Self::ResourceNotFound(m)
            | Self::Conflict(m)
            | Self::IdempotencyConflict(m)
            | Self::PreconditionFailed(m)
            | Self::EngineUnavailable(m)
            | Self::ProcessingError(m) => m.clone(),
            Self::RateLimited => "Rate limit exceeded.".to_string(),
        }
    }

    fn retriable(&self) -> bool {
        matches!(self, Self::RateLimited | Self::EngineUnavailable(_))
    }

    pub fn into_body(self) -> (StatusCode, ApiErrorBody) {
        let (type_, code, status) = self.code_and_status();
        let message = self.message();
        let retriable = self.retriable();
        (
            status,
            ApiErrorBody {
                error: ApiErrorPayload {
                    type_: type_.to_string(),
                    code: code.to_string(),
                    message,
                    param: None,
                    intent_id: None,
                    retriable,
                    docs_url: Some(format!("https://docs.stateset.com/icp/errors/{code}")),
                },
            },
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_body();
        (status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        ApiError::ProcessingError(value.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        ApiError::InvalidRequest(value.to_string())
    }
}
