# Intelligent Commerce Protocol (ICP)

**Version:** `2026-04-21`
**Status:** Draft 0.1 — StateSet reference specification
**Editors:** StateSet Commerce Engineering

---

## Abstract

The **Intelligent Commerce Protocol (ICP)** defines a wire-level contract between
**commerce agents** (autonomous AI systems acting on behalf of a buyer, seller, or
platform) and a **commerce handler** (a server that terminates the protocol and
executes commerce operations against a commerce engine of record).

ICP is **agent-first**, **intent-oriented**, and **verifiable by default**. It is
designed so that an agent with a single mandate can discover merchants, obtain
quotes, negotiate terms, authorize payment, complete checkout, track
fulfillment, and handle post-purchase events (returns, refunds, reorders,
subscription renewals) across jurisdictions and currencies — without the agent
needing to reason about merchant-specific APIs, payment processor primitives,
tax regimes, or fulfillment rails.

ICP is the **protocol layer**. Its reference implementation, this handler,
embeds the [StateSet iCommerce][icommerce] engine as the executable backend.
Merchants and platforms adopt ICP by running a handler in front of (or in place
of) their existing commerce stack.

ICP is designed to **subsume and interoperate with**:

- **OpenAI Agentic Commerce Protocol (ACP)** — ChatGPT Instant Checkout
- **Universal Commerce Protocol (UCP)** — StateSet's platform-neutral checkout
  interop spec
- **Google A2A** — agent-to-agent task protocol
- **AP2** — agent payment authorization mandates
- **MCP** — Model Context Protocol tool surface
- **x402** — agent payment payloads over HTTP 402

An ICP handler MAY expose ACP, UCP, A2A, and MCP endpoints as **compatibility
surfaces**; ICP is the native and most expressive surface.

[icommerce]: https://github.com/stateset/stateset-icommerce

---

## 1. Design principles

1. **Agent-first.** Every request is made by an identified agent acting under a
   verifiable mandate. There are no anonymous principals.
2. **Intent over CRUD.** Agents declare *what they want* (`intent.buy`,
   `intent.return`, `intent.subscribe`). The handler realizes the intent against
   policy, inventory, pricing, and fulfillment constraints. Low-level CRUD
   endpoints exist only for platform/admin surfaces.
3. **Deterministic execution.** Given the same inputs and the same engine state,
   two handlers MUST produce equivalent results. Pricing, tax, discount, and
   inventory reservation are side-effect-pure at the API boundary.
4. **Verifiable by default.** Every state-changing response is accompanied by a
   **signed receipt** over a canonicalized payload. Clients MAY verify receipts
   without calling back into the handler.
5. **Global from day one.** Multi-currency (fiat and stablecoin), multi-jurisdiction
   tax, cross-border fulfillment, and locale-aware messaging are first-class —
   not extensions.
6. **Embedded engine.** The reference handler carries the full commerce engine
   in-process. No external database, no remote control plane, no network hop
   between protocol and execution.
7. **Safe autonomy.** Mandates carry explicit budget, scope, and temporal
   bounds. Writes without a mandate that authorizes them MUST be rejected, even
   if the agent's API key would otherwise allow them.
8. **Small core, typed extensions.** The core is ~30 intents and ~15 resources.
   Everything else is a typed extension advertised in discovery.

---

## 2. Terminology

- **Agent** — an autonomous software system making requests. Every agent has a
  stable **agent identifier** (`agent_id`) and a cryptographic key pair used to
  sign mandates and, optionally, requests.
- **Principal** — the party on whose behalf the agent acts. Typically a buyer
  (`did:buyer:...`), seller (`did:seller:...`), or platform
  (`did:platform:...`).
- **Mandate** — a signed authorization from a principal to an agent, bounding
  what the agent MAY do (scope), up to how much (budget), and until when
  (validity window). An agent MAY hold multiple mandates.
- **Intent** — a typed declaration of desired commerce outcome
  (`intent.buy`, `intent.quote`, etc.).
- **Transaction** — the server-side persistent aggregate that results from
  accepting one or more intents from the same agent in the same context. A
  transaction has a lifecycle (`draft → quoted → authorized → fulfilled →
  settled`).
- **Receipt** — a signed document that records the outcome of an intent.
- **Handler** — an ICP server (this reference implementation, or any
  conforming server).
- **Engine** — the commerce backend of record. The reference handler embeds
  `stateset-icommerce`.
