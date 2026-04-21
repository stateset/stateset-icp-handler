# Changelog

All notable changes to the StateSet ICP Handler are documented here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to date-based ICP versioning — see
[`docs/specification/ICP_SPEC.md` §16](./docs/specification/ICP_SPEC.md#16-versioning).

## [Unreleased]

## [0.1.0] — 2026-04-21 — Cornerstone

### Added
- **Intelligent Commerce Protocol specification**, version `2026-04-21`,
  published at [`docs/specification/ICP_SPEC.md`](./docs/specification/ICP_SPEC.md).
- Reference Rust handler:
  - `IcpService` intent router covering `intent.search`, `intent.describe`,
    `intent.quote`, `intent.authorize`, `intent.buy` / `intent.pay`,
    `intent.track`, `intent.return`, `intent.refund_request`.
  - `MandateLedger` for in-memory mandate decoding, scope/budget/window
    enforcement, and spend accrual.
  - Ed25519 `ReceiptSigner` producing compact-JWS receipts over
    JCS-canonicalized response bodies.
  - Embedded `stateset-icommerce` engine wrapper — persists buy-flow
    orders to SQLite (or PostgreSQL behind the `postgres` feature).
  - HTTP router on `:8082` (axum) with `/icp/v1/intents`, transactions,
    receipts, mandate usage, SSE event stream, discovery, JWKS, health,
    readiness, and Prometheus metrics.
  - gRPC server on `:50052` (tonic) with reflection and health.
  - Event bus (tokio broadcast) backing both SSE and gRPC streaming.
- Proto definition: `proto/icp_handler/v1/icp_handler.proto`.
- Docs: `architecture.md`, `getting-started.md`, `interop.md`,
  `agent-guide.md`, and the error table under
  `specification/errors.md`.
- Dockerfile (multi-stage, path-dep-aware) and `demo_test.sh` for an
  end-to-end walk through discovery → quote → authorize → buy.

### Known gaps (planned for 0.2)
- Mandate signature verification against resolvable principal DIDs.
- Full engine routing for tax, promotions, and shipping.
- `intent.subscribe`, `intent.renew`, `intent.pause`,
  `intent.cancel_subscription`.
- `intent.a2a_pay`, `intent.a2a_quote`.
- Wired ACP and UCP compatibility paths (currently advertised in
  discovery; compat handlers land in 0.2).
- MCP stdio tool surface.
- Language bindings mirroring the ACP/UCP handlers.
- Conformance test suite (`npx icp-conformance`).
