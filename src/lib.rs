#![doc = include_str!("../README.md")]
#![recursion_limit = "512"]
// `tonic::Status` is large; boxing it through the gRPC layer is more churn
// than savings, matching the conventions of the sibling ACP/UCP handlers.
#![allow(clippy::result_large_err)]
// The README is embedded as crate docs; rustdoc's lazy-continuation rule
// over-fires on ordinary GFM list formatting.
#![allow(clippy::doc_lazy_continuation)]

pub mod agent;
pub mod auth;
pub mod commerce;
pub mod compat;
pub mod config;
pub mod constants;
pub mod discovery;
pub mod errors;
pub mod events;
pub mod grpc;
pub mod idempotency;
pub mod intent;
pub mod mandate;
pub mod mcp;
pub mod metrics;
pub mod middleware;
pub mod models;
pub mod openapi;
pub mod rate_limit;
pub mod receipts;
pub mod resolver;
pub mod scheduler;
pub mod service;
pub mod signing;
pub mod state_db;
pub mod state_store;
pub mod webhook;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::from_fn,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::agent::{AgentIdentifier, ApiKeyInfo, ApiKeyStore};
use crate::commerce::CommerceEngine;
use crate::config::Config;
use crate::constants::{headers, MAX_REQUEST_BODY_BYTES};
use crate::errors::ApiError;
use crate::models::IntentEnvelope;
use crate::service::{IcpService, IntentInput};
use crate::signing::ReceiptSigner;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<IcpService>,
    pub keys: ApiKeyStore,
    pub config: Arc<Config>,
}

/// Build the app state from configuration. Public so tests can construct
/// it without going through `main`.
pub async fn build_app_state(config: &Config) -> anyhow::Result<AppState> {
    config.validate_runtime()?;

    let engine = if config.commerce_enabled {
        match CommerceEngine::open(&config.commerce_db_path) {
            Ok(engine) => {
                info!(
                    "iCommerce engine initialized at {}",
                    config.commerce_db_path
                );
                Some(engine)
            }
            Err(err) => {
                warn!("failed to open iCommerce engine: {err}");
                None
            }
        }
    } else {
        info!("iCommerce engine disabled (COMMERCE_ENABLED=false)");
        None
    };

    let signer = match config.signing_key_pem_env.as_deref() {
        Some(pem) => ReceiptSigner::from_pkcs8_pem(&config.signing_kid, pem)
            .map_err(|e| anyhow::anyhow!("load ICP receipt signing key: {e}"))?,
        None => ReceiptSigner::generate(&config.signing_kid),
    };
    info!("ICP receipt signer ready (kid={}, alg=EdDSA)", signer.kid);

    let state_pool = state_db::open(&config.state_db_path)
        .map_err(|e| anyhow::anyhow!("open ICP state DB at `{}`: {e}", config.state_db_path))?;
    if config.state_db_path == ":memory:" {
        info!("ICP state DB opened in-memory (ephemeral)");
    } else {
        info!("ICP state persisted at {}", config.state_db_path);
    }

    let mut service = IcpService::new(config.clone(), engine, signer);
    service.mandates = crate::mandate::MandateLedger::with_pool(state_pool.clone());
    service.receipts = crate::receipts::ReceiptStore::with_pool(state_pool.clone());
    service.transactions = crate::state_store::TransactionStore::with_pool(state_pool.clone());
    service.subscriptions = crate::state_store::SubscriptionStore::with_pool(state_pool.clone());
    service.peer_quotes = crate::state_store::PeerQuoteStore::with_pool(state_pool.clone());
    service.idempotency = crate::idempotency::IdempotencyStore::with_pool(state_pool.clone());
    service.webhook_outbox = crate::webhook::WebhookOutbox::with_pool(state_pool.clone());
    service.webhook_subscribers = crate::webhook::SubscriberStore::with_pool(state_pool);

    let keys = load_api_keys(config)?;
    if keys.is_empty() {
        warn!(
            "no API keys loaded — all requests will fail. Set ICP_API_KEYS_JSON \
             or ICP_ENABLE_DEMO_KEYS=true for local development."
        );
    }

    Ok(AppState {
        service: Arc::new(service),
        keys,
        config: Arc::new(config.clone()),
    })
}

