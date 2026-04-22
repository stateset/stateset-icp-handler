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
    /// When true, mandate JWS signatures are cryptographically verified
    /// against the principal's resolved keyset. When false, mandates are
    /// structurally decoded + scope/budget/window-checked but the
    /// signature is trusted (dev mode).
    ///
    /// Production default is `true` — disabling this in production lets a
    /// handler advertise ICP compliance while silently accepting
    /// `alg:none` mandates, which is a spec violation of §6.1 and a
    /// security hole. Only set to `false` for local development against
    /// `alg:none` test fixtures.
    pub verify_mandate_signatures: bool,

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
    /// SQLite path for handler-owned state: mandates, receipts,
    /// transactions, subscriptions, peer quotes. Separate from
    /// `commerce_db_path` because protocol-level state evolves
    /// independently of the commerce engine's schema. Use `:memory:` for
    /// ephemeral tests.
    pub state_db_path: String,
    /// Reserved for a future distributed mandate ledger. Read but not yet
    /// wired — SQLite persistence is the v0.1 story.
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

    // Subscription auto-billing scheduler
    pub subscription_scheduler_enabled: bool,
    pub subscription_scheduler_interval_secs: u64,
    /// Backoff schedule between failed renewal attempts, in hours.
    /// Each entry is the delay until the next try after the
    /// corresponding failure: `[1, 6, 24]` means wait 1h after the 1st
    /// failure, 6h after the 2nd, 24h after the 3rd; on the 4th
    /// failure (no schedule entry left) the subscription transitions
    /// to `past_due`. An empty schedule preserves the legacy "burn
    /// all retries in immediate succession then past_due" behavior —
    /// fine for tests, never appropriate in production.
    pub subscription_dunning_schedule_hours: Vec<u32>,

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
            verify_mandate_signatures: env_bool("ICP_VERIFY_MANDATE_SIGNATURES", true),
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
            state_db_path: env_default("ICP_STATE_DB_PATH", "./icp-state.db"),
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
            pre_auth_rate_limit_per_minute: env_default(
                "ICP_PRE_AUTH_RATE_LIMIT_PER_MINUTE",
                "120",
            )
            .parse()
            .unwrap_or(120),
            subscription_scheduler_enabled: env_bool("ICP_SUBSCRIPTION_SCHEDULER_ENABLED", true),
            subscription_scheduler_interval_secs: env_default(
                "ICP_SUBSCRIPTION_SCHEDULER_INTERVAL_SECS",
                "60",
            )
            .parse()
            .unwrap_or(60),
            subscription_dunning_schedule_hours: parse_dunning_schedule(&env_default(
                "ICP_SUBSCRIPTION_DUNNING_SCHEDULE_HOURS",
                "1,6,24",
            )),
            log_level: env_default("LOG_LEVEL", "info"),
        })
    }

    /// Build a config suitable for integration tests — env-independent,
    /// in-memory, demo keys on, mandate optional. Override fields as needed.
    pub fn for_test() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 0,
            grpc_host: "127.0.0.1".into(),
            grpc_port: 0,
            icp_version: ICP_VERSION.into(),
            require_icp_version: false,
            require_mandate: false,
            require_request_id: false,
            require_idempotency_key: false,
            verify_mandate_signatures: false,
            public_base_url: "http://127.0.0.1".into(),
            handler_id: "icp://test".into(),
            service_name: "ICP Test".into(),
            signing_key_pem_env: None,
            signing_kid: "icp-test-key".into(),
            allow_insecure_urls: true,
            cors_allow_origins: vec!["*".into()],
            api_keys_json: None,
            api_keys_file: None,
            enable_demo_keys: true,
            commerce_enabled: false,
            commerce_db_path: ":memory:".into(),
            transaction_ttl: Duration::from_secs(DEFAULT_TRANSACTION_TTL_SECS),
            state_db_path: ":memory:".into(),
            redis_url: None,
            webhook_url: None,
            webhook_secret: None,
            a2a_enabled: true,
            mcp_enabled: true,
            acp_compat_enabled: true,
            ucp_compat_enabled: true,
            rate_limit_per_minute: 10_000,
            pre_auth_rate_limit_per_minute: 10_000,
            // Tests drive `tick_subscriptions` directly for determinism;
            // the background loop is off unless the test opts in.
            subscription_scheduler_enabled: false,
            subscription_scheduler_interval_secs: 60,
            // Empty schedule preserves the legacy "fail fast then
            // past_due" behavior the existing scheduler tests assume.
            // Production / test_dunning opt in by setting this
            // explicitly.
            subscription_dunning_schedule_hours: vec![],
            log_level: "warn".into(),
        }
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

/// Parse a comma-separated list of u32 hours into a dunning schedule.
/// Empty input → empty schedule (legacy semantics). Invalid entries
/// (non-numeric, zero, > 8760) are silently dropped — operators get
/// the validated subset, never a panic at boot. A schedule of all
/// invalid entries collapses to empty.
fn parse_dunning_schedule(s: &str) -> Vec<u32> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<u32>().ok())
        .filter(|n| (1..=8760).contains(n))
        .collect()
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}
