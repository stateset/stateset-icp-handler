//! ICP handler binary entry point.
//!
//! Boots the iCommerce engine, the intent service, and the HTTP + gRPC
//! surfaces. All routing, state construction, and serving lives in
//! `lib.rs` so the exact same code drives tests and production.

use std::net::SocketAddr;

use stateset_icp_handler::{build_app_state, config::Config, serve};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();

    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
    let env_filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true)
        .json()
        .init();

    info!("Starting StateSet ICP Handler...");

    let config = Config::load()?;
    info!(
        "ICP {} on http://{}:{} + grpc://{}:{} (engine={}, mandates={})",
        config.icp_version,
        config.host,
        config.port,
        config.grpc_host,
        config.grpc_port,
        if config.commerce_enabled {
            config.commerce_db_path.as_str()
        } else {
            "disabled"
        },
        if config.require_mandate {
            "required"
        } else {
            "optional"
        },
    );

    let http_addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let grpc_addr: SocketAddr = format!("{}:{}", config.grpc_host, config.grpc_port).parse()?;

    let state = build_app_state(&config).await?;
    serve(state, http_addr, grpc_addr).await
}
