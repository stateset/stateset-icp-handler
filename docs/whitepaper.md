# The StateSet ICP Handler

**A reference implementation of the Intelligent Commerce Protocol for agent-native commerce**

Version: 0.3.0 (handler) / `2026-04-21` (protocol)
Status: Published — tag `v0.3.0`
Authors: StateSet Commerce Engineering

---

## Abstract

The **StateSet ICP Handler** is a single-process Rust server that terminates the **Intelligent Commerce Protocol (ICP)** and executes every accepted intent against an embedded copy of the StateSet iCommerce engine. Where existing agentic-commerce protocols solve fragments of the problem — ACP terminates ChatGPT checkout, UCP standardizes platform-neutral checkout interop, MCP exposes generic tool surfaces, and A2A defines agent-to-agent task semantics — ICP is designed as the *superset* contract that an autonomous fleet of agents can target as a single, stable wire format. The handler ships HTTP, gRPC, SSE, MCP (HTTP and stdio), and A2A transports; verifies signed mandates against a pluggable principal resolver; emits Ed25519-signed JWS receipts over JCS-canonicalized response bodies; and maintains a persistent SQLite ledger for mandate spend, receipts, transactions, subscriptions, and peer quotes. This paper describes the protocol, the architectural choices behind the reference handler, and the security and conformance properties they yield.

---

## 1. Motivation

Commerce on the open web was designed for browsers driving HTML forms and, later, REST APIs invoked by first-party mobile clients. Autonomous agents are a third client class with different requirements:

1. **Identity at every hop.** A merchant cannot evaluate "is this charge authorized?" without knowing *which* agent is acting on whose behalf, under what budget, with what scope, and until when. HTTP basic-auth and bearer keys identify *tenants*, not principals.
2. **Verifiable outcomes.** When an agent reports back to a buyer that "the order is placed," the buyer's runtime must be able to prove this independently — without re-querying the merchant — for audit, dispute resolution, or downstream automation.
3. **Stable contracts across merchants.** An agent that talks to a hundred merchants cannot integrate against a hundred bespoke REST schemas. It needs a small, declarative verb catalog whose semantics are normative.
4. **Global commerce by default.** Currency conversion, jurisdictional tax, cross-border fulfillment, and stablecoin settlement are first-order concerns, not extensions.
5. **Returns, subscriptions, peer-to-peer.** The interesting commerce primitives go far beyond one-shot card capture. Any honest agent contract must cover the full commerce lifecycle.

The Intelligent Commerce Protocol is StateSet's response. The handler in this repository is its reference implementation.

### 1.1 Relationship to neighboring protocols

| Protocol | Scope | Gap that ICP closes |
|---|---|---|
| **ACP** (OpenAI, 2025) | ChatGPT Instant Checkout: session create/update/complete, delegated payment vault tokens | No agent identity, no mandates, no negotiation, no returns, no subscriptions, no stablecoins, no peer commerce |
| **UCP** (StateSet, 2026-01) | Platform-neutral checkout interop, discovery, OAuth identity linking, AP2 mandate passthrough | No declarative intent model, no first-class verifiable receipts, no embedded engine, no peer-agent payments |
| **MCP** (Anthropic) | Stdio/HTTP JSON-RPC tool surface for agent runtimes | Untyped for commerce semantics — every merchant invents its own tool schema |
| **A2A** (Google) | Agent-to-agent task protocol | No commerce semantics |
| **AP2** | Agent payment authorization mandates | Authorization only; no end-to-end commerce flow |
| **x402** | Agent payment payloads over HTTP 402 | Payment semantics only |

ICP subsumes ACP and UCP at the wire level — the handler exposes both as compatibility paths that route into the same intent pipeline — and integrates AP2, x402, MCP, and A2A as either compatibility surfaces or named extensions.

---

## 2. Design principles

The protocol and the handler share a deliberately small set of axioms.

1. **Agent-first.** Every request carries an identified agent (`ICP-Agent-Id`) and, on writes, a verifiable mandate (`ICP-Mandate`). There are no anonymous principals.
2. **Intent over CRUD.** Agents declare *what they want*. The handler realizes the intent against policy, inventory, pricing, and fulfillment constraints. Low-level CRUD endpoints exist only on platform/admin surfaces.
3. **Deterministic execution.** Given the same inputs and the same engine state, two handlers produce equivalent results. Pricing, tax, discount, and inventory reservation are side-effect-pure at the API boundary.
4. **Verifiable by default.** Every state-changing response is signed. Clients verify receipts without calling back into the handler.
5. **Global from day one.** Multi-currency (fiat and stablecoin), multi-jurisdiction tax, cross-border fulfillment, and locale-aware messaging are first-class.
6. **Embedded engine.** The reference handler carries the full commerce engine in-process. No external database, no remote control plane, no network hop between protocol and execution.
7. **Safe autonomy.** Mandates carry explicit budget, scope, and temporal bounds. Writes without an authorizing mandate are rejected — even if the agent's API key would otherwise allow them.
8. **Small core, typed extensions.** The core is ~17 intents and ~10 resources. Everything else is a typed extension advertised in discovery.

