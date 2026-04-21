# Getting Started

A 10-minute tour of ICP from an agent integrator's perspective.

## Prerequisites

- Rust 1.85+ (the sibling iCommerce workspace pins this).
- A checkout of `stateset-icommerce` at `../stateset-icommerce/` relative to
  this repo (the workspace's path dependency expects this).
- `curl` + `jq` + `openssl` (for mandate signing examples).

## Boot the handler

```bash
ICP_ENABLE_DEMO_KEYS=true \
ICP_REQUIRE_MANDATE=false \
cargo run --release
```

For first-time exploration we disable mandate enforcement so you can walk the
flow with just a bearer key. Re-enable `ICP_REQUIRE_MANDATE=true` once you're
ready to work with real mandates.

## Discover

```bash
curl -s http://localhost:8082/.well-known/icp | jq '{
  icp_version,
  intents,
  currencies,
  compatibility,
  signing_keys: (.signing_keys | length)
}'
```

Expected response (trimmed):

```json
{
  "icp_version": "2026-04-21",
  "intents": ["intent.search", "intent.describe", "intent.quote",
              "intent.authorize", "intent.buy", "..."],
  "currencies": ["USD", "EUR", "GBP", "USDC", "ssUSD"],
  "compatibility": {
    "acp": { "version": "2025-09-29", "base_url": "http://127.0.0.1:8082" },
    "ucp": { "version": "2026-01-11", "base_url": "http://127.0.0.1:8082/ucp" }
  },
  "signing_keys": 1
}
```

The signing key is the public half of the Ed25519 key used to sign
receipts. Retrieve the full JWKS:

```bash
curl -s http://localhost:8082/.well-known/icp/jwks.json | jq
```

## Quote

Every ICP request carries an **agent identifier** and (for writes)
a **mandate**. The demo handler ships with `ICP_REQUIRE_MANDATE=false` in
this walkthrough so you can skip mandate construction.

```bash
curl -s -X POST http://localhost:8082/icp/v1/intents \
  -H "Authorization: Bearer icp_demo_key_123" \
  -H "ICP-Agent-Id: did:stateset:agent:demo-alice" \
  -H "Content-Type: application/json" \
  -d '{
    "intent": "intent.quote",
    "agent_id": "did:stateset:agent:demo-alice",
    "params": {
      "items": [
        { "sku": "WIDGET-001", "quantity": 2,
          "unit_price_hint": { "amount_minor": 2999, "currency": "USD" } }
      ],
      "buyer": { "first_name": "Alice", "last_name": "Smith",
                 "email": "alice@example.com" },
      "ship_to": { "name": "Alice Smith", "line_one": "1 Market St",
                   "city": "San Francisco", "state": "CA",
                   "postal_code": "94105", "country": "US" }
    },
    "context": { "currency": "USD", "jurisdiction": "US-CA" }
  }' | jq
```

Take note of `transaction.id` in the response — you'll reuse it in the
next two steps.

## Authorize

```bash
TXN_ID="<paste from previous response>"
curl -s -X POST http://localhost:8082/icp/v1/intents \
  -H "Authorization: Bearer icp_demo_key_123" \
  -H "ICP-Agent-Id: did:stateset:agent:demo-alice" \
  -H "Content-Type: application/json" \
  -d "{
    \"intent\": \"intent.authorize\",
    \"agent_id\": \"did:stateset:agent:demo-alice\",
    \"params\": { \"transaction_id\": \"${TXN_ID}\" }
  }" | jq
```

Transaction state advances to `authorized`. The response now carries a
**receipt**: `receipt.jws` is the compact Ed25519 JWS, and the same value
appears in the `ICP-Receipt` response header.

## Buy

```bash
curl -s -X POST http://localhost:8082/icp/v1/intents \
  -H "Authorization: Bearer icp_demo_key_123" \
  -H "ICP-Agent-Id: did:stateset:agent:demo-alice" \
  -H "Content-Type: application/json" \
  -d "{
    \"intent\": \"intent.buy\",
    \"agent_id\": \"did:stateset:agent:demo-alice\",
    \"params\": {
      \"transaction_id\": \"${TXN_ID}\",
      \"payment\": {
        \"method\": \"card\",
        \"token\": \"tok_demo\",
        \"last_digits\": \"4242\",
        \"brand\": \"visa\"
      }
    }
  }" | jq
```

The response includes an `order` object — that order is now persisted in
the embedded iCommerce SQLite database at `./commerce.db`:

```bash
sqlite3 commerce.db "SELECT id, order_number, total_amount FROM orders;"
```

## Verify the receipt

Receipts are self-contained JWS tokens that any holder of the handler's
JWKS can verify offline:

```bash
echo "$ICP_RECEIPT_JWS" | cut -d. -f1 | base64 -d | jq
echo "$ICP_RECEIPT_JWS" | cut -d. -f2 | base64 -d | jq
```

The claims include the `body_digest` — a SHA-256 hash of the
JCS-canonicalized response body — so tampering with the order payload
downstream invalidates the receipt.

## Stream events

In another terminal:

```bash
curl -N \
  -H "Authorization: Bearer icp_demo_key_123" \
  http://localhost:8082/icp/v1/events:stream
```

Re-run the intent flow — you'll see `transaction.quoted`,
`transaction.authorized`, `transaction.completed` events arrive in real time.

## Next steps

- Read the full spec: [`docs/specification/ICP_SPEC.md`](./specification/ICP_SPEC.md).
- Write your first mandate: [`docs/specification/ICP_SPEC.md#6-mandates`](./specification/ICP_SPEC.md#6-mandates).
- Integrate with existing ACP/UCP handlers:
  [`docs/interop.md`](./interop.md).
- Build an agent that operates under a mandate with a $500/day budget:
  [`docs/agent-guide.md`](./agent-guide.md).
