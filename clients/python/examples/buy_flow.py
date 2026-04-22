"""End-to-end buy flow against a running ICP handler.

Run locally::

    # Start the handler (in another terminal)
    cd ../../..
    ICP_VERIFY_MANDATE_SIGNATURES=true cargo run --release

    # Install this client in editable mode and run the example
    cd clients/python
    pip install -e .
    python examples/buy_flow.py
"""

from __future__ import annotations

import os

from stateset_icp import (
    Client,
    Ed25519KeyPair,
    create_mandate_payload,
    sign_mandate,
)

ICP_URL = os.environ.get("ICP_URL", "http://localhost:8082")
API_KEY = os.environ.get("ICP_API_KEY", "icp_demo_key_123")
AGENT_ID = os.environ.get("ICP_AGENT_ID", "did:stateset:agent:demo-alice")


def main() -> None:
    # 1. Mint a fresh Ed25519 keypair and turn it into a `did:key`. In
    # production the issuer would be a persistent principal (`did:web`
    # pointing to a key registry), but `did:key` is self-contained and
    # great for demos — the handler can resolve it from the string alone.
    keypair = Ed25519KeyPair.generate()
    print(f"buyer did = {keypair.did}")

    # 2. Build and sign a mandate that authorizes the three intents our
    # buy flow needs. Budget is the *upper bound* on total spend within
    # the window — the handler rejects anything over it.
    mandate_payload = create_mandate_payload(
        issuer=keypair.did,
        subject=AGENT_ID,
        scope=["quote", "authorize", "buy"],
        budget_currency="USD",
        budget_amount_minor=50_000,  # $500 ceiling
        merchants=["*"],
        valid_for_secs=600,
    )
    mandate_jws = sign_mandate(mandate_payload, keypair)
    print(f"mandate jti = {mandate_payload['jti']}")

    with Client(ICP_URL, api_key=API_KEY, agent_id=AGENT_ID) as icp:
        # 3. Discovery — confirms the handler is live and what tier it
        # claims. Both ICP-Core and ICP-Full accept the buy flow below.
        disco = icp.discovery()
        tier = disco.get("conformance", {}).get("tier", "<unknown>")
        print(f"handler: {disco['service_name']} (tier={tier})")

        # 4. Quote — prices a basket without committing to buy.
        quote = icp.quote(
            items=[
                {
                    "sku": "WIDGET-001",
                    "quantity": 2,
                    "unit_price_hint": {"amount_minor": 2999, "currency": "USD"},
                }
            ],
            buyer={"first_name": "Alice", "email": "alice@example.com"},
            ship_to={
                "name": "Alice Smith",
                "line_one": "1 Market St",
                "city": "San Francisco",
                "state": "CA",
                "postal_code": "94105",
                "country": "US",
            },
            currency="USD",
            jurisdiction="US-CA",
            mandate_jws=mandate_jws,
        )
        txn = quote["transaction"]
        print(f"quoted   txn={txn['id']} state={txn['state']} total={txn['totals']['total']}")

        # 5. Authorize — reserves funds on the payment method, if any.
        auth = icp.authorize(txn["id"], mandate_jws=mandate_jws)
        print(f"auth'd   txn={auth['transaction']['id']} state={auth['transaction']['state']}")

        # 6. Buy — captures the payment and emits an order. The response
        # carries a signed receipt (compact JWS) that a counterparty can
        # verify against `/.well-known/icp/jwks.json` without calling us.
        buy = icp.buy(
            txn["id"],
            payment={
                "method": "card",
                "token": "tok_demo",
                "last_digits": "4242",
                "brand": "visa",
            },
            mandate_jws=mandate_jws,
        )
        print(f"bought   order={buy.get('order', {}).get('id')} state={buy['transaction']['state']}")
        print(f"receipt  jti={buy['receipt']['jti']} kid={buy['receipt']['kid']}")

        # 7. Round-trip the receipt by jti — proves the handler persists
        # signed receipts across requests and would survive a restart.
        fetched = icp.get_receipt(buy["receipt"]["jti"])
        assert fetched["jws"] == buy["receipt"]["jws"], "receipt fetched by jti must match"

        # 8. Mandate usage — shows how much of the $500 budget this flow
        # consumed. Client-side budgets can be enforced off this endpoint.
        usage = icp.get_mandate_usage(mandate_payload["jti"])
        print(f"mandate  spent_minor={usage['spent_minor']}")


if __name__ == "__main__":
    main()