---

## 3. System architecture

The handler is a **single Rust binary** that hosts five things in one process:

```
                 ┌──────────────────────────────────────────────────────┐
                 │                    ICP Handler (Rust)                │
                 │                                                      │
   Agents ──┬──▶ │  HTTP  /icp/v1/intents  ──┐                          │
            │    │  gRPC  icp_handler.v1     │                          │
            │    │  SSE   /icp/v1/events    ─┤    ┌──────────────────┐  │
            │    │  MCP   /mcp             ──┼──▶ │   IcpService     │  │
            │    │  A2A   /a2a/v1          ──┘    │  (intent router) │  │
            │    │                                │                  │  │
            │    │  Compat:                       │   ▸ mandate      │  │
            ├──▶ │  ACP   /checkout_sessions  ───▶│   ▸ quote        │  │
            │    │  UCP   /api/checkout-…     ───▶│   ▸ authorize    │  │
            │    │                                │   ▸ buy/pay      │  │
            │    │                                │   ▸ return       │  │
            │    │                                │   ▸ track        │  │
            │    │                                └────────┬─────────┘  │
            │    │                                         │            │
            │    │                                ┌────────▼─────────┐  │
            │    │                                │ stateset-        │  │
            │    │                                │  icommerce       │  │
            │    │                                │ (embedded)       │  │
            │    │                                │ SQLite / PG      │  │
            │    │                                └──────────────────┘  │
            │    │                                                      │
            │    │  Receipts → Ed25519 JWS  → /.well-known/icp/jwks.json│
            └────┘                                                      │
                 └──────────────────────────────────────────────────────┘
```

The five hosted components:

1. **HTTP server** (axum) on `:8082`, terminating the canonical ICP REST surface plus all compatibility paths.
2. **gRPC server** (tonic) on `:50052`, carrying `icp_handler.v1.IcpHandler` with the same intent semantics over proto.
3. **Embedded commerce engine** (`stateset-embedded`) backed by SQLite by default and PostgreSQL behind the `postgres` Cargo feature.
4. **In-process event bus** (tokio broadcast) consumed by SSE on `/icp/v1/events:stream` and by gRPC streaming RPCs.
5. **Receipt signer** (Ed25519) whose public key is advertised at `/.well-known/icp/jwks.json` and inside the discovery document.

### 3.1 Why a single process, not a service mesh

A typical agentic-commerce stack accumulates layers: an API gateway, a checkout microservice, an order-management service, a tax service, a fulfillment-event broker, an outbound-webhook worker, and a separate database for every component. Each hop adds tail latency, a deployment dependency, and a place for state to drift.

The ICP handler intentionally collapses this. The protocol layer and the execution layer share an address space. There is one commit point, one set of locks, one durable store. Deployment is `docker run`. The cost is operational — a single process is harder to scale horizontally for write-heavy tenants — but for the agent traffic profile (small numbers of high-value transactions per agent per minute), the single-process design is dramatically simpler to reason about and to verify. Tenants who outgrow it run multiple handlers behind a sticky-by-`agent_id` load balancer, sharing PostgreSQL.

### 3.2 Layering

The handler is intentionally shallow. The internal layers map closely to source files:

| Layer | Source | Responsibility |
|---|---|---|
| Wiring | `main.rs`, `lib.rs` | Config load, state build, server boot |
| Transport cross-cutting | `auth.rs`, `middleware.rs` | API key resolution, header defaults, CORS |
| Intent routing | `service.rs` | The only place that knows about the intent catalog |
| Mandate logic | `mandate.rs` | JWS decoding, signature verification, budget bookkeeping |
| Engine adapter | `commerce.rs` | The only direct consumer of `stateset_embedded::Commerce` |
| Receipt cryptography | `signing.rs`, `receipts.rs` | Ed25519 JWS signing and persistent storage |
| Events | `events.rs` | In-process broadcast channel |
| gRPC surface | `grpc.rs` | Tonic services, payloads are JSON bytes by spec |
| Discovery | `discovery.rs` | `/.well-known/icp` document assembly |
| Compatibility | `compat/` | ACP, UCP, MCP HTTP/stdio, A2A path adapters |

No layer above `commerce.rs` knows about the engine's schema. No layer below `service.rs` knows about HTTP.

---

## 4. Identity model

### 4.1 Three identity classes

ICP distinguishes three identity classes that earlier protocols conflate:

- **Tenant** — the merchant operating the handler. Identified by an `Authorization: Bearer <api_key>`. Carries rate limits and billing.
- **Agent** — the autonomous software making the request. Identified by an opaque URI in `ICP-Agent-Id`. The supported namespaces are `did:stateset:agent:<uuid>`, `did:key:<multibase>`, and `https://<issuer>/agents/<id>`.
- **Principal** — the party on whose behalf the agent acts (typically a buyer, sometimes a seller or platform). Principals delegate authority to agents by signing a **mandate**.