- **Jurisdiction** — a tax/regulatory locale identified by
  ISO-3166 country + sub-division + tax regime (e.g. `US-CA/sales`,
  `DE/VAT`).

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
are to be interpreted as described in [RFC 2119][rfc2119].

[rfc2119]: https://www.rfc-editor.org/rfc/rfc2119

---

## 3. Wire format

### 3.1 Transports

A conforming handler MUST implement **HTTPS/JSON** as its base transport. It
MAY additionally expose:

- **gRPC** (`icp_handler.v1.IcpHandler`) — binary/proto encoding, same semantics
- **Server-Sent Events** — for subscription to transaction, order, shipment,
  and a2a events
- **MCP (stdio or HTTP)** — tool surface for MCP-native agent runtimes
- **A2A (`/a2a/v1/*`)** — Google A2A agent card, message send, task polling

Each transport MUST map onto the same intent model and MUST emit
structurally-identical receipts.

### 3.2 Encoding

- Bodies are **JSON (UTF-8)**. Proto encoding is permitted for gRPC.
- Timestamps are **RFC 3339** (`2026-04-21T14:02:00Z`).
- Monetary amounts are sent in two equivalent but distinct forms:
  - **`amount_minor`**: integer, minor units (cents for USD, satoshis for BTC,
    6-decimal base units for USDC). REQUIRED in every monetary field.
  - **`amount_display`**: decimal string for human presentation (optional).
- Currency codes: ISO 4217 for fiat; extended codes for stablecoins
  (`USDC`, `USDT`, `DAI`, `ssUSD`) and cryptocurrencies (`BTC`, `ETH`, `SOL`).
- JSON member names are `snake_case`.
- Unknown members MUST be ignored by clients and handlers (forward
  compatibility).

### 3.3 Canonicalization

For signing, payloads are canonicalized using **JCS (RFC 8785)**. Clients and
handlers SHOULD sign the exact bytes they transmitted; receivers that need to
verify after parsing MUST re-canonicalize via JCS.

---

## 4. Headers

Every ICP request/response carries a standardized envelope of headers.

| Header | Direction | Required | Description |
|---|---|---|---|
| `ICP-Version` | req/res | MUST | Protocol version the caller understands (date-based, e.g. `2026-04-21`). |
| `ICP-Agent-Id` | req | MUST | Stable identifier of the calling agent. |
| `ICP-Agent-Key-Id` | req | MAY | Key id of the agent key used for request signing. |
| `ICP-Mandate` | req | MUST on writes | Compact JWS of the mandate authorizing this request. |
| `ICP-Request-Id` | req/res | SHOULD | Client-generated UUID; echoed in response and receipt. |
| `ICP-Idempotency-Key` | req | SHOULD on writes | Per-principal idempotency key. |
| `ICP-Trace-Id` | req/res | MAY | W3C trace identifier. |
| `ICP-Receipt` | res | MUST on state change | Compact JWS receipt of the response body. |
| `ICP-Receipt-Kid` | res | MUST on state change | Key id of the handler signing key. |
| `Authorization` | req | MUST | `Bearer <api_key>` — merchant-issued credential identifying the tenant. |

Unknown `ICP-*` headers MUST be ignored.

---

## 5. Identity model

### 5.1 Agent identifiers

An `agent_id` is an opaque URI. The following namespaces are defined:

- `did:stateset:agent:<uuid>` — StateSet-issued agent DID
- `did:key:<multibase>` — self-issued key DID (RFC draft)
- `https://<issuer>/agents/<id>` — HTTPS-resolvable agent profile

Handlers MUST accept any of the three forms. Handlers MAY require agents to
publish a **profile document** at a resolvable URL advertising:

```json
{
  "agent_id": "did:stateset:agent:0e9a...",
  "name": "Alice Shopping Assistant",
  "operator": "did:platform:openai",
  "public_keys": [
    { "kid": "k1", "alg": "EdDSA", "jwk": { ... } }
  ],
  "capabilities": ["icp.buy", "icp.subscribe", "icp.return"],
  "created": "2026-04-01T00:00:00Z"
}
```

### 5.2 Principals

Principals are DIDs or verifiable e-mail-equivalents
(`mailto:alice@example.com`, `did:web:alice.example`). A principal signs a
**mandate** delegating authority to an agent.

### 5.3 Request signing (optional)

