#![doc = include_str!("../README.md")]
#![recursion_limit = "512"]
// `tonic::Status` is large; boxing it through the gRPC layer is more churn
// than savings, matching the conventions of the sibling ACP/UCP handlers.
#![allow(clippy::result_large_err)]
// The README is embedded as crate docs; rustdoc's lazy-continuation rule
// over-fires on ordinary GFM list formatting.
#![allow(clippy::doc_lazy_continuation)]

pub mod agent;
pub mod app;
pub mod auth;
pub mod commerce;
pub mod compat;
pub mod config;
pub mod constants;
pub mod discovery;
pub mod errors;
pub mod events;
pub mod grpc;
pub mod handlers;
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

// Re-exports — keep the historical public surface stable for tests, the
// binary, and downstream language clients.
pub use app::{build_app_state, build_router, serve, AppState};

// Re-export handler functions at the crate root so the OpenAPI derive
// (which uses `crate::*` paths) and any downstream code that grabs a
// handler by name continue to work.
pub use handlers::discovery::{discovery_handler, jwks_handler};
pub use handlers::events::sse_events;
pub use handlers::intents::submit_intent;
pub use handlers::ops::{health, metrics_handler, ready};
pub use handlers::queries::{
    get_mandate_usage, get_peer_quote, get_receipt, get_subscription, get_transaction,
    list_peer_quotes, list_receipts, list_subscriptions, list_transactions,
};
pub use handlers::webhook_admin::{
    create_webhook_subscriber, delete_webhook_subscriber, disable_webhook_subscriber,
    enable_webhook_subscriber, get_webhook_delivery, get_webhook_subscriber,
    list_webhook_deliveries, list_webhook_subscribers, retry_webhook_delivery,
    update_webhook_subscriber, CreateSubscriberBody, UpdateSubscriberBody,
};