Conflating these three is the root of most agent-commerce security incidents in the wild: a leaked tenant key is read as authorization for any agent, and a compromised agent credential is read as authorization for any buyer. ICP separates them at the wire level so that authorization decisions are always a function of all three.

### 4.2 Mandates

A mandate is a compact JWS whose payload delegates authority from a principal to an agent under explicit constraints. The payload structure (ICP §6):

```json
{
  "iss": "did:buyer:alice.example",
  "sub": "did:stateset:agent:0e9a...",
  "iat": 1745251200,
  "nbf": 1745251200,
  "exp": 1745337600,
  "jti": "mandate_01HYN7...",
  "icp": {
    "version": "2026-04-21",
    "scope": ["buy", "return", "track"],
    "budget": {
      "currency": "USD",
      "amount_minor": 50000,
      "per_transaction": 20000,
      "period": "P1D"
    },
    "merchants": ["*"],
    "categories": ["electronics", "books"],
    "jurisdictions": ["US", "CA", "GB"],
    "policies": {
      "require_receipt": true,
      "require_shipping_address_confirmation": false
    },
    "linked_payment_methods": ["pm_visa_4242_tokenref"]
  }
}
```

Before executing any write, the handler verifies, in order:

1. **Signature.** The JWS is verified against the principal DID's advertised key set, resolved through a pluggable `PrincipalResolver`. The reference handler ships `did:key` and `did:web` resolvers; additional methods (`did:stateset:buyer`) are planned.
2. **Validity window.** `nbf ≤ now ≤ exp`.
3. **Scope.** Every intent type in the request is present in `icp.scope`.
4. **Merchant.** If `merchants` is not `["*"]`, the target merchant is listed.
5. **Budget.** Running spend under this mandate plus the intent's maximum authorized amount does not exceed `budget.amount_minor` in the period.
6. **Jurisdiction.** The fulfillment jurisdiction is in `jurisdictions` (or the list is `["*"]`).
7. **Policies.** All declared policies are honored.

Spend is tracked in a windowed ledger (`MandateLedger`) that is **persisted to SQLite** in production. The decision to persist is non-negotiable: a 24-hour budget mandate whose spend is forgotten on restart effectively allows unbounded further spend in the remaining window. The ledger backend is also pluggable for ephemeral tests.

Signature verification is gated by `ICP_VERIFY_MANDATE_SIGNATURES` (default `true`). The flag exists so local development can use `alg:none` test fixtures; production deployments leave it on.

### 4.3 Scopes

The protocol defines eight scopes that gate the intent catalog:

| Scope | Grants |
|---|---|
| `discover` | `intent.search`, `intent.describe` |
| `quote` | `intent.quote`, `intent.negotiate` |
| `buy` | `intent.authorize`, `intent.buy`, `intent.pay` |
| `subscribe` | `intent.subscribe`, `intent.renew`, `intent.pause`, `intent.cancel_subscription` |
| `fulfill` | `intent.confirm_receipt`, `intent.track` |
| `return` | `intent.return`, `intent.refund_request` |
| `pay_peer` | `intent.a2a_pay`, `intent.a2a_quote` |
| `admin` | All of the above plus handler admin operations |

The mapping from scope to intent is enforced in `intent.rs::Intent::scope()`. Read-only intents (`search`, `describe`) require no mandate.

---

## 5. Intent model

Intents are the verbs of ICP. Every write is an intent. The catalog (17 intents in the canonical spec) is:

| Intent | Scope | Purpose |
|---|---|---|
| `intent.search` | `discover` | Search catalog by text, attributes, or structured query |
| `intent.describe` | `discover` | Fetch canonical product detail |
| `intent.quote` | `quote` | Produce a priced, tax-inclusive, shipping-resolved quote |
| `intent.negotiate` | `quote` | Propose a counter-price (merchant opt-in) |
| `intent.authorize` | `buy` | Convert an accepted quote into an authorized transaction (reserves inventory) |
| `intent.buy` | `buy` | Capture payment and create the order |
| `intent.pay` | `buy` / `pay_peer` | Settle an existing transaction or pay a peer |
| `intent.subscribe` | `subscribe` | Create a recurring plan subscription |
| `intent.renew` | `subscribe` | Force renewal of a cycle |
| `intent.pause` | `subscribe` | Pause an active subscription |
| `intent.cancel_subscription` | `subscribe` | Cancel (immediate or end-of-period) |
| `intent.track` | `fulfill` | Retrieve fulfillment status |
| `intent.confirm_receipt` | `fulfill` | Buyer confirmation |
| `intent.return` | `return` | Initiate an RMA |
| `intent.refund_request` | `return` | Request a refund |
| `intent.a2a_quote` | `pay_peer` | Request a quote from a peer agent |
| `intent.a2a_pay` | `pay_peer` | Pay another agent (direct, split, or escrow) |

Extensions (§9) MAY add vendor-specific intents under a reverse-DNS prefix (`intent.ext.com.example.custom`). Handlers reject unknown intents with `error.intent_not_supported`.