When `ICP-Agent-Key-Id` is present, the request MUST carry a detached JWS over
the canonicalized body in `ICP-Signature` (compact JWS). Handlers MAY reject
unsigned writes above a configurable risk threshold.

---

## 6. Mandates

A **mandate** is a JWS whose payload is a JSON document delegating authority
from a principal to an agent.

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

Signature is computed over the JCS-canonicalized payload by the principal's
key (resolved via the principal DID).

### 6.1 Mandate evaluation

Before executing any write, the handler MUST verify:

1. **Signature** — resolves under the principal's advertised key set.
2. **Validity window** — `nbf ≤ now ≤ exp`.
3. **Scope** — every intent type in the request is present in `icp.scope`.
4. **Merchant** — if `merchants` is not `["*"]`, the target merchant is listed.
5. **Budget** — the running spend under this mandate plus the intent's
   maximum authorized amount does not exceed `budget.amount_minor` in the
   period.
6. **Jurisdiction** — the fulfillment jurisdiction is in
   `jurisdictions` (or `jurisdictions` is `["*"]`).
7. **Policies** — all declared policies are honored.

A mandate MUST be persisted with a monotonically-accrued `spent_minor` counter
to enforce budget across requests. Handlers SHOULD expose
`GET /icp/v1/mandates/:jti/usage` for agents to inspect remaining budget.

### 6.2 Scopes

The following scopes are defined in core:

| Scope | Grants |
|---|---|
| `discover` | `intent.search`, `intent.describe` (read-only) |
| `quote` | `intent.quote`, `intent.negotiate` |
| `buy` | `intent.authorize`, `intent.buy`, `intent.pay` |
| `subscribe` | `intent.subscribe`, `intent.renew`, `intent.pause`, `intent.cancel_subscription` |
| `fulfill` | `intent.confirm_receipt`, `intent.track` |
| `return` | `intent.return`, `intent.refund_request` |
| `pay_peer` | `intent.a2a_pay` (agent-to-agent payments) |
| `admin` | all of the above **plus** handler admin operations |

---

## 7. Intents

Intents are the **verbs** of ICP. Every write is an intent.

### 7.1 Intent envelope

```json
{
  "intent": "buy",
  "intent_id": "int_01HYN...",
  "transaction_id": "txn_01HYN...",
  "agent_id": "did:stateset:agent:0e9a...",
  "mandate_jti": "mandate_01HYN...",
  "params": { ... intent-specific ... },
  "context": {
    "locale": "en-US",
    "jurisdiction": "US-CA",
    "currency": "USD",
    "channel": "chat",
    "session_hint": "sess_01HYN..."
  }
}
```

`transaction_id` groups intents into a single commerce transaction. A handler
MUST create a transaction on the first write in a context and MUST accept
subsequent writes against the same `transaction_id` for as long as the
transaction is not terminal.

### 7.2 Core intents

| Intent | Scope | Purpose |
|---|---|---|
| `intent.search` | `discover` | Search catalog by text, attributes, or structured query. |
| `intent.describe` | `discover` | Fetch canonical product detail, including variants, availability, and price. |
| `intent.quote` | `quote` | Produce a priced, tax-inclusive, shipping-resolved quote for a basket. |
| `intent.negotiate` | `quote` | Propose a counter-price or alternative terms (merchant opt-in). |
| `intent.authorize` | `buy` | Convert an accepted quote into an authorized transaction (reserves inventory). |
| `intent.buy` | `buy` | Capture payment and create the order. May be issued with an authorized quote or inline. |
| `intent.pay` | `buy` or `pay_peer` | Settle an existing transaction or pay a peer agent. |
| `intent.subscribe` | `subscribe` | Create a recurring plan subscription. |
| `intent.renew` | `subscribe` | Force renewal of a subscription cycle. |
| `intent.pause` | `subscribe` | Pause an active subscription. |
| `intent.cancel_subscription` | `subscribe` | Cancel a subscription (immediate or end-of-period). |
| `intent.track` | `fulfill` | Retrieve fulfillment status and events. |
| `intent.confirm_receipt` | `fulfill` | Buyer confirmation that goods were received. |
| `intent.return` | `return` | Initiate a return/RMA. |
| `intent.refund_request` | `return` | Request a refund against an order. |
| `intent.a2a_pay` | `pay_peer` | Pay another agent (direct, split, or escrow). |
| `intent.a2a_quote` | `pay_peer` | Request a quote from a peer agent. |

