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

The compatibility paths are not yet wired in v0.1 but the spec-level mapping
is defined in [`docs/interop.md`](./docs/interop.md) and the handler is
structured so compatibility paths forward into the same intent service.

---

## Status (v0.1)

This is the **cornerstone release**: it establishes the spec, the wire
contract, and a working Rust handler skeleton that:

- ✅ Serves `/.well-known/icp` with accurate capability advertisement
- ✅ Publishes Ed25519 receipt signing keys at `/.well-known/icp/jwks.json`
- ✅ Validates mandates (scope, budget, window, merchant)
- ✅ Persists buy-flow orders into embedded iCommerce
- ✅ Emits signed receipts over JCS-canonicalized responses
- ✅ Streams transaction events via SSE + gRPC
- ✅ Exposes Prometheus metrics on `/metrics`

Since v0.1:

- ✅ **ACP compatibility path** — `/checkout_sessions/*` maps to ICP
  intents one-to-one (see `src/compat/acp.rs`).
- ✅ **UCP compatibility path** — `/checkout-sessions/*` + `/.well-known/ucp`
  (see `src/compat/ucp.rs`).
- ✅ **MCP surface** — `POST /mcp` serves JSON-RPC 2.0; every ICP intent
  is advertised as a discoverable tool (see `src/mcp.rs`).
- ✅ **MCP stdio transport** — second binary `icp-mcp-stdio` for Claude
  Desktop, Cursor, and other clients that spawn MCP servers as
  subprocesses (see [`docs/claude-desktop.md`](./docs/claude-desktop.md)).
- ✅ **Mandate signature verification** — real Ed25519 verification
  against `did:key` *and* `did:web` principals via a pluggable
  [`PrincipalResolver`](./src/resolver.rs) trait. On by default
  (`ICP_VERIFY_MANDATE_SIGNATURES=true`); set to `false` only for local
  development against `alg:none` test fixtures.
- ✅ **Subscriptions** — `intent.subscribe` / `renew` / `pause` /
  `cancel_subscription` shipped, with weekly/monthly/annual cadences,
  charge-on-subscribe, signed receipts on every state change, and an
  **automatic billing scheduler** ([`src/scheduler.rs`](./src/scheduler.rs))
  that runs renewals on a tokio interval. Three consecutive scheduler
  failures transition a sub to `past_due`; manual `intent.renew` clears
  the failure counter and reactivates.
- ✅ **A2A peer commerce** — `intent.a2a_quote` and `intent.a2a_pay`
  let agents transact directly. Pay-against-quote consumes a `PeerQuote`
  and produces a signed receipt; direct-pay skips the quote step.
  Production `external_required` mode requires
  `payment.method=external_authorization` and records the settlement
  reference on the transaction. Mandate scope `pay_peer` enforced.
- ✅ **Distributed rate limiting** — local fixed-window counters for
  development/tests, with Redis-backed per-tenant and pre-auth counters
  for production fleets (`REDIS_URL` required when production limits are
  enabled).
- ✅ **`icp-conformance` harness** — implementation-independent test
  suite that validates any ICP handler URL against the spec
  (see `src/bin/icp_conformance.rs`). Run
  `./demo_conformance.sh` for a one-command self-test.
- ✅ CI on every push/PR: `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test`, Docker build.
- ✅ 110+ integration tests locking in ICP, ACP, UCP, MCP (HTTP and
  stdio), `did:key` and `did:web` mandate verification, engine
  persistence, subscription lifecycle, scheduler-driven auto-billing,
  and A2A peer-commerce wire contracts.

15 of 17 catalog intents are now implemented end-to-end; only
`intent.negotiate` and `intent.confirm_receipt` remain.

Planned for v0.3+:

- [ ] `intent.negotiate`, `intent.confirm_receipt` (catalog completion)
- [ ] Additional DID resolver methods (`did:stateset:buyer`)
- [ ] Full engine routing for tax, promotions, shipping
- [ ] Subscription dunning, prorated mid-cycle changes, trial periods
- [ ] Language bindings (Node, Python, Go) — mirrored from the ACP/UCP
  handlers
- [ ] Publish `icp-conformance` as `npx icp-conformance` (Node shim)

Consult [`CHANGELOG.md`](./CHANGELOG.md) for the release history.

---

## License

Dual-licensed under MIT or Apache-2.0 at your option.