fn load_api_keys(config: &Config) -> anyhow::Result<ApiKeyStore> {
    if let Some(raw) = &config.api_keys_json {
        let keys: Vec<ApiKeyInfo> = serde_json::from_str(raw)?;
        return ApiKeyStore::try_new(keys);
    }
    if let Some(path) = &config.api_keys_file {
        let bytes = std::fs::read(path)?;
        let keys: Vec<ApiKeyInfo> = serde_json::from_slice(&bytes)?;
        return ApiKeyStore::try_new(keys);
    }
    if config.enable_demo_keys {
        info!("loading bundled demo API keys (ICP_ENABLE_DEMO_KEYS=true)");
        return Ok(ApiKeyStore::demo());
    }
    Ok(ApiKeyStore::default())
}

/// Build the HTTP router (public so integration tests can drive it in-memory).
pub fn build_router(state: AppState) -> Router {
    let cors = build_cors(&state.config.cors_allow_origins);
    let acp_enabled = state.config.acp_compat_enabled;
    let ucp_enabled = state.config.ucp_compat_enabled;
    let mcp_enabled = state.config.mcp_enabled;

    let mut router = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_handler))
        .route("/openapi.json", get(crate::openapi::openapi_json))
        .route("/docs", get(crate::openapi::docs_html))
        .route("/.well-known/icp", get(discovery_handler))
        .route("/.well-known/icp/jwks.json", get(jwks_handler))
        .route("/icp/v1/intents", post(submit_intent))
        .route("/icp/v1/transactions", get(list_transactions))
        .route("/icp/v1/transactions/:id", get(get_transaction))
        .route("/icp/v1/subscriptions", get(list_subscriptions))
        .route("/icp/v1/subscriptions/:id", get(get_subscription))
        .route("/icp/v1/peer_quotes", get(list_peer_quotes))
        .route("/icp/v1/peer_quotes/:id", get(get_peer_quote))
        .route("/icp/v1/receipts", get(list_receipts))
        .route("/icp/v1/receipts/:jti", get(get_receipt))
        .route("/icp/v1/mandates/:jti/usage", get(get_mandate_usage))
        .route("/icp/v1/events:stream", get(sse_events))
        .route("/icp/v1/webhook_deliveries", get(list_webhook_deliveries))
        .route("/icp/v1/webhook_deliveries/:id", get(get_webhook_delivery))
        .route(
            "/icp/v1/webhook_deliveries/:id/retry",
            post(retry_webhook_delivery),
        )
        .route(
            "/icp/v1/webhook_subscribers",
            post(create_webhook_subscriber).get(list_webhook_subscribers),
        )
        .route(
            "/icp/v1/webhook_subscribers/:id",
            get(get_webhook_subscriber)
                .patch(update_webhook_subscriber)
                .delete(delete_webhook_subscriber),
        )
        .route(
            "/icp/v1/webhook_subscribers/:id/disable",
            post(disable_webhook_subscriber),
        )
        .route(
            "/icp/v1/webhook_subscribers/:id/enable",
            post(enable_webhook_subscriber),
        );

    if acp_enabled {
        router = router
            .route(
                "/checkout_sessions",
                post(crate::compat::acp::create_session),
            )
            .route(
                "/checkout_sessions/:id",
                get(crate::compat::acp::get_session).post(crate::compat::acp::update_session),
            )
            .route(
                "/checkout_sessions/:id/complete",
                post(crate::compat::acp::complete_session),
            )
            .route(
                "/checkout_sessions/:id/cancel",
                post(crate::compat::acp::cancel_session),
            );
    }

    if ucp_enabled {
        router = router
            .route("/.well-known/ucp", get(crate::compat::ucp::discovery))
            .route(
                "/checkout-sessions",
                post(crate::compat::ucp::create_checkout),
            )
            .route(
                "/checkout-sessions/:id",
                get(crate::compat::ucp::get_checkout).put(crate::compat::ucp::update_checkout),
            )
            .route(
                "/checkout-sessions/:id/complete",
                post(crate::compat::ucp::complete_checkout),
            )
            .route(
                "/checkout-sessions/:id/cancel",
                post(crate::compat::ucp::cancel_checkout),
            );
    }

    if mcp_enabled {
        router = router.route("/mcp", post(crate::mcp::handle));
    }

    router
        .layer(from_fn(crate::middleware::icp_version_middleware))
        .layer(CompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn build_cors(origins: &[String]) -> CorsLayer {
    if origins.iter().any(|o| o == "*") {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let values = origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect::<Vec<_>>();
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(values))
            .allow_methods(Any)
            .allow_headers(Any)
    }
}

// --------------------------------------------------------------------------
// Handlers
// --------------------------------------------------------------------------

