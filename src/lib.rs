#![doc = include_str!("../README.md")]
#![recursion_limit = "512"]
// `tonic::Status` is large; boxing it through the gRPC layer is more churn
// than savings, matching the conventions of the sibling ACP/UCP handlers.
#![allow(clippy::result_large_err)]

pub mod agent;
pub mod auth;
pub mod commerce;
pub mod config;
pub mod constants;
pub mod discovery;
pub mod errors;
pub mod events;
pub mod grpc;
pub mod intent;
pub mod mandate;
pub mod metrics;
pub mod middleware;
pub mod models;
pub mod receipts;
pub mod service;
pub mod signing;
pub mod state_store;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{Extension, Path, State},
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

    let signer = ReceiptSigner::generate(&config.signing_kid);
    info!(
        "ICP receipt signer ready (kid={}, alg=EdDSA)",
        signer.kid
    );

    let service = IcpService::new(config.clone(), engine, signer);

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
        return Ok(ApiKeyStore::new(keys));
    }
    if let Some(path) = &config.api_keys_file {
        let bytes = std::fs::read(path)?;
        let keys: Vec<ApiKeyInfo> = serde_json::from_slice(&bytes)?;
        return Ok(ApiKeyStore::new(keys));
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

    Router::new()
        .route("/", get(root_handler))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics_handler))
        .route("/.well-known/icp", get(discovery_handler))
        .route("/.well-known/icp/jwks.json", get(jwks_handler))
        .route("/icp/v1/intents", post(submit_intent))
        .route(
            "/icp/v1/transactions/:id",
            get(get_transaction),
        )
        .route("/icp/v1/receipts/:jti", get(get_receipt))
        .route("/icp/v1/mandates/:jti/usage", get(get_mandate_usage))
        .route("/icp/v1/events:stream", get(sse_events))
        .layer(from_fn(crate::middleware::icp_version_middleware))
        .layer(CompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn build_cors(origins: &[String]) -> CorsLayer {
    if origins.iter().any(|o| o == "*") {
        CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)
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

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "stateset-icp-handler",
        "version": env!("CARGO_PKG_VERSION"),
        "icp_version": crate::constants::ICP_VERSION,
        "git_sha": option_env!("BUILD_GIT_SHA").unwrap_or("unknown"),
        "built_at": option_env!("BUILD_TIME").unwrap_or("unknown"),
    }))
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let engine_ok = state.service.engine.is_some();
    let body = serde_json::json!({
        "status": if engine_ok { "ready" } else { "degraded" },
        "engine": if engine_ok { "available" } else { "unavailable" },
    });
    (StatusCode::OK, Json(body))
}

async fn metrics_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        crate::metrics::encode(),
    )
}

async fn discovery_handler(State(state): State<AppState>) -> impl IntoResponse {
    let doc = crate::discovery::build(&state.config, &state.service.signer);
    Json(doc)
}

async fn jwks_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "keys": [state.service.signer.jwk()] }))
}

async fn submit_intent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(envelope): Json<IntentEnvelope>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Resolve tenant (bearer) + agent (ICP-Agent-Id) inline so that a single
    // endpoint can enforce ICP-specific auth semantics without an auth
    // middleware that does not know about intent scopes.
    let tenant = resolve_tenant(&headers, &state.keys)?;
    let agent = resolve_agent(&headers)?;

    let mandate_jws = headers
        .get(headers::ICP_MANDATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let request_id = headers
        .get(headers::ICP_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("req_{}", Uuid::new_v4().simple()));
    let trace_id = headers
        .get(headers::ICP_TRACE_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let input = IntentInput {
        envelope,
        agent,
        tenant,
        mandate_jws: mandate_jws.as_deref(),
        request_id,
        trace_id,
    };

    let started = Instant::now();
    let body = state.service.handle_intent(input).await?;
    crate::metrics::record_intent(body.intent.as_str(), "ok");
    crate::metrics::record_http(
        "/icp/v1/intents",
        200,
        started.elapsed().as_secs_f64(),
    );

    Ok(Json(serde_json::to_value(body)?))
}

async fn get_transaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _tenant = resolve_tenant(&headers, &state.keys)?;
    let txn = state
        .service
        .transactions
        .get(&id)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("transaction {id}")))?;
    Ok(Json(serde_json::to_value(txn)?))
}

async fn get_receipt(
    State(state): State<AppState>,
    Path(jti): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _tenant = resolve_tenant(&headers, &state.keys)?;
    let r = state
        .service
        .receipts
        .get(&jti)
        .ok_or_else(|| ApiError::ResourceNotFound(format!("receipt {jti}")))?;
    Ok(Json(serde_json::json!({
        "jti": r.jti,
        "kid": r.kid,
        "jws": r.jws,
        "body_digest": r.body_digest,
        "claims": r.claims,
    })))
}

async fn get_mandate_usage(
    State(state): State<AppState>,
    Path(jti): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _tenant = resolve_tenant(&headers, &state.keys)?;
    let usage = state.service.mandates.usage(&jti);
    Ok(Json(serde_json::json!({
        "jti": jti,
        "spent_minor": usage.spent_minor,
        "window_start": usage.window_start,
    })))
}

async fn sse_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::response::Sse<
    impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
>, ApiError> {
    let _tenant = resolve_tenant(&headers, &state.keys)?;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let rx = state.service.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|evt| match evt {
        Ok(e) => {
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

fn resolve_tenant(
    headers: &HeaderMap,
    keys: &ApiKeyStore,
) -> Result<ApiKeyInfo, ApiError> {
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::AuthenticationFailed("Bearer token required".into()))?;
    keys.lookup(bearer)
        .ok_or_else(|| ApiError::AuthenticationFailed("unknown API key".into()))
}

fn resolve_agent(headers: &HeaderMap) -> Result<AgentIdentifier, ApiError> {
    let v = headers
        .get(headers::ICP_AGENT_ID)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::AuthenticationFailed("ICP-Agent-Id header required".into()))?;
    Ok(AgentIdentifier::parse(v))
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

    tokio::select! {
        res = http => { res?; }
        res = grpc => { res?; }
    }
    Ok(())
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