### 5.1 Envelope

Every intent shares a single envelope:

```json
{
  "intent": "buy",
  "intent_id": "int_01HYN...",
  "transaction_id": "txn_01HYN...",
  "agent_id": "did:stateset:agent:0e9a...",
  "mandate_jti": "mandate_01HYN...",
  "params": { /* intent-specific */ },
  "context": {
    "locale": "en-US",
    "jurisdiction": "US-CA",
    "currency": "USD",
    "channel": "chat",
    "session_hint": "sess_01HYN..."
  }
}
```

The `transaction_id` groups intents into a single commerce transaction. The handler creates a transaction on the first write in a context and accepts subsequent writes against the same `transaction_id` for as long as the transaction is not in a terminal state.

### 5.2 Lifecycle

```
┌───────────┐    intent.search / describe    ┌─────────┐
│ discovery │ ─────────────────────────────▶ │ catalog │
└───────────┘                                 └─────────┘
       │
       │ intent.quote
       ▼
  ┌─────────┐   intent.negotiate (optional, loop)
  │  draft  │ ◀──────────────────────────┐
  └────┬────┘                            │
       │ intent.authorize                 │
       ▼                                 │
  ┌──────────────┐                       │
  │  authorized  │ ── intent.revise ─────┘
  └──────┬───────┘
         │ intent.buy / intent.pay
         ▼
   ┌──────────┐    intent.track       ┌─────────────┐
   │ captured │ ─────────────────▶    │ fulfillment │
   └────┬─────┘                       └──────┬──────┘
        │                                    │
        │ intent.return / refund             │ intent.confirm_receipt
        ▼                                    ▼
   ┌─────────┐                         ┌───────────┐
   │ reversed│                         │ completed │
   └─────────┘                         └───────────┘
```

State transitions are gated by the service layer. Calling an intent against a transaction whose state does not allow it returns `precondition_failed` (HTTP 412).

### 5.3 Idempotency

Every intent is idempotent under its `(agent_id, intent_id)` tuple. Replaying the same intent returns the same response and the same receipt `jti`. Handlers SHOULD additionally honor `ICP-Idempotency-Key` across intent types from the same agent. The reference handler implements both via the persistent `idempotency` table (`src/idempotency.rs`).

---

## 6. Receipts

Every state-changing response carries a **receipt**: a compact JWS over the canonicalized response body, returned both in the `ICP-Receipt` header and inlined in the response payload as `receipt: { jws: "...", kid: "..." }`. The payload claims:

```json
{
  "iss": "https://merchant.example/icp",
  "aud": "did:stateset:agent:0e9a...",
  "iat": 1745251200,
  "jti": "rcpt_01HYN...",
  "icp": {
    "version": "2026-04-21",
    "intent": "buy",
    "transaction_id": "txn_01HYN...",
    "order_id": "ord_01HYN...",
    "mandate_jti": "mandate_01HYN...",
    "body_digest": "sha256:...",
    "body_canonicalization": "jcs"
  }
}
```

### 6.1 Why JCS over the response body

The receipt signs the SHA-256 hash of the **JCS-canonicalized** (RFC 8785) response body, not the raw bytes. JCS guarantees that any party who parses the response into a JSON value tree and re-serializes via JCS gets the same bytes the handler signed. This matters because:

1. Receipts pass through proxies, CDNs, and client SDKs that re-serialize JSON.
2. Auditors verify receipts months later against archived JSON, not the original wire bytes.
3. Pretty-printed and minified bodies must verify to the same digest.

The receipt therefore provides **tamper evidence** even when the body is re-serialized downstream. A verifier with the handler's JWKS can confirm offline that a given JSON body is exactly what the handler issued.

### 6.2 Signing keys

Keys are Ed25519 by default (advertised as `EdDSA` per RFC 8037). They are published at `/.well-known/icp/jwks.json` and again in the discovery document under `signing_keys`. Production deployments rotate at least every 90 days; a handler MAY publish multiple keys in JWKS to support overlap during rotation.

A receipt store (`src/receipts.rs`) persists every issued receipt by `jti` so that `GET /icp/v1/receipts/:jti` can return both the JWS and the signed claims for late verification.

---

## 7. Discovery

Every handler serves `GET /.well-known/icp` with a capability document advertising the protocol version, supported transports, intent catalog, currencies, jurisdictions, payment methods, signing keys, and compatibility surfaces:

```json
{
  "icp_version": "2026-04-21",
  "handler_id": "icp://merchant.example",
  "service_name": "Merchant Example",
  "transports": {
    "http": "https://merchant.example",
    "grpc": "grpc://merchant.example:50051",
    "sse_events": "https://merchant.example/icp/v1/events:stream",
    "mcp": "https://merchant.example/mcp",
    "a2a": "https://merchant.example/a2a/v1"
  },
  "intents": [ "intent.search", "intent.quote", "intent.buy", ... ],
  "currencies": ["USD", "EUR", "GBP", "USDC", "ssUSD"],
  "jurisdictions": ["US", "CA", "GB", "DE", "FR"],
  "payment_methods": [ ... ],
  "signing_keys": [
    { "kid": "rcpt-2026-04", "alg": "EdDSA", "jwk": { ... } }
  ],
  "compatibility": {
    "acp": { "version": "2025-09-29", "base_url": "..." },
    "ucp": { "version": "2026-01-11", "base_url": "..." },
    "mcp": { "tools_url": "..." },
    "a2a": { "agent_card_url": "..." }
  },
  "conformance": { "tier": "icp-core" }
}
```

