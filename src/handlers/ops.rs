//! Liveness, readiness, metrics, and the root banner.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use crate::AppState;

pub async fn root_handler() -> &'static str {
    "StateSet ICP Handler — Intelligent Commerce Protocol"
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Ops",
    responses(
        (status = 200, description = "Handler is alive. Returns service metadata, build info, and applied state-DB migration versions."),
    ),
)]
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "stateset-icp-handler",
        "version": env!("CARGO_PKG_VERSION"),
        "icp_version": crate::constants::ICP_VERSION,
        "git_sha": option_env!("BUILD_GIT_SHA").unwrap_or("unknown"),
        "built_at": option_env!("BUILD_TIME").unwrap_or("unknown"),
        // Lets operators sanity-check that a deploy actually rolled
        // the migration ladder forward instead of silently rolling
        // back to an old binary against a newer schema.
        "state_schema": {
            "applied": crate::state_db::applied_versions(&state.state_pool)
                .unwrap_or_default(),
            "expected": crate::state_db::MIGRATIONS
                .iter().map(|m| m.version).collect::<Vec<_>>(),
        },
    }))
}

#[utoipa::path(
    get,
    path = "/ready",
    tag = "Ops",
    responses(
        (status = 200, description = "`ready` when the commerce engine is mounted; `degraded` otherwise."),
    ),
)]
pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let engine_ok = state.service.engine.is_some();
    let body = serde_json::json!({
        "status": if engine_ok { "ready" } else { "degraded" },
        "engine": if engine_ok { "available" } else { "unavailable" },
    });
    (StatusCode::OK, Json(body))
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Ops",
    responses(
        (status = 200, description = "Prometheus exposition format (text/plain; version=0.0.4).", content_type = "text/plain"),
    ),
)]
pub async fn metrics_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        crate::metrics::encode(),
    )
}
