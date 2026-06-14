# StateSet ICP Handler

**Reference implementation of the Intelligent Commerce Protocol (ICP).**

ICP is StateSet's open protocol for **agent-native commerce**: one wire
contract that lets any autonomous agent discover merchants, quote baskets,
authorize payment, complete checkout, track fulfillment, and handle returns —
across currencies, jurisdictions, and payment rails — under a verifiable
mandate.

This repository ships:

1. **[`docs/specification/ICP_SPEC.md`](./docs/specification/ICP_SPEC.md)** —
   the protocol specification (version `2026-04-21`).
2. A **standalone Rust handler** (this crate) that terminates ICP over HTTP
   + gRPC and executes every intent against an embedded
   [**StateSet iCommerce**][icommerce] engine.
3. **Compatibility surfaces** for the sibling protocols: ACP (OpenAI
   Agentic Commerce), UCP (Universal Commerce Protocol), MCP (Model Context
   Protocol), and Google A2A.

> **Positioning.** ACP terminates ChatGPT checkout. UCP provides platform-
> neutral checkout interop. **ICP subsumes both**: it is the superset spec
> that a globally-distributed fleet of agents can target as a single, stable
> contract — with the engine of record sitting *inside* the handler process.

[icommerce]: https://github.com/stateset/stateset-icommerce

---

## Why another protocol?

| Existing protocol | What it covers | What it leaves out |
|---|---|---|
| **ACP** (OpenAI, 2025) | ChatGPT Instant Checkout — session create, update, complete + delegated payment vault tokens. | Agent identity, mandates, budgets, negotiation, returns, subscriptions, stablecoin payments, global jurisdictions, peer (A2A) commerce. |
| **UCP** (StateSet, 2026-01) | Platform-neutral checkout interop with discovery, tokenization, OAuth identity linking, AP2 mandates, order webhooks. | Negotiation, declarative intent model, verifiable receipts, embedded engine, peer-agent payments, first-class global commerce. |
| **MCP** (Anthropic) | Agent tool surface (stdio JSON-RPC). | Commerce semantics — MCP tools are untyped for the commerce lifecycle. |
| **A2A** (Google) | Agent-to-agent task protocol. | Commerce semantics. |

**ICP** is designed to be the protocol an agent speaks when it is doing
commerce *as a primary intent*. It is:

- **Agent-first** — every request carries an identified agent + a signed
  mandate. No anonymous writes.
- **Intent-based** — the API is a small, stable set of verbs
  (`intent.quote`, `intent.buy`, `intent.return`, …) rather than
  merchant-specific CRUD.
- **Verifiable** — every state-changing response is a compact JWS receipt
  over the JCS-canonicalized body. Any party can verify offline.
- **Global** — multi-currency (fiat + stablecoin), multi-jurisdiction tax,
  cross-border fulfillment as first-class.
- **Embedded engine** — the handler carries the full iCommerce engine in
  process. No separate database, no control plane, no network hop between
  protocol and execution.
- **Interoperable** — one handler can expose ICP as the native surface and
  speak ACP, UCP, MCP, and A2A on compatibility paths that route into the
  same transaction/receipt pipeline.

See the full spec in [`docs/specification/ICP_SPEC.md`](./docs/specification/ICP_SPEC.md).

---

## Architecture

```text
                 ┌──────────────────────────────────────────────────────┐
                 │                    ICP Handler (Rust)                │
                 │                                                      │
   Agents ──┬──▶ │  HTTP  /icp/v1/intents  ──┐                           │
            │    │  gRPC  icp_handler.v1     │                           │
            │    │  SSE   /icp/v1/events    ─┤     ┌──────────────────┐  │
            │    │  MCP   /mcp             ──┼──▶  │   IcpService     │  │
            │    │  A2A   /a2a/v1          ──┘     │  (intent router) │  │
            │    │                                 │                  │  │
            │    │  Compat:                        │   ▸ mandate      │  │
            ├──▶ │  ACP   /checkout_sessions  ───▶ │   ▸ quote        │  │
            │    │  UCP   /api/checkout-…     ───▶ │   ▸ authorize    │  │
            │    │                                 │   ▸ buy/pay      │  │
            │    │                                 │   ▸ return       │  │
            │    │                                 │   ▸ track        │  │
            │    │                                 └─────────┬────────┘  │
            │    │                                           │           │
            │    │                                   ┌───────▼────────┐  │
            │    │                                   │ stateset-      │  │
            │    │                                   │  icommerce     │  │
            │    │                                   │ (embedded)     │  │
            │    │                                   │ SQLite / PG    │  │
            │    │                                   └────────────────┘  │
            │    │                                                        │
            │    │  Receipts → Ed25519 JWS  → /.well-known/icp/jwks.json  │
            └────┘                                                        │
                 └──────────────────────────────────────────────────────┘
```

---