Discovery is the single source of truth that lets agents auto-configure. Capability advertisement is gated on real implementation: the discovery generator filters intents through `Intent::is_implemented()` so that an agent never sees an advertised intent that returns `intent_not_supported`.

---

## 8. Compatibility surfaces

A central design claim of ICP is that an existing agent ecosystem can keep talking ACP, UCP, MCP, or A2A while landing on the same handler, the same engine, and the same receipt pipeline as native ICP traffic. The reference handler implements all four:

### 8.1 ACP

| ACP | ICP |
|---|---|
| `POST /checkout_sessions` (items only) | `intent.quote` |
| `POST /checkout_sessions` (items + buyer + address) | `intent.quote` + `intent.authorize` |
| `POST /checkout_sessions/:id/complete` | `intent.buy` on the same `transaction_id` |
| `POST /checkout_sessions/:id/cancel` | `intent.return` (canceled-before-fulfillment) |
| `POST /agentic_commerce/delegate_payment` | `intent.buy` with `PaymentInstrument::DelegatedVault` |

The ACP `checkout_session_id` is stored in `Transaction.external_refs["acp_session_id"]` so a handler serving both surfaces resolves the same transaction by either key. On the ACP path the merchant's bearer key is treated as a **self-mandate** with unbounded scope limited to that merchant — preserving ICP's "no anonymous writes" invariant without breaking ACP's identity model.

### 8.2 UCP

| UCP | ICP |
|---|---|
| `POST /api/checkout-sessions` | `intent.quote` |
| `PUT /api/checkout-sessions/:id` | `intent.quote` (merge) or `intent.authorize` |
| `POST /api/checkout-sessions/:id/complete` | `intent.buy` |
| `POST /api/checkout-sessions/:id/cancel` | `intent.return` |
| UCP `tokenize`/`detokenize` | `com.stateset.icp.ext.tokenization` extension |
| UCP `ap2.merchant_authorization` | ICP mandate (interpreted as a partial mandate) |

`/.well-known/ucp` is emitted alongside `/.well-known/icp` and shares the same capability set.

### 8.3 MCP

The MCP surface (`POST /mcp` for HTTP, `icp-mcp-stdio` for subprocess-spawning clients like Claude Desktop or Cursor) maps each ICP intent to a discoverable MCP tool (`icp_quote`, `icp_buy`, `icp_track`, …). Tool schemas are derived from the intent parameter types in `src/models.rs`, so the MCP catalog cannot drift from the ICP catalog.

### 8.4 A2A

`/a2a/v1/message:send` accepts an A2A message whose body is an ICP intent and routes it through `IcpService::handle_intent`. Agent cards are emitted at `/.well-known/agent.json`. A2A task cancellation maps to `intent.return` or `intent.cancel_subscription`. Agent-to-agent payments use ICP's first-class `intent.a2a_pay` and `intent.a2a_quote`.

### 8.5 x402

A paid HTTP endpoint returning `HTTP 402 Payment Required` can advertise `WWW-Authenticate: ICP …` with a target intent. The agent replies with a mandate-bearing `intent.buy` carrying `PaymentInstrument::Stablecoin`. The x402 server verifies the receipt before serving the resource.

### 8.6 Property comparison

| Property | ACP | UCP | ICP |
|---|---|---|---|
| Discovery | no | `/.well-known/ucp` | `/.well-known/icp` |
| Intent model | no (REST) | partial | yes |
| Agent identity | implicit | partial | required |
| Signed mandates | no | AP2 passthrough | first-class |
| Verifiable receipts | no | partial | on every state change |
| Returns / subscriptions | out of scope | out of scope | in core |
| Stablecoin payments | out of scope | out of scope | in core |
| Peer (A2A) commerce | out of scope | out of scope | in core |
| Embedded engine | no | optional | by design |

---

## 9. Subscriptions and peer commerce

Two intent families distinguish ICP from any prior agent-commerce protocol.

### 9.1 Subscriptions

`intent.subscribe`, `intent.renew`, `intent.pause`, and `intent.cancel_subscription` provide first-class recurring billing. The handler ships:

- Weekly, monthly, and annual cadences.
- Charge-on-subscribe semantics.
- Signed receipts on every state change (creation, renewal, pause, cancel).
- An **automatic billing scheduler** (`src/scheduler.rs`) running on a Tokio interval that invokes renewals at their due time.
- A failure ladder: three consecutive scheduler failures transition a subscription to `past_due`. A successful manual `intent.renew` clears the failure counter and reactivates.