Extensions (see §11) MAY add vendor-specific intents under a reverse-DNS prefix
(`intent.ext.com.example.custom`). Handlers MUST reject unknown intents with
`error.intent_not_supported`.

### 7.3 Intent lifecycle

```
┌───────────┐    intent.search / describe    ┌─────────┐
│ discovery │  ─────────────────────────────> │ catalog │
└───────────┘                                  └─────────┘
       │
       │ intent.quote
       ▼
  ┌─────────┐   intent.negotiate (optional, loop)
  │  draft  │ ◄───────────────────────────┐
  └────┬────┘                             │
       │ intent.authorize                  │
       ▼                                  │
  ┌──────────────┐                        │
  │  authorized  │ ── intent.revise ──────┘
  └──────┬───────┘
         │ intent.buy / intent.pay
         ▼
   ┌──────────┐    intent.track       ┌─────────────┐
   │ captured │ ──────────────────>   │ fulfillment │
   └────┬─────┘                       └──────┬──────┘
        │                                    │
        │ intent.return / refund             │ intent.confirm_receipt
        ▼                                    ▼
   ┌─────────┐                         ┌───────────┐
   │ reversed│                         │ completed │
   └─────────┘                         └───────────┘
```

### 7.4 Idempotency

An intent MUST be idempotent under its `(agent_id, intent_id)` tuple. A
handler SHOULD additionally honor `ICP-Idempotency-Key` across intent types
from the same agent.

---

## 8. Resources

ICP resources are the **nouns**. All resource reads are available via:

- `GET /icp/v1/<resource>` — list
- `GET /icp/v1/<resource>/:id` — retrieve

| Resource | Description |
|---|---|
| `/catalog` | Products, variants, availability, pricing. |
| `/transactions` | Persistent transaction aggregates. |
| `/orders` | Orders of record (engine-backed). |
| `/shipments` | Fulfillment records with event stream. |
| `/returns` | RMAs and their states. |
| `/subscriptions` | Recurring subscriptions and billing cycles. |
| `/mandates` | Mandate introspection (usage, remaining budget). |
| `/receipts` | Receipts issued by this handler (keyed by `jti`). |
| `/agents` | Known agents (platform/admin surface). |
| `/events` | Event log, SSE stream. |

All list endpoints accept `cursor`, `limit`, and resource-specific filters.

---

## 9. Receipts

Every state-changing response carries an **ICP receipt** — a compact JWS over
the canonicalized response body, returned in the `ICP-Receipt` header and
also inlined in the response payload as `receipt: { jws: "...", kid: "..." }`.

The receipt payload has the form:

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

`body_digest` is the SHA-256 hash of the JCS-canonicalized response body,
providing tamper evidence even if the body is re-serialized downstream.

Handlers MUST publish their signing keys at
`/.well-known/icp/jwks.json` and rotate on a configurable schedule. Keys are
also advertised in discovery (§10).

---

## 10. Discovery

Every handler MUST serve:

```
GET /.well-known/icp
```

Returning:

```json
{
  "icp_version": "2026-04-21",
  "handler_id": "icp://merchant.example",
  "service_name": "Merchant Example",
  "supported_versions": ["2026-04-21"],
  "transports": {
    "http": "https://merchant.example",
    "grpc": "grpc://merchant.example:50051",
    "sse_events": "https://merchant.example/icp/v1/events:stream",
    "mcp": "https://merchant.example/mcp",
    "a2a": "https://merchant.example/a2a/v1"
  },
  "intents": [
    "intent.search", "intent.describe",
    "intent.quote", "intent.negotiate",
    "intent.authorize", "intent.buy", "intent.pay",
    "intent.subscribe", "intent.renew", "intent.pause", "intent.cancel_subscription",
    "intent.track", "intent.confirm_receipt",
    "intent.return", "intent.refund_request",
    "intent.a2a_pay", "intent.a2a_quote"
  ],
  "currencies": ["USD", "EUR", "GBP", "USDC", "ssUSD"],
  "jurisdictions": ["US", "CA", "GB", "DE", "FR"],
  "payment_methods": [
    { "id": "card", "brands": ["visa", "mastercard", "amex"] },
    { "id": "stablecoin", "assets": ["USDC", "ssUSD"], "chains": ["base", "set"] },
    { "id": "delegated_vault", "spec": "acp.delegated_payment" }
  ],
  "signing_keys": [
    { "kid": "rcpt-2026-04", "alg": "EdDSA", "jwk": { ... } }
  ],
  "profile_url": "https://merchant.example/.well-known/icp/profile.json",
  "compatibility": {
    "acp": { "version": "2025-09-29", "base_url": "https://merchant.example" },
    "ucp": { "version": "2026-01-11", "base_url": "https://merchant.example/ucp" },
    "mcp": { "tools_url": "https://merchant.example/mcp" },
    "a2a": { "agent_card_url": "https://merchant.example/.well-known/agent.json" }
  },
  "extensions": []
}
```