## Quickstart

```bash
# 1. Clone the handler
git clone https://github.com/stateset/stateset-icp-handler

# 2. Build
cd stateset-icp-handler
cargo build --release

# 3. Run a local demo (demo API key, relaxed request headers)
ICP_ENABLE_DEMO_KEYS=true \
ICP_REQUIRE_VERSION=false \
ICP_REQUIRE_MANDATE=false \
ICP_REQUIRE_REQUEST_ID=false \
ICP_REQUIRE_IDEMPOTENCY_KEY=false \
cargo run --release

# HTTP  → http://0.0.0.0:8082
# gRPC  → 0.0.0.0:50052
```

The demo script runs a full agent flow end-to-end:

```bash
./demo_test.sh
```

---

## Minimal intent flow

1. Agent fetches discovery:
   ```bash
   curl -s http://localhost:8082/.well-known/icp | jq
   ```
2. Agent quotes a basket:
   ```bash
   curl -s -X POST http://localhost:8082/icp/v1/intents \
     -H "Authorization: Bearer icp_demo_key_123" \
     -H "ICP-Agent-Id: did:stateset:agent:demo" \
     -H "ICP-Version: 2026-04-21" \
     -H "ICP-Request-Id: req_demo_quote_1" \
     -H "ICP-Idempotency-Key: idem_demo_quote_1" \
     -H "ICP-Mandate: <compact-jws-mandate>" \
     -H "Content-Type: application/json" \
     -d '{
       "intent": "intent.quote",
       "agent_id": "did:stateset:agent:demo",
       "params": {
         "items": [
           { "sku": "WIDGET-001", "quantity": 2,
             "unit_price_hint": { "amount_minor": 2999, "currency": "USD" } }
         ]
       },
       "context": { "currency": "USD", "jurisdiction": "US-CA" }
     }'
   ```
3. Agent authorizes + buys against the returned `transaction_id`.
4. Each state-changing response carries a signed receipt in the
   `ICP-Receipt` header and inline under `receipt.jws`.

See [`docs/getting-started.md`](./docs/getting-started.md) for the full
walkthrough including mandate construction.

---

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/.well-known/icp` | Discovery document (capabilities, keys, interop). |
| `GET` | `/.well-known/icp/jwks.json` | Receipt signing keys (EdDSA). |
| `POST` | `/icp/v1/intents` | Submit any ICP intent. |
| `GET` | `/icp/v1/transactions/:id` | Retrieve a transaction aggregate. |
| `GET` | `/icp/v1/receipts/:jti` | Retrieve a receipt. |
| `GET` | `/icp/v1/mandates/:jti/usage` | Inspect mandate spend/remaining budget. |
| `GET` | `/icp/v1/events:stream` | Server-Sent Events stream of transaction events. |
| `GET` | `/health`, `/ready`, `/metrics` | Ops endpoints. |

gRPC: `icp_handler.v1.IcpHandler` on `:50052`, with proto at
[`proto/icp_handler/v1/icp_handler.proto`](./proto/icp_handler/v1/icp_handler.proto).

---

## Configuration

All knobs are environment variables. See
[`.env.example`](./.env.example) for the full list.

| Variable | Default | Notes |
|---|---|---|
| `HOST` / `PORT` | `0.0.0.0` / `8082` | HTTP bind. |
| `GRPC_HOST` / `GRPC_PORT` | `0.0.0.0` / `50052` | gRPC bind. |
| `ICP_REQUIRE_MANDATE` | `true` | Reject writes without a mandate JWS. |
| `ICP_ENABLE_DEMO_KEYS` | `false` | Bundle `icp_demo_key_123` on boot. |
| `COMMERCE_ENABLED` | `true` | Open the embedded iCommerce engine. |
| `COMMERCE_DB_PATH` | `./commerce.db` | SQLite path (or `postgres://…` with `--features postgres`). |
| `ICP_A2A_ENABLED` | `true` | Advertise A2A compatibility. |
| `ICP_MCP_ENABLED` | `true` | Advertise MCP compatibility. |
| `ICP_ACP_COMPAT_ENABLED` | `true` | Advertise ACP compatibility (path `/checkout_sessions`). |
| `ICP_UCP_COMPAT_ENABLED` | `true` | Advertise UCP compatibility (path `/ucp`). |
| `ICP_SIGNING_KID` | `icp-receipt-2026-04` | JWKS key id advertised for receipts. |

---

## Interoperability

ICP is a superset of ACP and UCP. The mapping is normative:

| ACP / UCP surface | ICP intent |
|---|---|
| `POST /checkout_sessions` | `intent.quote` (if items only) → `intent.authorize` (if buyer + address supplied) |
| `POST /checkout_sessions/:id/complete` | `intent.buy` |
| `POST /checkout_sessions/:id/cancel` | `intent.return` (canceled-before-fulfillment) |
| `POST /agentic_commerce/delegate_payment` | `intent.buy` with `PaymentInstrument::DelegatedVault` |
| `POST /api/checkout-sessions` (UCP) | `intent.quote` + `intent.authorize` |
| `POST /api/checkout-sessions/:id/complete` (UCP) | `intent.buy` |

