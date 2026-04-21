# ICP Agent Integration Guide

Audience: engineers building autonomous agents that transact under ICP.

---

## The mental model

Your agent holds:

1. A **keypair** (Ed25519 or ES256). The private half stays inside the agent
   runtime; the public half is published in the agent's profile document
   (`GET <agent_profile_url>`).
2. A **mandate** — a compact JWS signed by the *principal* (the buyer, or
   the platform operating the agent on the buyer's behalf). The mandate
   bounds what the agent MAY do: scope, budget, merchants, jurisdictions,
   validity window.
3. A **tenant API key** — a merchant-issued bearer token used to call a
   specific ICP handler. The tenant key identifies *who the agent is
   talking to*, while the mandate identifies *what the agent is allowed
   to do*.

Every ICP write is the intersection of:

```
   tenant_capabilities  ∩  mandate_authorization  ∩  engine_state
```

All three must allow the operation.

---

## Constructing a mandate

A mandate is a JWS. Header:

```json
{ "alg": "EdDSA", "typ": "JWT", "kid": "buyer-k1" }
```

Payload (JCS-canonicalized before signing):

```json
{
  "iss": "did:buyer:alice.example",
  "sub": "did:stateset:agent:0e9a12f8",
  "iat": 1745251200,
  "nbf": 1745251200,
  "exp": 1745337600,
  "jti": "mandate_01HYN7...",
  "icp": {
    "version": "2026-04-21",
    "scope": ["quote", "buy", "track", "return"],
    "budget": {
      "currency": "USD",
      "amount_minor": 50000,
      "per_transaction": 20000,
      "period": "P1D"
    },
    "merchants": ["*"],
    "jurisdictions": ["US", "CA"],
    "policies": { "require_receipt": true }
  }
}
```

The signature is computed over `base64url(header) + "." + base64url(payload)`
and encoded with the standard JWS compact serialization.

The handler accepts any principal DID it can resolve. For testing, a
platform-issued mandate (`iss = "did:platform:your-platform"`) is fine
provided the platform's public keyset is reachable.

---

## Client workflow

### 1. Discover

```text
GET /.well-known/icp
```

Cache the discovery response for up to 5 minutes. From it you learn:

- Supported intents → what your agent can ask for.
- Currencies → pick a `context.currency` the merchant accepts.
- Jurisdictions → pick a `context.jurisdiction`.
- Signing keys → needed if you want to verify receipts on the client.
- Compatibility — tells you whether the same endpoint also speaks ACP,
  UCP, MCP, or A2A (useful for fallback or for agents that speak multiple
  protocols).

### 2. Plan

Before writing, *shape* the transaction with read-only intents:

```text
POST /icp/v1/intents  { "intent": "intent.search", ... }
POST /icp/v1/intents  { "intent": "intent.describe", ... }
POST /icp/v1/intents  { "intent": "intent.quote", ... }
```

The quote response is the authoritative basis for any budget decision — it
returns tax-inclusive totals in the currency you specified.

### 3. Decide

Compare the quoted total against:

- Mandate remaining budget: `GET /icp/v1/mandates/:jti/usage`.
- Per-transaction cap in the mandate.
- Any policy the agent itself enforces.

If the decision is "buy", proceed. If "don't buy", do nothing — no state
has been mutated yet.

### 4. Commit

```text
POST /icp/v1/intents  { "intent": "intent.authorize", ... }
POST /icp/v1/intents  { "intent": "intent.buy", "params": { "payment": {...} } }
```

The `authorize` and `buy` responses each carry a signed receipt. Persist
the receipts; they are your audit trail.

### 5. Follow up

```text
GET /icp/v1/transactions/:id
GET /icp/v1/events:stream                 ← SSE
POST /icp/v1/intents  { "intent": "intent.track" }
POST /icp/v1/intents  { "intent": "intent.return" }   ← if needed
```

---

## Idempotency

Always send `ICP-Idempotency-Key` on writes. If the network drops and you
retry, the handler returns the original response (and receipt) rather than
double-charging.

Additionally, every intent SHOULD carry a client-generated `intent_id`.
The handler treats `(agent_id, intent_id)` as the canonical uniqueness key.

---

## Verifying receipts

For offline verification:

1. Split the JWS at the `.`: `header_b64.payload_b64.signature_b64`.
2. Base64url-decode the header; check `alg == "EdDSA"` and look up the
   `kid` in the handler's JWKS (cached from `/.well-known/icp/jwks.json`).
3. Verify the Ed25519 signature over `header_b64 + "." + payload_b64`
   using the public key for that `kid`.
4. Base64url-decode the payload. Check that:
   - `iss` matches the expected handler URL.
   - `aud` matches your agent's DID.
   - `icp.body_digest` matches `sha256:<hex>` of the JCS-canonicalized
     response body you received.

If any of those fail, treat the response as unauthenticated.

---

## Mandate budget accounting

The handler maintains a ledger keyed by `mandate.jti`. It records the
captured total (in `mandate.budget.currency`) for every `intent.buy` /
`intent.pay` that succeeds under that mandate. The ledger rolls over when
the mandate's `period` elapses.

To inspect remaining headroom:

```text
GET /icp/v1/mandates/<jti>/usage
```

Returns `spent_minor` and `window_start`. Compare to `budget.amount_minor`
to get remaining headroom.

---

## Error handling

All errors return the body shape described in
[`docs/specification/ICP_SPEC.md#12-errors`](./specification/ICP_SPEC.md#12-errors).

Retry-safe errors: `rate_limited`, `engine_unavailable`.
Non-retriable: `mandate_*`, `intent_not_supported`, `resource_not_found`,
`precondition_failed`, `conflict`.

`invalid_request` is non-retriable without fixing the payload.

---

## Recommended agent architecture

```
           ┌──────────────────────────┐
           │   Policy / Planner       │
           │  (LLM or deterministic)  │
           └────────────┬─────────────┘
                        │ plans intents
                        ▼
           ┌──────────────────────────┐
           │   ICP client             │
           │  ▸ HTTPS + auto-retry    │
           │  ▸ receipt verification  │
           │  ▸ mandate cache         │
           │  ▸ idempotency           │
           └────────────┬─────────────┘
                        │
                        ▼
                ICP handler (merchant)
```

Keep the planner and the ICP client separate. The planner produces
*candidate* intents; the client is the only component that actually sends
them, and it is the only component that holds the private key material.