async fn root_handler() -> &'static str {
    "StateSet ICP Handler — Intelligent Commerce Protocol"
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Ops",
    responses(
        (status = 200, description = "Handler is alive. Returns service metadata and build info."),
    ),
)]
pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "stateset-icp-handler",
        "version": env!("CARGO_PKG_VERSION"),
        "icp_version": crate::constants::ICP_VERSION,
        "git_sha": option_env!("BUILD_GIT_SHA").unwrap_or("unknown"),
        "built_at": option_env!("BUILD_TIME").unwrap_or("unknown"),
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

#[utoipa::path(
    get,
    path = "/.well-known/icp",
    tag = "ICP Core",
    responses(
        (status = 200, description = "ICP discovery document — advertised intents, signing keys, capabilities, interop surfaces."),
    ),
)]
pub async fn discovery_handler(State(state): State<AppState>) -> impl IntoResponse {
    let doc = crate::discovery::build(&state.config, &state.service.signer);
    Json(doc)
}

#[utoipa::path(
    get,
    path = "/.well-known/icp/jwks.json",
    tag = "ICP Core",
    responses(
        (status = 200, description = "JWKS (Ed25519 verifying keys) used to verify receipt signatures."),
    ),
)]
pub async fn jwks_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "keys": [state.service.signer.jwk()] }))
}

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
    use crate::idempotency::{CachedResponse, IdempotencyStore, LookupOutcome};
    use axum::response::IntoResponse;

    // Pre-auth per-IP rate limit. Fires BEFORE bearer resolution so a
    // flood of fake API keys can't burn unbounded CPU on keystore
    // lookups + 401 logging. Keyed by `X-Forwarded-For` first segment
    // (production deployments are always behind a proxy that sets
    // this); falls back to a sentinel for the rare direct-connect
    // case so all unknown clients share a single bucket.
    let client_ip = client_ip_for_rate_limit(&headers);
    if let crate::rate_limit::RateLimitDecision::Denied {
        limit,
        retry_after_secs,
    } = state.service.pre_auth_limiter.check(&client_ip, None)
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

    // Per-tenant rate limit. Per-tenant override via the API key entry's
    // `rate_limit_per_minute` (a value of 0 disables); falls back to
    // the handler-wide config default.
    let rl_decision = state
        .service
        .rate_limiter
        .check(&tenant.tenant_id, tenant.rate_limit_per_minute);
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
            // Return a 429 with Retry-After + X-RateLimit-* headers.
            // ApiError::RateLimited carries the spec-aligned body.
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
    // *semantic* equivalence, not by raw byte equality (e.g. different
    // key order from the client serializer doesn't trigger a conflict).
    let canonical =
        serde_jcs::to_vec(&envelope).map_err(|e| ApiError::ProcessingError(format!("jcs: {e}")))?;
    let request_digest = IdempotencyStore::digest_request(&canonical);

    let tenant_id = tenant.tenant_id.clone();
    if let Some(key) = idempotency_key.as_deref() {
        let (outcome, cached) =
            state
                .service
                .idempotency
                .lookup(&tenant_id, key, &request_digest, chrono::Utc::now());
        match outcome {
            LookupOutcome::Replay => {
                let body = cached.expect("Replay always carries a cached response");
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
        "active" => Ok(SubscriptionStatus::Active),
        "paused" => Ok(SubscriptionStatus::Paused),
        "canceled" => Ok(SubscriptionStatus::Canceled),
        "past_due" => Ok(SubscriptionStatus::PastDue),
        other => Err(ApiError::InvalidRequest(format!(
            "unknown status filter '{other}' — expected one of active|paused|canceled|past_due"
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
    if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::InvalidRequest(
            "url must be a non-empty http(s):// string".into(),
        ));
    }
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
/// Same validation as create: URL must be `http(s)://`, secret must
/// be non-empty if supplied.
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
        if u.is_empty() || !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err(ApiError::InvalidRequest(
                "url must be a non-empty http(s):// string".into(),
            ));
        }
    }
    let secret_trimmed = body.secret.as_deref().map(str::trim);
    if let Some(s) = secret_trimmed {
        if s.is_empty() {
            return Err(ApiError::InvalidRequest(
                "secret must be non-empty — events are HMAC-signed and the receiver needs the same secret to verify".into(),
            ));
        }
    }

    // Tenant scope: cross-tenant ids 404 the same way `get`/`enable`/
    // `delete` do — existence is never confirmed across tenants.
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
    // Redact secret on response — same policy as get/list/enable.
    updated.secret = None;
    Ok(Json(serde_json::to_value(updated)?))
}

/// Soft-disable a subscriber — flips `active` to false. Future events
/// won't fan out to this URL, but the row remains in the store so the
/// operator can re-enable it via the matching `enable` endpoint.
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

/// Re-enable a previously-disabled subscriber. Idempotent — calling
/// it on an already-active subscriber is a no-op (returns the same
/// row). Tenant-scoped: cross-tenant ids 404. Critical operator
/// path because the alternative (delete + recreate) rotates the id
/// and the secret, breaking any downstream verifier configuration.
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
            // Tenant-derive via the backing transaction. Same
            // policy as `get_receipt`: receipts whose backing
            // transaction has been GC'd or pre-dates `tenant_id`
            // stamping (`tenant_id == ""`) are invisible to any
            // real tenant — conservative default that matches the
            // documented isolation behavior of the get endpoint.
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
    // jti space isn't enumerable across tenants. If the underlying
    // transaction has been GC'd or pre-dates `tenant_id` stamping,
    // the row's `tenant_id` is "" — invisible to any real bearer,
    // which is the desired conservative default.
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
    // Cross-tenant or never-spent jtis surface as 404 — same shape,
    // so jti space isn't enumerable.
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

#[utoipa::path(
    get,
    path = "/icp/v1/events:stream",
    tag = "ICP Core",
    responses(
        (status = 200, description = "Server-Sent Events stream of `transaction.*`, `subscription.*`, and `peer_quote.*` events.", content_type = "text/event-stream"),
    ),
)]
pub async fn sse_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<
    axum::response::Sse<
        impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    let tenant = resolve_tenant(&headers, &state.keys)?;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let tenant_id = tenant.tenant_id.clone();
    let service = state.service.clone();
    let rx = state.service.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |evt| match evt {
        Ok(e) => {
            if !service.event_belongs_to_tenant(&e, &tenant_id) {
                return None;
            }
            let data = serde_json::to_string(&e).unwrap_or_default();
            Some(Ok(axum::response::sse::Event::default()
                .id(e.id)
                .event(e.r#type)
                .data(data)))
        }
        Err(_) => None,
    });
    Ok(axum::response::Sse::new(stream))
}