### 9.2 Agent-to-agent (A2A) commerce

Two agents can transact directly with no merchant in the middle:

- `intent.a2a_quote` — agent A asks agent B for a quote, which is persisted as a `PeerQuote` and returned with a signed receipt.
- `intent.a2a_pay` — agent A pays agent B. Pay-against-quote consumes the `PeerQuote`; direct-pay skips the quote step. Mandate scope `pay_peer` is enforced in both directions.

This is the primitive that lets agents cooperate on commerce: a logistics agent quoting a fulfillment fee to a merchant agent, a research agent invoicing a pricing-comparison agent, an integration agent paying for SaaS quota.

---

## 10. Persistence

The handler maintains two distinct persistent stores:

1. **The embedded commerce engine** (`stateset-icommerce`) holds orders, shipments, returns, and catalog state. SQLite by default; PostgreSQL behind the `postgres` Cargo feature.
2. **A protocol-level state database** (`icp-state.db`, accessed via `src/state_db.rs` and `src/state_store.rs`) holds mandates, mandate spend, receipts, transactions, subscriptions, peer quotes, and idempotency records.

These are separate by design. The engine database is the merchant's commercial system of record and may be replaced with an existing PostgreSQL warehouse. The protocol-level state is operational data that must survive handler restarts but is meaningful only to the protocol layer. Keeping them apart lets a tenant migrate the engine without losing receipt history, and lets the handler advertise its own backup and rotation cadence independently of the engine's.

---

## 11. Security

- **TLS 1.2+** is required on production transports.
- **Mandates** bind every write to a principal. With `ICP_REQUIRE_MANDATE=true` (default) the handler rejects unsigned writes outright.
- **Mandate signature verification** is on by default (`ICP_VERIFY_MANDATE_SIGNATURES=true`). The flag exists only to support `alg:none` test fixtures during local development.
- **Receipts** are signed with EdDSA (Ed25519) or ES256, advertised in JWKS, and rotated at least every 90 days.
- **Replay protection** is provided by `intent_id` uniqueness within an agent, by mandate `jti` + `nbf` window, and by the persistent idempotency table.
- **PII minimization.** The handler does not log raw PAN, CVC, or mandate payloads. Emails and addresses are logged with configurable redaction.
- **Egress controls.** Outbound webhook delivery (`src/webhook.rs`) enforces SSRF guards and a declared-destination allow-list.
- **Tenant separation.** API keys identify tenants and scope every storage query. Cross-tenant lookups are not possible via the public API.
- **Three-class identity.** Tenant, agent, and principal are checked independently on every request; compromise of one class does not authorize the others.

---

## 12. Conformance

The protocol defines two tiers so handlers can ship an honest 1.0 without gating on every intent landing.

**ICP-Core** (required for ICP conformance):

`intent.search`, `intent.describe`, `intent.quote`, `intent.authorize`, `intent.buy`, `intent.pay`, `intent.track`, `intent.return`, `intent.refund_request`, `intent.subscribe`, `intent.renew`, `intent.pause`, `intent.cancel_subscription`, `intent.a2a_quote`, `intent.a2a_pay`.

**ICP-Full** (Core plus the richer negotiation surface):

`intent.negotiate`, `intent.confirm_receipt`.

A handler declares its tier in the discovery document under `conformance.tier`, and the advertised tier is *derived from the concrete intent set* — the handler cannot lie about it. Calling an intent outside the declared tier returns `intent_not_supported` with HTTP `501 Not Implemented`.

The repository ships an `icp-conformance` binary — an implementation-independent test suite that drives any ICP handler URL through the spec and reports pass/fail per requirement. It deliberately imports **nothing** from the handler library so that passing its suite is evidence a *different* handler conforms, not that it matches StateSet's internals. `./demo_conformance.sh` runs the suite against the local handler in one command; `npx @stateset/icp-conformance --url … --api-key … --agent-id …` runs it without a Rust toolchain installed.

The reference handler ships **all 17 catalog intents end-to-end** and advertises `tier: "icp-full"` in its discovery document.

---

## 13. Versioning and extensions

ICP uses **date-based, additive versioning**. The current version is `2026-04-21`, sent in `ICP-Version` on every request and response.

- Additive changes (new intents, fields, extensions) increment the date but do not break existing clients.
- Clients request the oldest version they can accept; handlers do not downgrade below `supported_versions[0]`.
- Breaking changes require a new major line (e.g., `v2`); a handler MAY support multiple major lines simultaneously.

Extensions are named, versioned capabilities identified by reverse DNS:

```json
{
  "extensions": [
    {
      "id": "com.stateset.icp.ext.loyalty",
      "version": "1.0",
      "intents": ["intent.ext.com.stateset.loyalty.redeem"],
      "headers": ["ICP-Loyalty-Tier"]
    }
  ]
}
```

Agents do not assume an extension is available without consulting discovery.

---

## 14. Implementation notes

