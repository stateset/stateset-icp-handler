//! Handler configuration.
//!
//! All configuration is loaded from environment variables (or a `.env` file).
//! The defaults are tuned for local development; see `.env.example` for the
//! production knobs.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::constants::{DEFAULT_TRANSACTION_TTL_SECS, ICP_VERSION};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    // Bind
    pub host: String,
    pub port: u16,
    pub grpc_host: String,
    pub grpc_port: u16,

    // Protocol
    pub icp_version: String,
    pub require_icp_version: bool,
    pub require_mandate: bool,
    pub require_request_id: bool,
    pub require_idempotency_key: bool,

    // Public URL + signing
    pub public_base_url: String,
    pub handler_id: String,
    pub service_name: String,
    pub signing_key_pem_env: Option<String>,
    pub signing_kid: String,
    pub allow_insecure_urls: bool,

    // CORS
    pub cors_allow_origins: Vec<String>,

    // Tenant API keys
    pub api_keys_json: Option<String>,
    pub api_keys_file: Option<String>,
    pub enable_demo_keys: bool,

    // Commerce engine
    pub commerce_enabled: bool,
    pub commerce_db_path: String,

    // State + sessions
    pub transaction_ttl: Duration,
    pub redis_url: Option<String>,

    // Outbound webhooks
    pub webhook_url: Option<String>,
    pub webhook_secret: Option<String>,

    // A2A + MCP + interop
    pub a2a_enabled: bool,
    pub mcp_enabled: bool,
    pub acp_compat_enabled: bool,
    pub ucp_compat_enabled: bool,

    // Rate limiting
    pub rate_limit_per_minute: u32,
    pub pre_auth_rate_limit_per_minute: u32,

    // Observability
    pub log_level: String,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let host = env_default("HOST", "0.0.0.0");
        let port: u16 = env_default("PORT", "8082")
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid PORT: {e}"))?;
        let grpc_host = env_default("GRPC_HOST", &host);
        let grpc_port: u16 = env_default("GRPC_PORT", "50052")
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid GRPC_PORT: {e}"))?;

        Ok(Self {
            host,
            port,
            grpc_host,
            grpc_port,
            icp_version: env_default("ICP_VERSION", ICP_VERSION),
            require_icp_version: env_bool("ICP_REQUIRE_VERSION", true),
            require_mandate: env_bool("ICP_REQUIRE_MANDATE", true),
            require_request_id: env_bool("ICP_REQUIRE_REQUEST_ID", false),
            require_idempotency_key: env_bool("ICP_REQUIRE_IDEMPOTENCY_KEY", false),
            public_base_url: env_default("ICP_PUBLIC_BASE_URL", "http://127.0.0.1:8082"),
            handler_id: env_default("ICP_HANDLER_ID", "icp://localhost"),
            service_name: env_default("ICP_SERVICE_NAME", "StateSet ICP Handler"),
            signing_key_pem_env: std::env::var("ICP_SIGNING_KEY_PEM").ok(),
            signing_kid: env_default("ICP_SIGNING_KID", "icp-receipt-2026-04"),
            allow_insecure_urls: env_bool("ICP_ALLOW_INSECURE_URLS", true),
            cors_allow_origins: split_csv(&env_default("ICP_CORS_ALLOW_ORIGINS", "*")),
            api_keys_json: std::env::var("ICP_API_KEYS_JSON").ok(),
            api_keys_file: std::env::var("ICP_API_KEYS_FILE").ok(),
            enable_demo_keys: env_bool("ICP_ENABLE_DEMO_KEYS", true),
            commerce_enabled: env_bool("COMMERCE_ENABLED", true),
            commerce_db_path: env_default("COMMERCE_DB_PATH", "./commerce.db"),
            transaction_ttl: Duration::from_secs(
                env_default("ICP_TRANSACTION_TTL_SECONDS", "")
                    .parse()
                    .unwrap_or(DEFAULT_TRANSACTION_TTL_SECS),
            ),
            redis_url: std::env::var("REDIS_URL").ok(),
            webhook_url: std::env::var("ICP_WEBHOOK_URL").ok(),
            webhook_secret: std::env::var("ICP_WEBHOOK_SECRET").ok(),
            a2a_enabled: env_bool("ICP_A2A_ENABLED", true),
            mcp_enabled: env_bool("ICP_MCP_ENABLED", true),
            acp_compat_enabled: env_bool("ICP_ACP_COMPAT_ENABLED", true),
            ucp_compat_enabled: env_bool("ICP_UCP_COMPAT_ENABLED", true),
            rate_limit_per_minute: env_default("ICP_RATE_LIMIT_PER_MINUTE", "300")
                .parse()
                .unwrap_or(300),
            pre_auth_rate_limit_per_minute: env_default("ICP_PRE_AUTH_RATE_LIMIT_PER_MINUTE", "120")
                .parse()
                .unwrap_or(120),
            log_level: env_default("LOG_LEVEL", "info"),
        })
    }
}

fn env_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}