The compatibility paths are live: ACP at `/checkout_sessions/*`, UCP at
`/checkout-sessions/*` + `/.well-known/ucp`. The spec-level mapping is
documented in [`docs/interop.md`](./docs/interop.md); both routes forward
into the same intent pipeline so receipts, mandate enforcement, and
audit trails are identical across surfaces.

---

## Status (v0.4)

All 17 catalog intents are implemented end-to-end and exercised by the
test suite. The handler:

- ✅ Serves `/.well-known/icp` with accurate capability advertisement
- ✅ Publishes Ed25519 receipt signing keys at `/.well-known/icp/jwks.json`
- ✅ Validates mandates (scope, budget, window, merchant) with real
  Ed25519 signature verification against `did:key` and `did:web`
  principals via the pluggable
  [`PrincipalResolver`](./src/resolver.rs) trait
- ✅ Persists buy-flow orders into embedded iCommerce, with the
  protocol-level state (mandates, receipts, transactions, subscriptions,
  peer quotes, idempotency cache, webhook outbox) on a separate SQLite
  database so the two schemas evolve independently
- ✅ Emits signed receipts over JCS-canonicalized responses
- ✅ Streams transaction events via SSE + gRPC
- ✅ Exposes Prometheus metrics on `/metrics` and an OpenAPI 3.1
  document at `/openapi.json` (+ `/docs` viewer)
- ✅ Enforces tenant-scoped reads across transactions, subscriptions,
  peer quotes, idempotency, receipts, and ACP/UCP compat sessions
- ✅ Routes compatibility traffic through the same pipeline as native
  ICP: ACP `/checkout_sessions/*`, UCP `/checkout-sessions/*`,
  MCP `POST /mcp` (HTTP) and the `icp-mcp-stdio` binary for Claude
  Desktop / Cursor (see [`docs/claude-desktop.md`](./docs/claude-desktop.md))
- ✅ Runs an automatic subscription billing scheduler with
  weekly/monthly/annual cadences, charge-on-subscribe, dunning that
  transitions a subscription to `past_due` after three consecutive
  failures, and manual `intent.renew` recovery
- ✅ Supports A2A peer commerce (`intent.a2a_quote`, `intent.a2a_pay`)
  with production `external_required` mode that gates settlement on a
  `payment.method=external_authorization` block carrying the provider,
  authorization id, and optional instrument hint
- ✅ Implements `intent.negotiate` (counter-offer + revised quote) and
  `intent.confirm_receipt` (buyer-side fulfillment acknowledgement)
- ✅ Distributed rate limiting via Redis for both per-tenant and
  per-IP pre-auth buckets, with production validation that refuses to
  silently fall back to a local limiter when `REDIS_URL` is configured
- ✅ Durable webhook outbox with exponential backoff, dead-lettering,
  SSRF-guarded destination validation, and HMAC-SHA256 signatures
  (Stripe-style `t=…,v1=…`) — synchronous enqueue so the response
  body is never returned before the event is committed
- ✅ Idempotency cache per ICP §13: replay on byte-equivalent retry,
  409 `idempotency_conflict` on divergent body, with a TTL sweeper
- ✅ Conformance harness (`icp-conformance` binary, also published as
  `npx @stateset/icp-conformance`) that validates any handler URL
  against the spec — see `./demo_conformance.sh`
- ✅ CI on every push/PR: `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test`, `cargo deny check`, Docker build, MSRV (1.85) gate
- ✅ 348 tests across 36 integration files plus property tests via
  proptest — see `tests/`

Language clients ship from this repo:

- **Rust** — this crate
- **Python** — [`clients/python/stateset_icp`](./clients/python)
  (published as `stateset-icp` on PyPI)
- **Go** — [`clients/go/stateset-icp-go`](./clients/go/stateset-icp-go)
- **TypeScript / npm** — [`@stateset/icp-conformance`](./clients/npm/icp-conformance)
  and [`@stateset/create-icp-commerce`](./clients/npm/create-icp-commerce)

On the roadmap toward v1.0:

- [ ] Additional DID resolver methods (`did:stateset:buyer`)
- [ ] Full engine routing for tax, promotions, shipping
- [ ] Subscription mid-cycle proration

Recently shipped: subscription free trials (`intent.subscribe` `trial_days`)
and cross-instance idempotency (database reservations so multiple replicas
sharing one store can't double-charge). Consult [`CHANGELOG.md`](./CHANGELOG.md)
for the full release history.

---

## License

Dual-licensed under MIT or Apache-2.0 at your option.
