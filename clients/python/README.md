# stateset-icp (Python)

Python client for [StateSet ICP](https://github.com/stateset/stateset-icp-handler)
handlers — the reference implementation of the Intelligent Commerce
Protocol.

**This client is handwritten from `/openapi.json` and
[`ICP_SPEC.md`](../../docs/specification/ICP_SPEC.md) alone.** No code is
imported or generated from the Rust handler; if this package works
end-to-end against any conforming handler, that's the substrate test for
"someone else can implement ICP from the spec."

## Install

```bash
pip install stateset-icp
```

Or from source:

```bash
cd clients/python
pip install -e .
```

## Quick start

```python
from stateset_icp import (
    Client,
    Ed25519KeyPair,
    create_mandate_payload,
    sign_mandate,
)

keypair = Ed25519KeyPair.generate()

mandate = create_mandate_payload(
    issuer=keypair.did,
    subject="did:stateset:agent:demo-alice",
    scope=["quote", "authorize", "buy"],
    budget_currency="USD",
    budget_amount_minor=50_000,
    merchants=["*"],
)
mandate_jws = sign_mandate(mandate, keypair)

with Client(
    "http://localhost:8082",
    api_key="icp_demo_key_123",
    agent_id="did:stateset:agent:demo-alice",
) as icp:
    q = icp.quote(
        items=[{"sku": "WIDGET-001", "quantity": 1,
                "unit_price_hint": {"amount_minor": 2999, "currency": "USD"}}],
        mandate_jws=mandate_jws,
    )
    a = icp.authorize(q["transaction"]["id"], mandate_jws=mandate_jws)
    b = icp.buy(q["transaction"]["id"],
                payment={"method": "card", "token": "tok_demo",
                         "last_digits": "4242", "brand": "visa"},
                mandate_jws=mandate_jws)
    print(b["receipt"]["jti"])
```

Examples in [`examples/`](./examples/):

- **[`buy_flow.py`](./examples/buy_flow.py)** — annotated end-to-end
  lifecycle: keypair → mandate → discovery → quote → authorize → buy →
  receipt roundtrip → mandate usage.
- **[`listen.py`](./examples/listen.py)** — live SSE tail that pairs
  with `buy_flow.py`; run them in two terminals and events from one
  show up in the other.
- **[`anthropic_agent.py`](./examples/anthropic_agent.py)** — Claude
  driving the merchant end-to-end via tool use. Registers five ICP
  intents as Anthropic tools (`icp_search`, `icp_quote`, `icp_authorize`,
  `icp_buy`, `icp_track`) and runs the standard tool-use loop until
  `end_turn`. The substrate test for "agent framework as a first-class
  consumer." Install with `pip install -e . anthropic`.

## What's covered

Every intent in the ICP-Full catalog (17/17 — see
[`ICP_SPEC §15.1`](../../docs/specification/ICP_SPEC.md#15-compliance))
has an ergonomic wrapper. `submit_intent` is also public for raw-envelope
use cases the wrappers don't cover.

| Intent | Method | Notes |
|---|---|---|
| `intent.search` | `icp.search(query=..., limit=..., cursor=...)` | Read-only |
| `intent.describe` | `icp.describe(product_id=... \| sku=...)` | Read-only |
| `intent.quote` | `icp.quote(items=..., buyer=..., ship_to=..., currency=...)` | |
| `intent.authorize` | `icp.authorize(txn_id)` | |
| `intent.buy` | `icp.buy(txn_id, payment=...)` | After authorize |
| `intent.pay` | `icp.pay(txn_id, payment=...)` | Direct, skips authorize |
| `intent.track` | `icp.track(txn_id)` | Read-only |
| `intent.return` | `icp.return_(txn_id, items=..., reason=...)` | Trailing `_` — keyword collision |
| `intent.refund_request` | `icp.refund_request(txn_id, amount=...)` | Omit `amount` for full refund |
| `intent.subscribe` | `icp.subscribe(items=..., cadence=..., payment=...)` | |
| `intent.renew` | `icp.renew(sub_id)` | Forces a charge |
| `intent.pause` | `icp.pause(sub_id)` | |
| `intent.cancel_subscription` | `icp.cancel_subscription(sub_id)` | Terminal |
| `intent.a2a_quote` | `icp.a2a_quote(peer_agent_id=..., service=...)` | |
| `intent.a2a_pay` | `icp.a2a_pay(from_wallet=..., peer_quote_id=... \| peer_agent_id+amount)` | Two modes |
| `intent.negotiate` | `icp.negotiate(txn_id, proposed_total=... \| discount_pct=...)` | |
| `intent.confirm_receipt` | `icp.confirm_receipt(txn_id, note=...)` | Escrow-release trigger |

Plus the read-side:

| Endpoint | Method |
|---|---|
| `GET /.well-known/icp` | `icp.discovery()` |
| `GET /.well-known/icp/jwks.json` | `icp.jwks()` |
| `GET /icp/v1/transactions/{id}` | `icp.get_transaction(id)` |
| `GET /icp/v1/subscriptions/{id}` | `icp.get_subscription(id)` |
| `GET /icp/v1/peer_quotes/{id}` | `icp.get_peer_quote(id)` |
| `GET /icp/v1/receipts/{jti}` | `icp.get_receipt(jti)` |
| `GET /icp/v1/mandates/{jti}/usage` | `icp.get_mandate_usage(jti)` |

And signing:

| Capability | Where |
|---|---|
| `did:key` from Ed25519 public key | `did_key_from_public_key(pk)` |
| Mandate construction (spec §6 shape) | `create_mandate_payload(...)` |
| Compact-JWS signing | `sign_mandate(payload, keypair)` |

### Streaming

`GET /icp/v1/events:stream` is wrapped as a context-managed iterator:

```python
with icp.events() as stream:
    for event in stream:
        if event.type == "transaction.completed":
            handle_completed(event.data)
```

`event.data` is auto-parsed JSON when the server emits JSON (which ICP
always does for `transaction.*`, `subscription.*`, and `peer_quote.*`
events); for any other payload `event.raw` holds the original string.
Exit the `with` block to close the underlying HTTP connection —
works correctly even if you `break` out of the loop early.

### Not yet wrapped

| Surface | Workaround |
|---|---|
| gRPC transport | Out of scope — use `grpcio` with `proto/icp_handler/v1/*` |

## Versioning

Tracks the ICP spec date it targets. This release supports spec version
`2026-04-21` (ICP-Full tier; see
[`ICP_SPEC §15.1`](../../docs/specification/ICP_SPEC.md#15-compliance)).

## Contributing

Bug reports and pull requests welcome at
[stateset-icp-handler](https://github.com/stateset/stateset-icp-handler).

## License

MIT OR Apache-2.0.