fn resolve_tenant(headers: &HeaderMap, keys: &ApiKeyStore) -> Result<ApiKeyInfo, ApiError> {
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

fn resolve_agent(headers: &HeaderMap) -> Result<AgentIdentifier, ApiError> {
    let v = headers
        .get(headers::ICP_AGENT_ID)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::AuthenticationFailed("ICP-Agent-Id header required".into()))?;
    Ok(AgentIdentifier::parse(v))
}

fn ensure_agent_allowed(tenant: &ApiKeyInfo, agent: &AgentIdentifier) -> Result<(), ApiError> {
    if tenant.permits_agent(&agent.raw) {
        Ok(())
    } else {
        Err(ApiError::AuthenticationFailed(format!(
            "agent `{}` is not allowed for this API key",
            agent.raw
        )))
    }
}

fn stamp_receipt_headers(out_headers: &mut HeaderMap, body_json: &serde_json::Value) {
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

fn validate_webhook_url(url: &str, allow_insecure: bool) -> Result<(), ApiError> {
    if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::InvalidRequest(
            "url must be a non-empty http(s):// string".into(),
        ));
    }
    if !allow_insecure && !url.starts_with("https://") {
        return Err(ApiError::InvalidRequest(
            "url must use https:// when insecure URLs are disabled".into(),
        ));
    }
    Ok(())
}

/// Extract a client identifier suitable for the pre-auth rate-limit
/// bucket key. Reads `X-Forwarded-For` (the canonical proxy header) and
/// returns the first segment — the originating client IP per RFC 7239.
/// `X-Real-IP` is checked as a fallback (nginx convention). When
/// neither is present, all requests share a single `direct` bucket;
/// that's the right behavior for a directly-exposed handler since you
/// can't distinguish callers anyway.
fn client_ip_for_rate_limit(headers: &HeaderMap) -> String {
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
    "direct".to_string()
}

// --------------------------------------------------------------------------
// Serve
// --------------------------------------------------------------------------