Discovery SHOULD be **cacheable** (`Cache-Control: max-age=300`).

---

## 11. Extensions

An extension is a named, versioned capability identified by reverse DNS. An
extension declares additional intents, resources, or headers. Handlers advertise
supported extensions under `extensions` in discovery:

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

Agents MUST NOT assume an extension is available without consulting discovery.

---

## 12. Errors

Errors use the structure:

```json
{
  "error": {
    "type": "invalid_request",
    "code": "mandate_out_of_scope",
    "message": "Mandate does not include scope 'buy'.",
    "param": "$.headers.ICP-Mandate",
    "intent_id": "int_01HYN...",
    "retriable": false,
    "docs_url": "https://docs.stateset.com/icp/errors/mandate_out_of_scope"
  }
}
```

| `type` | HTTP | Meaning |
|---|---|---|
| `invalid_request` | 400 | Malformed input or failed validation. |
| `authentication_failed` | 401 | API key missing or invalid. |
| `mandate_invalid` | 401 | Mandate signature, window, or binding failed. |
| `mandate_out_of_scope` | 403 | Mandate does not authorize this intent. |
| `mandate_budget_exceeded` | 402 | Intent would exceed mandate budget. |
| `intent_not_supported` | 404 | Unknown intent. |
| `resource_not_found` | 404 | Resource does not exist. |
| `conflict` | 409 | Idempotency or state conflict. |
| `precondition_failed` | 412 | Transaction state does not allow this intent. |
| `rate_limited` | 429 | Handler rate limit exceeded. |
| `processing_error` | 500 | Handler internal failure. |
| `engine_unavailable` | 503 | Commerce engine failed or is degraded. |

A comprehensive code table is published in
[`docs/specification/errors.md`](./errors.md).

---

## 13. Security

- **TLS 1.2+** MUST be used on all production transports.
- **Mandates** bind every write to a principal; handlers MUST NOT accept an
  unsigned write when `ICP_REQUIRE_MANDATE=true` (default in production).
- **API keys** identify the tenant (merchant) and carry rate-limit policy.
- **Receipts** are signed with an **EdDSA** or **ES256** key advertised in
  `/.well-known/icp/jwks.json`. Keys MUST rotate at least every 90 days.
- **Replay protection** is provided by `intent_id` uniqueness within an
  agent and by mandate `jti` + `nbf` window.
- **PII minimization**: handlers SHOULD NOT log raw PAN, CVC, or mandate
  payloads. Emails and addresses are logged with configurable redaction.
- **Egress controls**: outbound webhooks SHOULD enforce SSRF guards and
  declared-destination allow-lists.

---

## 14. Interoperability

An ICP handler SHOULD expose the following compatibility surfaces, all sharing
the same underlying commerce engine state:

| Surface | Path | Purpose |
|---|---|---|
| ACP | `/checkout_sessions`, `/agentic_commerce/delegate_payment` | ChatGPT Instant Checkout. ACP intents map 1:1 to ICP `intent.authorize` + `intent.buy`. |
| UCP | `/api/checkout-sessions`, `/.well-known/ucp` | Platform-neutral checkout interop. |
| MCP | `/mcp` (or stdio) | Agent tool surface for MCP-native runtimes. |
| A2A | `/a2a/v1/*`, `/.well-known/agent.json` | Google A2A compatibility. |
| x402 | `HTTP 402 Payment Required` responses | Agent-mediated paid HTTP. |

The mapping table is normative and is published in
[`docs/interop.md`](../interop.md).

---

## 15. Compliance

### 15.1 Conformance tiers

ICP defines two conformance tiers so handlers can ship an honest 1.0
without waiting for every intent to land, and so clients can declare
which tier they target.

**ICP-Core** (required for a handler to claim ICP conformance):