The reference handler is ~14,000 lines of Rust across the `src/` tree (core handler, compatibility paths, conformance binary, MCP stdio binary, persistence layer, webhook outbox, rate limiter). Notable choices:

- **Async runtime:** Tokio. The HTTP server uses axum 0.7; the gRPC server uses tonic 0.12 with proto build via `tonic-build`. In the sibling `stateset-embedded` crate, Tokio is **optional** — a consumer can build with `default-features = false, features = ["sqlite"]` and pull in zero async-runtime crates, making the engine genuinely usable in CLI tools, WASM targets, and sync Rust services.
- **Crypto:** `ed25519-dalek` for signing and verification. Mandate JWS handling lives in `mandate.rs`; receipt JWS handling in `signing.rs`.
- **Canonicalization:** `serde_jcs` (RFC 8785) for both mandate verification and receipt body digesting.
- **Persistence:** `rusqlite` with the `bundled` feature so the handler ships without a system SQLite dependency. An `r2d2` pool in front. A shared `state_db` pool backs mandates, receipts, transactions, subscriptions, peer quotes, idempotency records, webhook deliveries, and per-tenant webhook subscribers — one SQLite file, one schema, one migration entrypoint. PostgreSQL is opt-in via Cargo feature.
- **Idempotency:** `ICP-Idempotency-Key` (spec §13) is honored end-to-end. Request equivalence is computed via JCS canonicalization + SHA-256, so a retry that reorders JSON object keys still matches. Only successful (2xx) responses are cached — transient errors never poison the cache. Cache survives handler restart.
- **Outbound webhooks:** HMAC-SHA256 signed (`ICP-Signature: t=<unix>,v1=<hex>`), outbox pattern with durable rows in SQLite so delivery is at-least-once across crashes. Exponential backoff, 5-attempt dead-letter, operator retry endpoint. Per-tenant subscribers fan out events to tenant-specific destinations; cross-tenant leakage is impossible by construction.
- **Rate limiting:** per-tenant (fixed-window counter keyed by `ApiKeyInfo.tenant_id`) **and** per-IP pre-auth (keyed by `X-Forwarded-For` first segment → `X-Real-IP` → `direct` sentinel). Pre-auth limiter fires *before* tenant resolution so fake-bearer floods get capped at the IP layer. Both stamp `X-RateLimit-Limit / Remaining / Reset` headers; denials include `Retry-After`.
- **Subscription dunning:** `ICP_SUBSCRIPTION_DUNNING_SCHEDULE_HOURS` (default `1,6,24`) governs the backoff between failed renewal attempts. Transient card declines no longer past_due customers in seconds.
- **OpenAPI:** `utoipa` derives the OpenAPI 3.1 schema from the same Rust types the handler executes against, so the machine-readable contract cannot drift from the code. `/openapi.json` is served standalone; `/docs` renders Swagger UI via CDN (no multi-MB UI assets compiled into the binary).
- **Observability:** `tracing` + `tracing-subscriber` for structured logs; `prometheus` for `/metrics`.
- **Build:** `cargo build --release`; `Dockerfile` and `docker-compose.yml` ship for one-command deployment. Release profile uses `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"`.
- **CI:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, Docker build on every push and PR.
- **Tests:** 226 integration tests on the Rust side covering every intent, every compat surface, durable state across restart, mandate signatures (`did:key` + `did:web`), subscription lifecycle + scheduler-driven auto-billing, dunning, webhooks end-to-end (with HMAC verification), rate limits, idempotency replays, golden-vector regression, and the MCP stdio subprocess.

A second binary `icp-mcp-stdio` exposes the MCP surface as a subprocess-spawnable server suitable for Claude Desktop, Cursor, and other MCP-native clients.

---

## 15. Polyglot substrate

The whole point of the spec + OpenAPI + golden-vectors discipline is that the ecosystem doesn't end at the Rust binary. A non-Rust developer can implement, operate, and verify an ICP deployment end-to-end:

- **`stateset-icp`** (PyPI) — a synchronous Python client handwritten from `/openapi.json` and `ICP_SPEC.md`. No code is imported or generated from the Rust source. Covers every ICP-Full intent (ergonomic `quote` / `authorize` / `buy` / `subscribe` / `a2a_pay` / `negotiate` / `confirm_receipt` / … wrappers), every read endpoint, the SSE event stream (context-managed `EventStream`), and inline mandate signing (`did:key` construction + compact-JWS signing with only `cryptography` as a crypto dep). 34 tests covering cross-language interop and wire contracts, all runnable offline via `httpx.MockTransport`.
- **`@stateset/icp-conformance`** (npm) — a ~150-LOC pure-Node launcher around the Rust `icp-conformance` binary. Resolves the binary in priority order (env override → platform cache → `PATH` → `cargo run` fallback), passes argv through, propagates exit code. Lets non-Rust developers validate any handler against the spec with `npx @stateset/icp-conformance --url … --api-key …` and no cargo installed. Zero runtime deps.
- **`@stateset/create-icp-commerce`** (npm) — one-command merchant scaffolder. `npx @stateset/create-icp-commerce my-store` generates a runnable Rust project (Cargo.toml + 20-line `main.rs` that calls `build_app_state` + `serve`, plus a preconfigured `.env`, `.gitignore`, and README walking through `cargo run` → `curl` smoke test → Python buy flow → `npx @stateset/icp-conformance` validation). 60 seconds from zero to a live merchant.
- **Golden wire-format vectors** (`docs/specification/vectors/`) — byte-exact fixtures for the operations every implementation must agree on: Ed25519 public key → `did:key` multibase encoding, compact-JWS mandate signing (JCS-canonicalized header + payload, Ed25519 over the signing-input bytes). Pinned 32-byte seeds use RFC 8032 test vectors so the inputs are publicly cross-checkable. Rust regression tests and Python interop tests both load the same JSON fixtures and assert byte-identical output. **Proven end-to-end:** a mandate signed by the Python client decodes and verifies under the Rust handler; identical JWS strings come out of both languages given identical inputs.

