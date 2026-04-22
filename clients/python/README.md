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

See [`examples/buy_flow.py`](./examples/buy_flow.py) for the full
annotated lifecycle (discovery → quote → authorize → buy → receipt
roundtrip → mandate usage).

## What's covered

| Surface | Status |
|---|---|
| Discovery (`/.well-known/icp`, `/.well-known/icp/jwks.json`) | ✅ |
| `submit_intent` (POST `/icp/v1/intents`) with mandate + idempotency | ✅ |
| Core intent pipeline: `quote`, `authorize`, `buy` | ✅ (ergonomic wrappers) |
| Transaction / subscription / peer quote / receipt retrieval | ✅ |
| Mandate usage lookup | ✅ |
| `did:key` + Ed25519 mandate signing | ✅ |
| Subscriptions (`subscribe`, `renew`, `pause`, `cancel_subscription`) | Use `submit_intent` directly |
| A2A (`a2a_quote`, `a2a_pay`) | Use `submit_intent` directly |
| SSE event stream | Not yet wrapped (use `httpx` directly against `/icp/v1/events:stream`) |
| gRPC transport | Out of scope — use `grpcio` with `proto/icp_handler/v1/*` |

## Versioning

Tracks the ICP spec date it targets. This release supports spec version
`2026-04-21` (ICP-Core tier; see
[`ICP_SPEC §15`](../../docs/specification/ICP_SPEC.md#15-compliance)).

## Contributing

Bug reports and pull requests welcome at
[stateset-icp-handler](https://github.com/stateset/stateset-icp-handler).

## License

MIT OR Apache-2.0.