- `intent.search`, `intent.describe`
- `intent.quote`, `intent.authorize`, `intent.buy`, `intent.pay`
- `intent.track`, `intent.return`, `intent.refund_request`
- `intent.subscribe`, `intent.renew`, `intent.pause`, `intent.cancel_subscription`
- `intent.a2a_quote`, `intent.a2a_pay`

**ICP-Full** (ICP-Core plus the richer negotiation surface):

- `intent.negotiate`
- `intent.confirm_receipt`

A handler MUST declare its tier in the discovery document via
`conformance.tier` (value `"icp-core"` or `"icp-full"`). Calling an
intent outside the declared tier returns `intent_not_supported` with
HTTP `501 Not Implemented`.

### 15.2 Common requirements

A conforming handler MUST:

1. Implement every intent in its declared tier under at least one transport.
2. Serve `/.well-known/icp` with accurate capability advertisement,
   including `conformance.tier`.
3. Verify mandates on every write under the rules in §6.1.
4. Issue signed receipts on every state-changing response.
5. Expose signing keys at `/.well-known/icp/jwks.json`.
6. Return errors using the taxonomy in §12.
7. Honor `intent_id` idempotency.
8. Publish its ICP version in `ICP-Version` on every response.

A handler MAY:

1. Implement any subset of extensions.
2. Expose additional transports.
3. Require request signing for writes.
4. Operate in a degraded mode where the engine is unavailable; it MUST then
   return `engine_unavailable` on state-changing intents and continue to serve
   discovery and receipt verification.

---

## 16. Versioning

ICP uses **date-based, additive versioning**. The version
(`2026-04-21`) is sent in `ICP-Version` on every request and response.

- **Additive changes** (new intents, new fields, new extensions) increment the
  date but do not break existing clients. Clients request the oldest version
  they can accept; handlers MUST NOT downgrade below `supported_versions[0]`.
- **Breaking changes** require a new **major line** (e.g., `v2`). A handler MAY
  support multiple major lines simultaneously.

---

## 17. IANA considerations

This specification reserves the following:

- Media types: `application/icp+json`, `application/icp-receipt+jwt`
- URI schemes: `icp://<handler-id>`, `did:stateset:agent:<uuid>`
- Well-known URIs: `/.well-known/icp`, `/.well-known/icp/jwks.json`,
  `/.well-known/icp/profile.json`

---

## Appendix A — Minimal buy flow

```text
Agent                                Handler
  │                                     │
  │  GET /.well-known/icp               │
  │────────────────────────────────────>│
  │<──── capabilities + keys ───────────│
  │                                     │
  │  POST /icp/v1/intents               │
  │  { intent: quote, params: {...} }   │
  │────────────────────────────────────>│
  │<─── 200 { transaction, receipt } ───│
  │                                     │
  │  POST /icp/v1/intents               │
  │  { intent: authorize, ... }         │
  │────────────────────────────────────>│
  │<─── 200 { transaction.authorized }──│
  │                                     │
  │  POST /icp/v1/intents               │
  │  { intent: buy, payment: {...} }    │
  │────────────────────────────────────>│
  │<─── 201 { order, receipt } ─────────│
  │                                     │
  │  GET /icp/v1/events:stream          │
  │────────────────────────────────────>│
  │<─── event: shipment.shipped ────────│
  │<─── event: order.delivered ─────────│
```

Receipts may be verified offline by any party holding the handler's JWKS.

---

## Appendix B — Relationship to ACP and UCP

ICP is a **strict superset** of the ACP and UCP checkout flows. The following
request in ACP:

```http
POST /checkout_sessions
{ "items": [...], "buyer": {...}, "fulfillment_address": {...} }
```

is equivalent to this ICP intent:

```http
POST /icp/v1/intents
ICP-Mandate: <jws>
{ "intent": "authorize",
  "params": { "items": [...], "buyer": {...}, "ship_to": {...} },
  "context": { "currency": "USD" } }
```

and the ACP `POST /checkout_sessions/:id/complete` maps to
`intent.buy` against the same `transaction_id`.

UCP's `POST /api/checkout-sessions` maps to `intent.quote` + `intent.authorize`
depending on `payment` completeness, and its `/complete` maps to `intent.buy`.

A handler MAY route ACP and UCP HTTP paths into the ICP intent pipeline
internally to guarantee a single source of state and signing.

---

*End of specification.*