/// Serve HTTP and gRPC concurrently. Returns once either shuts down.
pub async fn serve(
    state: AppState,
    http_addr: SocketAddr,
    grpc_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_router(state.clone());

    let http = async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        info!("HTTP listening on {http_addr}");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    };

    let grpc_handler = crate::grpc::GrpcHandler::new(state.service.clone(), state.keys.clone());
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(crate::grpc::proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<crate::grpc::proto::icp_handler_server::IcpHandlerServer<
            crate::grpc::GrpcHandler,
        >>()
        .await;

    let grpc = async move {
        info!("gRPC listening on {grpc_addr}");
        tonic::transport::Server::builder()
            .add_service(health_service)
            .add_service(reflection)
            .add_service(grpc_handler)
            .serve_with_shutdown(grpc_addr, shutdown_signal())
            .await
    };

    // Subscription auto-billing scheduler (opt-out via config). Runs
    // alongside HTTP and gRPC; cancels when either of those exits via
    // the surrounding `tokio::select!`.
    let scheduler_task = if state.config.subscription_scheduler_enabled {
        let svc = state.service.clone();
        let period = std::time::Duration::from_secs(
            state.config.subscription_scheduler_interval_secs.max(1),
        );
        Some(tokio::spawn(crate::scheduler::run_loop(svc, period)))
    } else {
        info!("subscription scheduler disabled (ICP_SUBSCRIPTION_SCHEDULER_ENABLED=false)");
        None
    };

    // Webhook delivery worker. Always runs so per-tenant subscribers
    // registered through the API can receive events even when no
    // global fallback URL is configured. Global fallback rows are only
    // enqueued when ICP_WEBHOOK_URL and ICP_WEBHOOK_SECRET are both set.
    if let Some(url) = state.config.webhook_url.as_deref() {
        if state.config.webhook_secret.is_some() {
            info!("webhook delivery worker enabled, global_target={url}");
        } else {
            warn!(
                "ICP_WEBHOOK_URL set but ICP_WEBHOOK_SECRET is missing — global fallback deliveries will not be enqueued"
            );
        }
    } else {
        info!("webhook delivery worker enabled for per-tenant subscribers");
    }
    let webhook_task = {
        let worker = crate::webhook::WebhookWorker::new_with_optional_secret(
            state.service.webhook_outbox.clone(),
            state.config.webhook_secret.clone(),
        )
        .with_subscribers(state.service.webhook_subscribers.clone())
        .with_retention(
            state.config.webhook_retain_delivered_days,
            state.config.webhook_retain_dead_lettered_days,
        );
        Some(tokio::spawn(crate::webhook::run_loop(
            worker,
            std::time::Duration::from_secs(crate::webhook::DEFAULT_TICK_SECS),
        )))
    };

    // Idempotency cache TTL sweeper — reclaims rows whose
    // `created_at + ttl < now`. Lazy TTL at lookup time already
    // prevents wrong replays; the active sweeper bounds disk usage.
    // Disable with `ICP_IDEMPOTENCY_SWEEPER_INTERVAL_SECONDS=0`.
    let idempotency_sweeper_task = if state.config.idempotency_sweeper_interval_secs > 0 {
        let interval =
            std::time::Duration::from_secs(state.config.idempotency_sweeper_interval_secs);
        info!(
            interval_secs = interval.as_secs(),
            "idempotency sweeper enabled"
        );
        Some(tokio::spawn(crate::idempotency::run_sweeper_loop(
            state.service.idempotency.clone(),
            interval,
        )))
    } else {
        info!("idempotency sweeper disabled (ICP_IDEMPOTENCY_SWEEPER_INTERVAL_SECONDS=0)");
        None
    };

    // Expiry sweeper — transitions stale quotes (transactions +
    // peer quotes) to the terminal `Expired` state once their
    // deadline passes. Default cadence 60s gives sub-minute
    // resolution on quote validity. Disable with
    // `ICP_EXPIRY_SWEEPER_INTERVAL_SECONDS=0`.
    let expiry_sweeper_task = if state.config.expiry_sweeper_interval_secs > 0 {
        let interval = std::time::Duration::from_secs(state.config.expiry_sweeper_interval_secs);
        info!(interval_secs = interval.as_secs(), "expiry sweeper enabled");
        Some(tokio::spawn(crate::scheduler::run_expiry_loop(
            state.service.clone(),
            interval,
        )))
    } else {
        info!("expiry sweeper disabled (ICP_EXPIRY_SWEEPER_INTERVAL_SECONDS=0)");
        None
    };

    let outcome = tokio::select! {
        res = http => res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
        res = grpc => res.map_err(|e| Box::new(e) as Box<dyn std::error::Error>),
    };

    if let Some(task) = scheduler_task {
        task.abort();
    }
    if let Some(task) = webhook_task {
        task.abort();
    }
    if let Some(task) = idempotency_sweeper_task {
        task.abort();
    }
    if let Some(task) = expiry_sweeper_task {
        task.abort();
    }

    outcome
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

// Pull in shared deps used by the HTTP router.
pub use axum::extract::Request as AxumRequest;
pub use axum::response::Response as AxumResponse;

#[allow(dead_code)]
fn _unused(_e: Extension<ApiKeyInfo>) {}