The Python and npm packages each carry their own package-scoped CHANGELOG so PyPI / npm readers see focused release notes rather than the handler's full changelog.

---

## 16. Roadmap

Phase 0-3 of the 1.0 readiness plan shipped in `v0.2.0`. Remaining work for a true 1.0:

- Additional DID resolver methods (`did:stateset:buyer`, formal `did:web` TTL-cache documentation).
- Full engine routing for tax, promotions, and shipping (stub seams exist today — real providers pluggable via `ShippingCalculator`, `TaxEngine`, `PromotionEngine` traits in a follow-up release).
- Go and Ruby bindings (Python + npm shipped; Go is the next natural target given agent frameworks in that language).
- Distributed state: the in-memory rate-limit counter and the per-instance SQLite state pool both cap multi-handler scale-out. A Redis- or Postgres-backed `StateBackend` trait implementation removes that cap.
- Multi-handler horizontal scale-out behind a sticky-by-`agent_id` load balancer with shared state.
- GitHub Releases publishing pre-built `icp-conformance` binaries so `npx @stateset/icp-conformance` can download on first run rather than requiring a monorepo checkout.
- ACP / UCP wire-format golden vectors to complement the current ICP-native ones.

---

## 17. Conclusion

The StateSet ICP Handler is a deliberately small, deliberately opinionated reference for a protocol whose goal is also small: give agents a single, stable, verifiable wire contract for commerce. The handler runs as one process. Every write is bound to an identified agent acting under a signed mandate. Every state change emits a signed receipt that any party can verify offline. State survives restart. Existing ACP, UCP, MCP, and A2A traffic lands on the same engine through compatibility paths. Subscriptions, peer-to-peer payments, negotiation, and receipt confirmation are first-class. Stablecoins, multi-jurisdiction tax, and cross-border fulfillment are not extensions.

The bet behind ICP is that as agents take over an increasing fraction of commerce traffic, the protocol layer becomes the load-bearing surface — and merchants and platforms will prefer one protocol that subsumes the rest to a permanent matrix of bilateral integrations. The bet behind this specific release is that a spec is only as real as the number of independent implementations that can interoperate with it. `v0.3.0` carries that substrate forward with production hardening around tenant isolation, durable background workers, receipt boundaries, webhook operations, and release-grade configuration while preserving the polyglot ecosystem shipped in `v0.2.0`. A Python developer with this release can implement, operate, and verify a StateSet merchant end-to-end without ever touching Rust. That is the substrate claim.

---

## References

- ICP specification — `docs/specification/ICP_SPEC.md` (this repository)
- ICP error catalog — `docs/specification/errors.md`
- ICP wire-format golden vectors — `docs/specification/vectors/`
- OpenAPI 3.1 schema — `GET /openapi.json` on any running handler
- Architecture notes — `docs/architecture.md`
- Interoperability mapping — `docs/interop.md`
- Getting started — `docs/getting-started.md`
- Agent guide — `docs/agent-guide.md`
- Claude Desktop integration — `docs/claude-desktop.md`
- Python client (`stateset-icp`) — `clients/python/README.md`
- npm conformance wrapper (`@stateset/icp-conformance`) — `clients/npm/icp-conformance/README.md`
- npm merchant scaffolder (`@stateset/create-icp-commerce`) — `clients/npm/create-icp-commerce/README.md`
- StateSet iCommerce — https://github.com/stateset/stateset-icommerce
- RFC 2119 — Key words for use in RFCs
- RFC 3339 — Date and time on the Internet
- RFC 8037 — CFRG ECDH and signatures in JOSE (Ed25519 / EdDSA)
- RFC 8785 — JSON Canonicalization Scheme (JCS)
- ACP — OpenAI Agentic Commerce Protocol, version `2025-09-29`
- UCP — StateSet Universal Commerce Protocol, version `2026-01-11`
- MCP — Anthropic Model Context Protocol
- A2A — Google Agent-to-Agent Protocol
- AP2 — Agent Payment Authorization
- x402 — Agent payment payloads over HTTP 402
