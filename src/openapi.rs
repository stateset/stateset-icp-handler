//! OpenAPI 3.1 schema for the ICP handler's HTTP surface.
//!
//! The schema is derived from `#[utoipa::path]` annotations on the axum
//! handlers and `#[derive(ToSchema)]` on the model types, so the
//! machine-readable contract is regenerated from source on every build
//! and can't drift from the running code. This is the "spec is the code,
//! the code is the spec" discipline that lets non-Rust implementers
//! generate clients mechanically instead of reverse-engineering
//! `src/models.rs`.
//!
//! Two routes expose the schema:
//!
//! * `GET /openapi.json` — the canonical machine-readable artifact. SDK
//!   generators (`openapi-generator`, `oapi-codegen`, Stainless) consume
//!   this directly.
//! * `GET /docs` — a CDN-loaded Swagger UI page for human browsing. The
//!   HTML is a few lines; no multi-megabyte UI assets are compiled into
//!   the binary. Operators who need offline docs can replace this route
//!   with their own embedded UI.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use utoipa::OpenApi;

use crate::constants::ICP_VERSION;

/// The authoritative OpenAPI document for the handler.
///
/// Paths and component schemas are added alongside the handler
/// annotations — keep new routes in sync by appending their path function
/// reference to `paths(...)` below.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "StateSet ICP Handler",
        description = "Reference implementation of the Intelligent Commerce Protocol \
            (ICP). This schema covers the HTTP surface only — gRPC is \
            described in `proto/icp_handler/v1/icp_handler.proto` and MCP \
            in the JSON-RPC 2.0 dispatch at `POST /mcp`.",
        license(name = "MIT OR Apache-2.0"),
        version = "2026-04-21",
    ),
    paths(
        crate::health,
        crate::ready,
        crate::metrics_handler,
        crate::discovery_handler,
        crate::jwks_handler,
        crate::submit_intent,
        crate::list_transactions,
        crate::get_transaction,
        crate::list_subscriptions,
        crate::get_subscription,
        crate::list_peer_quotes,
        crate::get_peer_quote,
        crate::list_webhook_deliveries,
        crate::get_webhook_delivery,
        crate::retry_webhook_delivery,
        crate::create_webhook_subscriber,
        crate::list_webhook_subscribers,
        crate::get_webhook_subscriber,
        crate::update_webhook_subscriber,
        crate::disable_webhook_subscriber,
        crate::enable_webhook_subscriber,
        crate::delete_webhook_subscriber,
        crate::list_receipts,
        crate::get_receipt,
        crate::get_mandate_usage,
        crate::sse_events,
    ),
    components(schemas(
        crate::models::ResponseEnvelope,
        crate::models::Money,
        crate::models::Address,
        crate::models::Buyer,
        crate::models::IntentEnvelope,
        crate::models::IntentContext,
        crate::models::TransactionState,
        crate::models::LineItem,
        crate::models::Totals,
        crate::models::Transaction,
        crate::models::IntentResponseBody,
        crate::models::OrderSummary,
        crate::models::ReceiptStub,
        crate::models::SearchParams,
        crate::models::DescribeParams,
        crate::models::QuoteParams,
        crate::models::RequestItem,
        crate::models::AuthorizeParams,
        crate::models::BuyParams,
        crate::models::PaymentInstrument,
        crate::models::TrackParams,
        crate::models::SubscriptionStatus,
        crate::models::BillingCadence,
        crate::models::Subscription,
        crate::models::SubscribeParams,
        crate::models::RenewParams,
        crate::models::SubscriptionRefParams,
        crate::models::PeerQuoteStatus,
        crate::models::A2aServiceKind,
        crate::models::A2aServiceSpec,
        crate::models::PeerQuote,
        crate::models::A2aQuoteParams,
        crate::models::A2aPayParams,
        crate::models::ReturnParams,
        crate::webhook::DeliveryStatus,
        crate::webhook::WebhookDelivery,
        crate::webhook::WebhookSubscriber,
        crate::CreateSubscriberBody,
        crate::UpdateSubscriberBody,
    )),
    tags(
        (name = "ICP Core", description = "First-class ICP intent pipeline (`/icp/v1/*`) and discovery (`/.well-known/icp`)."),
        (name = "Compat", description = "ACP + UCP compatibility surfaces (`/checkout_sessions*`, `/checkout-sessions*`)."),
        (name = "MCP", description = "Model Context Protocol JSON-RPC dispatch (`/mcp`)."),
        (name = "Ops", description = "Health, readiness, metrics, and schema endpoints."),
    ),
)]
pub struct ApiDoc;

pub fn json() -> String {
    ApiDoc::openapi()
        .to_pretty_json()
        .unwrap_or_else(|e| format!("{{\"error\":\"openapi render: {e}\"}}"))
}

pub async fn openapi_json() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json(),
    )
}

pub async fn docs_html() -> Response {
    // Swagger UI loaded from unpkg. Pinned to a major version so the
    // contract surface is stable; bump deliberately when upgrading.
    let body = format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>StateSet ICP Handler — API docs (ICP {version})</title>
  <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css" />
  <style>html,body,#ui{{margin:0;padding:0;height:100%}}</style>
</head>
<body>
  <div id="ui"></div>
  <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
  <script>
    window.ui = SwaggerUIBundle({{
      url: "/openapi.json",
      dom_id: "#ui",
      deepLinking: true,
      presets: [SwaggerUIBundle.presets.apis],
    }});
  </script>
</body>
</html>"##,
        version = ICP_VERSION,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}
