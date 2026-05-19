//! Application state, router construction, and the `serve` orchestrator.
//!
//! The same code drives tests (which build their own [`AppState`] with
//! `Config::for_test()`) and production (which goes through `main.rs`
//! and the env-loaded [`Config`]). Keeping construction here — out of
//! `lib.rs` — keeps the module index small and the routing surface
//! discoverable.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    http::HeaderValue,
    middleware::from_fn,
    routing::{get, post},
    Router,
};
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};
use tracing::{info, warn};

use crate::agent::{ApiKeyInfo, ApiKeyStore};
use crate::commerce::CommerceEngine;
use crate::config::Config;
use crate::constants::MAX_REQUEST_BODY_BYTES;
use crate::service::IcpService;
use crate::signing::ReceiptSigner;
use crate::state_db::StatePool;

/// Shared per-process state held by every HTTP handler.
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<IcpService>,
    pub keys: ApiKeyStore,
    pub config: Arc<Config>,
    /// Underlying SQLite pool for handler-owned protocol state. Held
    /// here (rather than only inside per-store handles) so observability
    /// endpoints — `/health` reports applied migration versions —
    /// don't need a back-channel into every store.
    pub state_pool: StatePool,
}

/// Build the app state from configuration. Public so tests can construct
/// it without going through `main`.
pub async fn build_app_state(config: &Config) -> anyhow::Result<AppState> {
    config.validate_runtime()?;
    if let Some(url) = config.webhook_url.as_deref() {
        crate::webhook::validate_destination_url(url, config.allow_insecure_urls)
            .map_err(|e| anyhow::anyhow!("ICP_WEBHOOK_URL: {e}"))?;
    }

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
                if config.is_production() {
                    return Err(anyhow::anyhow!(
                        "failed to open iCommerce engine at `{}`: {err}",
                        config.commerce_db_path
                    ));
                }
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

    let state_pool = crate::state_db::open(&config.state_db_path)
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
    service.webhook_subscribers = crate::webhook::SubscriberStore::with_pool(state_pool.clone());
    if let Some(redis_url) = config.redis_url.as_deref() {
        match crate::rate_limit::RedisRateLimiter::per_minute(
            redis_url,
            "icp:rate_limit:tenant",
            config.rate_limit_per_minute,
        )
        .await
        {
            Ok(limiter) => {
                service.distributed_rate_limiter = Some(limiter);
                info!("tenant rate limiting backed by Redis");
            }
            Err(err) if config.is_production() => {
                return Err(anyhow::anyhow!(
                    "connect REDIS_URL for tenant rate limiting: {err}"
                ));
            }
            Err(err) => {
                warn!("Redis tenant rate limiter unavailable; using local limiter: {err}");
            }
        }

        match crate::rate_limit::RedisRateLimiter::per_minute(
            redis_url,
            "icp:rate_limit:pre_auth",
            config.pre_auth_rate_limit_per_minute,
        )
        .await
        {
            Ok(limiter) => {
                service.distributed_pre_auth_limiter = Some(limiter);
                info!("pre-auth rate limiting backed by Redis");
            }
            Err(err) if config.is_production() => {
                return Err(anyhow::anyhow!(
                    "connect REDIS_URL for pre-auth rate limiting: {err}"
                ));
            }
            Err(err) => {
                warn!("Redis pre-auth rate limiter unavailable; using local limiter: {err}");
            }
        }
    }

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
        state_pool,
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
    use crate::handlers::{discovery, events, intents, ops, queries, webhook_admin};

    let cors = build_cors(&state.config.cors_allow_origins);
    let acp_enabled = state.config.acp_compat_enabled;
    let ucp_enabled = state.config.ucp_compat_enabled;
    let mcp_enabled = state.config.mcp_enabled;

    let mut router = Router::new()
        .route("/", get(ops::root_handler))
        .route("/health", get(ops::health))
        .route("/ready", get(ops::ready))
        .route("/metrics", get(ops::metrics_handler))
        .route("/openapi.json", get(crate::openapi::openapi_json))
        .route("/docs", get(crate::openapi::docs_html))
        .route("/.well-known/icp", get(discovery::discovery_handler))
        .route("/.well-known/icp/jwks.json", get(discovery::jwks_handler))
        .route("/icp/v1/intents", post(intents::submit_intent))
        .route("/icp/v1/transactions", get(queries::list_transactions))
        .route("/icp/v1/transactions/:id", get(queries::get_transaction))
        .route("/icp/v1/subscriptions", get(queries::list_subscriptions))
        .route("/icp/v1/subscriptions/:id", get(queries::get_subscription))
        .route("/icp/v1/peer_quotes", get(queries::list_peer_quotes))
        .route("/icp/v1/peer_quotes/:id", get(queries::get_peer_quote))
        .route("/icp/v1/receipts", get(queries::list_receipts))
        .route("/icp/v1/receipts/:jti", get(queries::get_receipt))
        .route(
            "/icp/v1/mandates/:jti/usage",
            get(queries::get_mandate_usage),
        )
        .route("/icp/v1/events:stream", get(events::sse_events))
        .route(
            "/icp/v1/webhook_deliveries",
            get(webhook_admin::list_webhook_deliveries),
        )
        .route(
            "/icp/v1/webhook_deliveries/:id",
            get(webhook_admin::get_webhook_delivery),
        )
        .route(
            "/icp/v1/webhook_deliveries/:id/retry",
            post(webhook_admin::retry_webhook_delivery),
        )
        .route(
            "/icp/v1/webhook_subscribers",
            post(webhook_admin::create_webhook_subscriber)
                .get(webhook_admin::list_webhook_subscribers),
        )
        .route(
            "/icp/v1/webhook_subscribers/:id",
            get(webhook_admin::get_webhook_subscriber)
                .patch(webhook_admin::update_webhook_subscriber)
                .delete(webhook_admin::delete_webhook_subscriber),
        )
        .route(
            "/icp/v1/webhook_subscribers/:id/disable",
            post(webhook_admin::disable_webhook_subscriber),
        )
        .route(
            "/icp/v1/webhook_subscribers/:id/enable",
            post(webhook_admin::enable_webhook_subscriber),
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
        .with_allow_insecure_urls(state.config.allow_insecure_urls)
        .with_retention(
            state.config.webhook_retain_delivered_days,
            state.config.webhook_retain_dead_lettered_days,
        );
        Some(tokio::spawn(crate::webhook::run_loop(
            worker,
            std::time::Duration::from_secs(crate::webhook::DEFAULT_TICK_SECS),
        )))
    };

    // Idempotency cache TTL sweeper.
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

    // Expiry sweeper — stale quotes → terminal Expired state.
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
